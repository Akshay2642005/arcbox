//! The write side of a sidecar: the Mac's writes into its image, the image
//! applied to the target, and the hooks that keep a target's attributes,
//! side entry and image together through create, remove and rename.
//!
//! An image is applied — parsed and stored as the target's attributes —
//! on `COMMIT`, once its writes have paused for a moment, and when it is
//! retired from memory; only once it parses whole, since the Mac writes a
//! sidecar in pieces, and only once the target exists, since `cp -R` of a
//! volume with real `._` files writes the sidecar first. An image the Mac
//! wrote into a sidecar it *created* is merged into the target's
//! attributes and then dropped, so the next read shows the union: the Mac
//! never read what the target had and writes only what it is adding —
//! after `mv x y` it stamps provenance into a fresh `._x` and renames that
//! over `._y`, which on a FAT volume lands on top of the old sidecar. An
//! image written over a sidecar the Mac looked up and read replaces the
//! target's attributes, which is how `xattr -d` works. Content that is not
//! AppleDouble at all is a file the Mac copied under a `._` name: it goes
//! to disk under that name and is a plain file from then on.

use std::ffi::OsStr;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Instant;

use nfs3_server::nfs3_types::nfs3::{fattr3, nfsstat3, sattr3, set_mode3, set_size3, stable_how};
use nfs3_server::vfs::FileHandleU64;

use super::appledouble::{AppleDouble, ParseError};
use super::images::Image;
use super::side_entry;
use super::xattrs::{Mode, Transfer};
use super::{FLUSH_IDLE, Sidecar, is_sidecar_name, sidecar_name_of};
use crate::machine_export::vfs::write::DEFAULT_FILE_MODE;
use crate::machine_export::vfs::{MachineRoot, blocking};

impl MachineRoot {
    /// Creates sidecar `sidecar` for the Mac to write — unless it exists,
    /// which `exclusive` refuses — and applies `attr` to it. A `CREATE`
    /// means the Mac believes the sidecar is new, so the image starts
    /// empty whatever the target has, and merges into it when applied.
    pub(in crate::machine_export::vfs) async fn create_sidecar(
        &self,
        sidecar: Sidecar,
        attr: &sattr3,
        exclusive: bool,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        if exclusive && self.image_facts(&sidecar).await?.is_some() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let id = sidecar.id(self);
        self.start_image(id).await;
        self.resize_and_chmod(id, attr)?;
        let fattr = self.sidecar_attr(&sidecar).await?;
        Ok((FileHandleU64::new(id), fattr))
    }

