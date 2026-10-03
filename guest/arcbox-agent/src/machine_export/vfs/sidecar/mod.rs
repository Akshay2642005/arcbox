//! The Mac's AppleDouble sidecars, translated into the target's extended
//! attributes.
//!
//! The macOS NFS client has no attribute channel on NFSv3, so it keeps a
//! file's extended attributes in a sibling `._<name>` in AppleDouble
//! format (`appledouble`), and recent macOS stamps `com.apple.provenance`
//! on every file a downloaded app's process creates: every file the Mac
//! writes into a machine comes with one. Left on disk they litter it —
//! `ls -a` shows them, git takes a `._pack-*.idx` for a pack index — and
//! nothing in Linux reads them.
//!
//! So a sidecar is never a file here. What the Mac writes into `._x` is
//! held as an image (`images`) until it is whole, then parsed and applied
//! to `x` as `user.*` extended attributes, with values too big for the
//! filesystem in a hidden side entry next to `x` (`xattrs`, `side_entry`).
//! What the Mac
//! reads from `._x` is synthesized from those attributes. `._x` exists,
//! to a lookup, exactly while `x` carries attributes or the Mac is
//! writing it, and never appears in a directory listing. `._.` is the
//! directory's own sidecar, as XNU names it. Attributes follow `x` on the
//! inode through every rename, on either side, and die with it; the side
//! entry is moved and removed by the operations the Mac sends. A `._x`
//! that does exist on disk — made inside the machine, or copied there by
//! the Mac as a file that merely has such a name — is a plain file and
//! takes precedence.

mod appledouble;
mod apply;
mod images;
mod side_entry;
#[cfg(test)]
mod tests;
mod xattrs;

use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nfs3_server::nfs3_types::nfs3::{fattr3, ftype3, nfsstat3, specdata3};
use nfs3_server::vfs::FileHandleU64;

pub(super) use self::images::Images;
use self::images::{Facts, Image};
pub(super) use self::side_entry::{SIDE_STORE_DIR, remove_dir};
pub(super) use self::xattrs::Store;
use super::super::attr::nfstime;
use super::write::DEFAULT_FILE_MODE;
use super::{MachineRoot, blocking, on_disk};

/// How often images due to go are settled.
#[cfg(target_os = "linux")]
const RETIREMENT_INTERVAL: Duration = Duration::from_secs(1);
/// How long a dirty image's writes pause before the tick applies it.
pub(super) const FLUSH_IDLE: Duration = Duration::from_secs(1);

/// Whether `name` is the AppleDouble sibling of another entry — `._x` for
/// `x`, `._.` for the directory itself.
pub fn is_sidecar_name(name: &OsStr) -> bool {
    target_of(name).is_some()
}

/// `x` for `._x`, `.` for `._.`; a bare `._` and `._..` are nobody's.
fn target_of(name: &OsStr) -> Option<&OsStr> {
    let rest = name.as_bytes().strip_prefix(b"._")?;
    (!rest.is_empty() && rest != b"..").then(|| OsStr::from_bytes(rest))
}

/// `._x` for `x`.
fn sidecar_name_of(target: &OsStr) -> OsString {
    let mut name = OsString::from("._");
    name.push(target);
    name
}

/// A sidecar the Mac named, resolved: nothing is on disk under its own
/// name, so it stands for its target's attributes.
#[derive(Debug, Clone)]
pub(super) struct Sidecar {
    dir: u64,
    name: OsString,
    /// Where the sidecar itself would be on disk.
    path: PathBuf,
    /// The object whose attributes it carries.
    target: PathBuf,
}

impl Sidecar {
    /// The sidecar's handle, issued if it has none yet.
    fn id(&self, fs: &MachineRoot) -> u64 {
        fs.ids().child(self.dir, &self.name)
    }

    /// The sidecar's handle, if one has been issued.
    fn existing_id(&self, fs: &MachineRoot) -> Option<u64> {
        fs.ids().lookup(self.dir, &self.name)
    }
}

/// The moment an inode's metadata last changed, as a `SystemTime`.
fn changed_at(meta: &Metadata) -> SystemTime {
    let seconds = u64::try_from(meta.ctime()).unwrap_or(0);
    let nanoseconds = u32::try_from(meta.ctime_nsec()).unwrap_or(0);
    UNIX_EPOCH + Duration::new(seconds, nanoseconds)
}

/// `count` bytes of `bytes` from `offset`, and whether that reached the end.
fn slice(bytes: &[u8], offset: u64, count: u32) -> (Vec<u8>, bool) {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = start.saturating_add(count as usize).min(bytes.len());
    (bytes[start..end].to_vec(), end >= bytes.len())
}

/// The read side: resolving a name or handle to a sidecar, and answering
/// lookups, attributes and reads from the image or the target.
impl MachineRoot {
    pub(super) fn images(&self) -> MutexGuard<'_, Images> {
        self.images.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves `name` in `dirid` to a sidecar: a `._` name with nothing on
    /// disk at `path`. `None` for a plain name, and for a `._` file that is
    /// on disk.
    pub(super) async fn sidecar_at(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) -> Result<Option<Sidecar>, nfsstat3> {
        let Some(target) = target_of(name) else {
            return Ok(None);
        };
        if on_disk(path.to_path_buf()).await? {
            return Ok(None);
        }
        let dir_path = self.path_of(dirid)?;
        let target = if target == "." {
            dir_path
        } else {
            dir_path.join(target)
        };
        Ok(Some(Sidecar {
            dir: dirid,
            name: name.to_owned(),
            path: path.to_path_buf(),
            target,
        }))
    }