    pub(in crate::machine_export::vfs) async fn setattr_sidecar(
        &self,
        sidecar: &Sidecar,
        attr: &sattr3,
    ) -> Result<fattr3, nfsstat3> {
        if self.image_facts(sidecar).await?.is_none() {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        self.resize_and_chmod(sidecar.id(self), attr)?;
        self.sidecar_attr(sidecar).await
    }

    /// Writes into the image of `sidecar`, which is the Mac's open file:
    /// seeded from the target when the Mac rewrites a sidecar it looked
    /// up, empty when there is nothing yet. The image is applied by the
    /// `COMMIT` that follows or by the retirement tick once the writes
    /// pause — not here, since the Mac writes a header stable and the
    /// resource fork behind it unstable, and the whole is what counts.
    pub(in crate::machine_export::vfs) async fn write_sidecar(
        &self,
        sidecar: &Sidecar,
        offset: u64,
        data: &[u8],
        stable: stable_how,
    ) -> Result<(fattr3, stable_how), nfsstat3> {
        let id = sidecar.id(self);
        if self.image_facts(sidecar).await?.is_none() {
            self.start_image(id).await;
        }
        self.images().write(id, offset, data, Instant::now())?;
        Ok((self.sidecar_attr(sidecar).await?, stable))
    }

    pub(in crate::machine_export::vfs) async fn commit_sidecar(
        &self,
        sidecar: &Sidecar,
    ) -> Result<(), nfsstat3> {
        self.flush(sidecar).await
    }

    /// Drops sidecar `sidecar`: its image, and every attribute of its
    /// target. `NFS3ERR_NOENT` when there was neither.
    pub(in crate::machine_export::vfs) async fn remove_sidecar(
        &self,
        sidecar: Sidecar,
    ) -> Result<(), nfsstat3> {
        let had_image = sidecar
            .existing_id(self)
            .is_some_and(|id| self.images().remove(id).is_some());
        let (target, store) = (sidecar.target.clone(), self.store);
        let had_attrs = match blocking(move || store.remove(&target)).await {
            Ok(any) => any,
            Err(nfsstat3::NFS3ERR_NOENT) => false,
            Err(e) => return Err(e),
        };
        self.ids().forget(sidecar.dir, &sidecar.name);
        if had_image || had_attrs {
            Ok(())
        } else {
            Err(nfsstat3::NFS3ERR_NOENT)
        }
    }

    /// Moves sidecar `from` to `to_name` in `to_dirid`. To another `._`
    /// name, the attributes move from one target to the other and the
    /// image follows the name — and lands on the new target at once if it
    /// was waiting for one; to a plain name, `NFS3ERR_XDEV`, so `mv` copies
    /// the bytes out and the Mac gets the file it asked for.
    pub(in crate::machine_export::vfs) async fn rename_sidecar(
        &self,
        from: Sidecar,
        to_dirid: u64,
        to_name: &OsStr,
        to_path: &Path,
    ) -> Result<(), nfsstat3> {
        let Some(to) = self.sidecar_at(to_dirid, to_name, to_path).await? else {
            return Err(if is_sidecar_name(to_name) {
                nfsstat3::NFS3ERR_EXIST
            } else {
                nfsstat3::NFS3ERR_XDEV
            });
        };
        if let Some(replaced) = to.existing_id(self) {
            self.images().remove(replaced);
        }
        let has_image = from
            .existing_id(self)
            .is_some_and(|id| self.images().contains(id));
        let (from_target, to_target, store) = (from.target.clone(), to.target.clone(), self.store);
        let transferred = blocking(move || store.transfer(&from_target, &to_target)).await?;
        // The image keeps its handle under the new name.
        self.ids().rename(from.dir, &from.name, to.dir, &to.name);
        if has_image {
            self.flush(&to).await?;
        }
        match transferred {
            Transfer::Parked(content) if !has_image => {
                let now = Instant::now();
                let id = to.id(self);
                let evicted =
                    self.images()
                        .insert(id, Image::pending(content.to_bytes(), now), now);
                self.settle_all(evicted).await;
                Ok(())
            }
            Transfer::Parked(_) | Transfer::Moved => Ok(()),
            // `mv x y` already carried the attributes on the inode.
            Transfer::Nothing if has_image || self.image_facts(&to).await?.is_some() => Ok(()),
            Transfer::Nothing => Err(nfsstat3::NFS3ERR_NOENT),
        }
    }

    /// After the Mac created `name` in `dirid`: a sidecar it wrote before
    /// the target now has one. A refusal is the attributes' alone and not
    /// the file's, so it is logged, not returned.
    pub(in crate::machine_export::vfs) async fn target_created(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) {
        let Some(sidecar) = self.pending_sidecar_of(dirid, name, path) else {
            return;
        };
        if let Err(e) = self.flush(&sidecar).await {
            tracing::warn!(target = %path.display(), error = ?e, "machine export: a sidecar written ahead of its file could not be applied");
        }
    }

    /// After the Mac removed `name` in `dirid`: the sidecar's image and
    /// side entry go with it; the inode's attributes went with the inode.
    pub(in crate::machine_export::vfs) async fn target_removed(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) {
        self.forget_sidecar(dirid, name);
        let path = path.to_path_buf();
        if let Err(e) = blocking(move || side_entry::drop_for(&path)).await {
            tracing::warn!(error = ?e, "machine export: a removed file's side entry stays behind");
        }
    }

    /// Before the Mac renames `name` in `dirid`: a sidecar it is still
    /// writing is applied, so the attributes travel on the inode.
    pub(in crate::machine_export::vfs) async fn target_renaming(
        &self,
        dirid: u64,
        name: &OsStr,
        path: &Path,
    ) {
        let Some(sidecar) = self.pending_sidecar_of(dirid, name, path) else {
            return;
        };
        if let Err(e) = self.flush(&sidecar).await {
            tracing::warn!(target = %path.display(), error = ?e, "machine export: a sidecar still being written was not applied before the rename");
        }
    }

    /// After the Mac renamed `from` to `to`: the side entry follows; the
    /// images of `from`'s sidecar, of the replaced `to`'s, and of a `._`
    /// file the rename wrote over go — the next read sees the target — and
    /// a sidecar written ahead of `to` now has its target.
    pub(in crate::machine_export::vfs) async fn target_renamed(
        &self,
        (from_dirid, from_name, from_path): (u64, &OsStr, &Path),
        (to_dirid, to_name, to_path): (u64, &OsStr, &Path),
    ) {
        self.forget_sidecar(from_dirid, from_name);
        let replaced = self.ids().lookup(to_dirid, to_name);
        if let Some(replaced) = replaced {
            self.images().remove(replaced);
        }
        if self
            .pending_sidecar_of(to_dirid, to_name, to_path)
            .is_none()
        {
            self.forget_sidecar(to_dirid, to_name);
        }
        let (from, to) = (from_path.to_path_buf(), to_path.to_path_buf());
        if let Err(e) = blocking(move || side_entry::follow_rename(&from, &to)).await {
            tracing::warn!(error = ?e, "machine export: a renamed file's side entry did not follow it");
        }
        self.target_created(to_dirid, to_name, to_path).await;
    }

    /// The sidecar of target `name`, if the Mac has a dirty image of it.
    fn pending_sidecar_of(&self, dirid: u64, name: &OsStr, path: &Path) -> Option<Sidecar> {
        let sidecar_name = sidecar_name_of(name);
        let id = self.ids().lookup(dirid, &sidecar_name)?;
        let dirty = self.images().peek(id).is_some_and(|image| image.dirty);
        dirty.then(|| Sidecar {
            dir: dirid,
            path: path.with_file_name(&sidecar_name),
            name: sidecar_name,
            target: path.to_path_buf(),
        })
    }

    /// Drops the image and the handle of target `name`'s sidecar.
    fn forget_sidecar(&self, dirid: u64, name: &OsStr) {
        let sidecar_name = sidecar_name_of(name);
        let id = self.ids().lookup(dirid, &sidecar_name);
        if let Some(id) = id {
            self.images().remove(id);
            self.ids().forget(dirid, &sidecar_name);
        }
    }

    /// Puts an empty image under `id` for the Mac to fill, in place of
    /// whatever image the handle had.
    async fn start_image(&self, id: u64) {
        let now = Instant::now();
        let evicted = self.images().insert(id, Image::fresh(now), now);
        self.settle_all(evicted).await;
    }

    fn resize_and_chmod(&self, id: u64, attr: &sattr3) -> Result<(), nfsstat3> {
        let now = Instant::now();
        let mut images = self.images();
        if let set_size3::Some(size) = attr.size {
            images.truncate(id, size, now)?;
        }
        if let set_mode3::Some(mode) = attr.mode
            && let Some(image) = images.get_mut(id, now)
        {
            image.mode = Some(mode & 0o7777);
        }
        Ok(())
    }

    /// Applies the image of `sidecar` to its target if it is dirty and
    /// parses whole. A replacing image stays as the Mac's read cache; a
    /// merging one goes, so the next read shows the union. A missing
    /// target leaves the image waiting. A refusal — no room for a value,
    /// an object that takes no attributes — reaches the client.
    async fn flush(&self, sidecar: &Sidecar) -> Result<(), nfsstat3> {
        let Some(id) = sidecar.existing_id(self) else {
            return Ok(());
        };
        let (bytes, generation, mode) = {
            let images = self.images();
            match images.peek(id) {
                Some(image) if image.dirty => (
                    image.bytes.clone(),
                    image.generation,
                    if image.merge {
                        Mode::Merge
                    } else {
                        Mode::Replace
                    },
                ),
                _ => return Ok(()),
            }
        };
        match AppleDouble::parse(&bytes) {
            Ok(content) => {
                let (target, store) = (sidecar.target.clone(), self.store);
                match blocking(move || store.store(&target, &content, mode)).await {
                    Ok(()) => {
                        let mut images = self.images();
                        match mode {
                            Mode::Merge => {
                                images.remove(id);
                            }
                            Mode::Replace => {
                                if let Some(image) = images.peek_mut(id)
                                    && image.generation == generation
                                {
                                    image.dirty = false;
                                }
                            }
                        }
                        Ok(())
                    }
                    Err(nfsstat3::NFS3ERR_NOENT) => {
                        self.tried(id, generation);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            Err(ParseError::Incomplete) => {
                self.tried(id, generation);
                Ok(())
            }
            Err(ParseError::Invalid(why)) => {
                tracing::debug!(path = %sidecar.path.display(), why, "machine export: a `._` file that is not a sidecar goes to disk");
                let Some(image) = self.images().remove(id) else {
                    return Ok(());
                };
                self.materialize(sidecar, image).await
            }
        }
    }

    /// Notes that `generation` of image `id` could not be applied yet, so
    /// the tick leaves it alone until the Mac writes more.
    fn tried(&self, id: u64, generation: u64) {
        if let Some(image) = self.images().peek_mut(id) {
            image.tried = Some(generation);
        }
    }

    /// Puts `image` on disk under the sidecar's own name, as the guest
    /// owner's plain file. The handle stays valid: it now resolves to the
    /// file.
    async fn materialize(&self, sidecar: &Sidecar, image: Image) -> Result<(), nfsstat3> {
        let path = sidecar.path.clone();
        let (uid, gid) = self.map.guest_owner();
        let mode = image.mode.unwrap_or(DEFAULT_FILE_MODE);
        blocking(move || {
            std::fs::write(&path, &image.bytes)?;
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
            std::fs::set_permissions(&path, Permissions::from_mode(mode))
        })
        .await
    }

    /// Applies the dirty images whose writes have paused for [`FLUSH_IDLE`]
    /// and still sit in the table: the retirement tick's work.
    pub(super) async fn flush_idle(&self, now: Instant) {
        let due = self.images().due(now, FLUSH_IDLE);
        for id in due {
            let Ok(Some(sidecar)) = self.sidecar_of(id).await else {
                continue;
            };
            if let Err(e) = self.flush(&sidecar).await {
                tracing::warn!(path = %sidecar.path.display(), error = ?e, "machine export: a sidecar could not be applied");
            }
        }
    }

    /// Retired and evicted images: each dirty one gets its last chance —
    /// a whole AppleDouble goes to its target if that exists, a file that
    /// is no AppleDouble goes to disk, and an unfinished sidecar is dropped.
    pub(super) async fn settle_all(&self, images: Vec<(u64, Image)>) {
        for (id, image) in images {
            if image.dirty {
                self.settle(id, image).await;
            }
        }
    }

    async fn settle(&self, id: u64, image: Image) {
        let sidecar = match self.sidecar_of(id).await {
            Ok(Some(sidecar)) => sidecar,
            // Shadowed by a file on disk, or its handle forgotten: nothing
            // to apply it to.
            _ => return,
        };
        let mode = if image.merge {
            Mode::Merge
        } else {
            Mode::Replace
        };
        match AppleDouble::parse(&image.bytes) {
            Ok(content) => {
                let (target, store) = (sidecar.target.clone(), self.store);
                if let Err(e) = blocking(move || store.store(&target, &content, mode)).await {
                    tracing::debug!(path = %sidecar.path.display(), error = ?e, "machine export: dropping a sidecar whose file never came or refused it");
                }
            }
            Err(ParseError::Incomplete) if AppleDouble::has_magic(&image.bytes) => {
                tracing::debug!(path = %sidecar.path.display(), "machine export: dropping a sidecar the Mac never finished");
            }
            Err(_) => {
                if let Err(e) = self.materialize(&sidecar, image).await {
                    tracing::warn!(path = %sidecar.path.display(), error = ?e, "machine export: a `._` file could not be written to disk");
                }
            }
        }
    }
}