    /// Resolves handle `id` the same way.
    pub(super) async fn sidecar_of(&self, id: u64) -> Result<Option<Sidecar>, nfsstat3> {
        let (dir, name) = {
            let ids = self.ids();
            match ids.entry(id) {
                Some((dir, name)) if is_sidecar_name(name) => (dir, name.to_owned()),
                _ => return Ok(None),
            }
        };
        let path = self.path_of(id)?;
        self.sidecar_at(dir, &name, &path).await
    }

    /// Resolves `._x` in `dirid`: a file on disk under that name is itself;
    /// otherwise the sidecar exists while the Mac holds an image of it or
    /// `x` carries attributes.
    pub(super) async fn lookup_sidecar(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) -> Result<FileHandleU64, nfsstat3> {
        let Some(sidecar) = self.sidecar_at(dirid, name, path).await? else {
            return Ok(self.handle_for(dirid, name));
        };
        match self.image_facts(&sidecar).await? {
            Some(_) => Ok(FileHandleU64::new(sidecar.id(self))),
            None => Err(nfsstat3::NFS3ERR_NOENT),
        }
    }

    /// What the image of `sidecar` looks like, loading it from the target
    /// when nothing is in memory; `None` when the target has no attributes
    /// or is not there.
    async fn image_facts(&self, sidecar: &Sidecar) -> Result<Option<Facts>, nfsstat3> {
        let now = Instant::now();
        if let Some(id) = sidecar.existing_id(self)
            && let Some(image) = self.images().get(id, now)
        {
            return Ok(Some(image.facts()));
        }
        self.synthesize(sidecar).await
    }

    /// Builds the image of `sidecar` from its target's attributes and keeps
    /// it for the reads that follow.
    async fn synthesize(&self, sidecar: &Sidecar) -> Result<Option<Facts>, nfsstat3> {
        let (target, store) = (sidecar.target.clone(), self.store);
        let loaded = blocking(move || {
            let content = store.load(&target)?;
            let meta = fs::symlink_metadata(&target)?;
            Ok(content.map(|content| (content, meta)))
        })
        .await;
        let (content, meta) = match loaded {
            Ok(Some(loaded)) => loaded,
            Ok(None) | Err(nfsstat3::NFS3ERR_NOENT) => return Ok(None),
            Err(e) => return Err(e),
        };
        let now = Instant::now();
        let image = Image::synthesized(content.to_bytes(), changed_at(&meta), now);
        let facts = image.facts();
        let id = sidecar.id(self);
        let evicted = self.images().insert(id, image, now);
        self.settle_all(evicted).await;
        Ok(Some(facts))
    }

    pub(super) async fn read_sidecar(
        &self,
        sidecar: &Sidecar,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        if self.image_facts(sidecar).await?.is_none() {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        let id = sidecar.id(self);
        let mut images = self.images();
        let image = images
            .get(id, Instant::now())
            .ok_or(nfsstat3::NFS3ERR_STALE)?;
        Ok(slice(&image.bytes, offset, count))
    }

    /// The attributes of a sidecar: a plain file of its target's owner with
    /// the target's read and write bits, as XNU creates one, sized and
    /// dated by the image.
    pub(super) async fn sidecar_attr(&self, sidecar: &Sidecar) -> Result<fattr3, nfsstat3> {
        let facts = self
            .image_facts(sidecar)
            .await?
            .ok_or(nfsstat3::NFS3ERR_STALE)?;
        let target = sidecar.target.clone();
        let owner = match blocking(move || fs::symlink_metadata(&target)).await {
            Ok(meta) => Some(meta),
            // Not there yet: the image is waiting for it.
            Err(nfsstat3::NFS3ERR_NOENT) => None,
            Err(e) => return Err(e),
        };
        let (uid, gid, mode) = match &owner {
            Some(meta) => (meta.uid(), meta.gid(), meta.mode() & 0o666),
            None => {
                let (uid, gid) = self.map.guest_owner();
                (uid, gid, DEFAULT_FILE_MODE)
            }
        };
        let elapsed = facts
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time = nfstime(
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
            i64::from(elapsed.subsec_nanos()),
        );
        Ok(fattr3 {
            type_: ftype3::NF3REG,
            mode: facts.mode.unwrap_or(mode),
            nlink: 1,
            uid: self.map.uid(uid),
            gid: self.map.gid(gid),
            size: facts.len,
            used: facts.len,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: sidecar.id(self),
            atime: time,
            mtime: time,
            ctime: time,
        })
    }

    /// Settles the images due to go, once a second, for as long as the
    /// export lives: `serve` spawns this.
    #[cfg(target_os = "linux")]
    pub fn retire_sidecars_forever(&self) -> impl Future<Output = ()> + Send + 'static {
        let fs = self.clone();
        async move {
            let mut tick = tokio::time::interval(RETIREMENT_INTERVAL);
            loop {
                tick.tick().await;
                fs.retire_sidecars(Instant::now()).await;
            }
        }
    }

    /// Applies the images whose writes have paused, then takes out the
    /// ones due to go at `now` and gives each its last chance.
    pub(super) async fn retire_sidecars(&self, now: Instant) {
        self.flush_idle(now).await;
        let retired = self.images().retire(now);
        self.settle_all(retired).await;
    }
}
