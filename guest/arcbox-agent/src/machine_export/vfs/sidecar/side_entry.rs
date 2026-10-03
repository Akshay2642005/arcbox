//! The side entry: where a value the filesystem will not hold on the
//! inode lives, next to its target.
//!
//! `.arcbox-xattrs/<name>` in the target's own directory is a hidden file
//! of this module's format that records the inode it belongs to. Next to
//! the target rather than in one store under the root because the entry
//! then follows its directory through every rename for free, `rm -rf` of
//! a tree inside the machine takes the entries with it, and a stale entry
//! is recognisable from its own directory alone; the cost is a hidden
//! directory wherever the Mac has put a value that large, which is a
//! resource fork in practice. An entry whose inode is gone — the file was
//! renamed or recreated inside the machine — is dropped when next seen,
//! and one the export cannot read is dropped too: a torn write left it,
//! and the Mac writes the values again when it next sets them. The export
//! root has no directory above it, so nothing can hold its overflow.

use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The hidden directory holding side entries, reserved in every directory.
pub const SIDE_STORE_DIR: &str = ".arcbox-xattrs";

const SIDE_MAGIC: &[u8; 4] = b"ABXA";
const SIDE_VERSION: u16 = 1;

/// The attributes a side entry holds: Mac names and values.
pub type SideAttrs = Vec<(Vec<u8>, Vec<u8>)>;

/// The side entry follows a target renamed from `from` to `to`; whatever
/// entry the name `to` had belonged to the object the rename replaced.
pub fn follow_rename(from: &Path, to: &Path) -> io::Result<()> {
    let (Some(src), Some(dst)) = (of(from), of(to)) else {
        return Ok(());
    };
    if fs::symlink_metadata(&src).is_err() {
        return remove(&dst);
    }
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::rename(&src, &dst)?;
    if let Some(dir) = src.parent() {
        // Only an empty side store goes.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// The side entry of a target the Mac removed.
pub fn drop_for(target: &Path) -> io::Result<()> {
    of(target).map_or(Ok(()), |entry| remove(&entry))
}

/// Removes directory `path` as the Mac sees it: a directory holding only a
/// side store is empty, and the entries in it belong to files already gone.
pub fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) && only_side_store(path)? => {
            fs::remove_dir_all(path.join(SIDE_STORE_DIR))?;
            fs::remove_dir(path)
        }
        result => result,
    }
}

fn only_side_store(dir: &Path) -> io::Result<bool> {
    let mut entries = fs::read_dir(dir)?;
    let Some(first) = entries.next() else {
        return Ok(false);
    };
    Ok(first?.file_name() == SIDE_STORE_DIR && entries.next().is_none())
}

/// What tells one inode's side entry from a successor's under the same
/// name: the inode number, and its birth time where the filesystem has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    ino: u64,
    birth: Option<(i64, u32)>,
}

impl Identity {
    pub fn of(meta: &Metadata) -> Self {
        let birth = meta
            .created()
            .ok()
            .map(|time| match time.duration_since(UNIX_EPOCH) {
                Ok(after) => (after.as_secs().cast_signed(), after.subsec_nanos()),
                Err(before) => (
                    -before.duration().as_secs().cast_signed(),
                    before.duration().subsec_nanos(),
                ),
            });
        Self {
            ino: meta.ino(),
            birth,
        }
    }
}

/// `<dir>/.arcbox-xattrs/<name>` for a target; `None` for the export root.
pub fn of(target: &Path) -> Option<PathBuf> {
    let name = target.file_name()?;
    Some(target.parent()?.join(SIDE_STORE_DIR).join(name))
}

/// A side entry: magic, version, the identity, then `count` attributes as
/// (name length, value length, name, value), little-endian throughout.
pub fn write(entry: &Path, identity: Identity, attrs: &[(&[u8], &[u8])]) -> io::Result<()> {
    let mut out = Vec::new();
    out.extend_from_slice(SIDE_MAGIC);
    out.extend_from_slice(&SIDE_VERSION.to_le_bytes());
    out.extend_from_slice(&identity.ino.to_le_bytes());
    let (secs, nanos) = identity.birth.unwrap_or((i64::MIN, u32::MAX));
    out.extend_from_slice(&secs.to_le_bytes());
    out.extend_from_slice(&nanos.to_le_bytes());
    out.extend_from_slice(&(attrs.len() as u32).to_le_bytes());
    for (name, value) in attrs {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(value);
    }
    if let Some(dir) = entry.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(entry, out)
}

/// The side entry at `entry`, or `None` when there is none. One that does
/// not parse is removed: a torn write left it, and the Mac will write the
/// values again when it next sets them.
pub fn read(entry: &Path) -> io::Result<Option<(Identity, SideAttrs)>> {
    let bytes = match fs::read(entry) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match parse(&bytes) {
        Some(parsed) => Ok(Some(parsed)),
        None => {
            tracing::warn!(entry = %entry.display(), "machine export: removing an unreadable side entry");
            remove(entry)?;
            Ok(None)
        }
    }
}

fn parse(bytes: &[u8]) -> Option<(Identity, SideAttrs)> {
    let mut at = 0usize;
    let mut take = |n: usize| {
        let field = bytes.get(at..at.checked_add(n)?)?;
        at += n;
        Some(field)
    };
    if take(4)? != SIDE_MAGIC || u16::from_le_bytes(take(2)?.try_into().ok()?) != SIDE_VERSION {
        return None;
    }
    let ino = u64::from_le_bytes(take(8)?.try_into().ok()?);
    let secs = i64::from_le_bytes(take(8)?.try_into().ok()?);
    let nanos = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let birth = (secs != i64::MIN || nanos != u32::MAX).then_some((secs, nanos));
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let mut attrs = Vec::new();
    for _ in 0..count {
        let name_len = usize::from(u16::from_le_bytes(take(2)?.try_into().ok()?));
        let value_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let name = take(name_len)?.to_vec();
        let value = take(value_len)?.to_vec();
        attrs.push((name, value));
    }
    Some((Identity { ino, birth }, attrs))
}

/// Removes an entry, and the side store with it when that was the last.
pub fn remove(entry: &Path) -> io::Result<()> {
    match fs::remove_file(entry) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    if let Some(dir) = entry.parent() {
        // Only an empty side store goes.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::appledouble::AppleDouble;
    use super::super::xattrs::{Mode, Store};
    use super::*;

    const STORE: Store = Store::new(64);

    fn attr(name: &str, value: &[u8]) -> super::super::appledouble::Attr {
        super::super::appledouble::Attr {
            name: name.as_bytes().to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn an_entry_sits_beside_its_target_except_at_the_root() {
        assert_eq!(of(Path::new("/")), None, "the root has no side store");
        assert_eq!(
            of(Path::new("/root/x")),
            Some(PathBuf::from("/root/.arcbox-xattrs/x"))
        );
        let identity = Identity {
            ino: 7,
            birth: Some((1_700_000_000, 5)),
        };
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join(SIDE_STORE_DIR).join("x");
        write(&entry, identity, &[(b"a", b"1"), (b"b", &[0u8; 300])]).unwrap();
        let (read_identity, attrs) = read(&entry).unwrap().unwrap();
        assert_eq!(read_identity, identity);
        assert_eq!(
            attrs,
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), vec![0u8; 300])
            ]
        );
        assert_eq!(read(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn side_entries_follow_renames_and_removals() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let x = dir.path().join("x");
        fs::write(&x, b"body").unwrap();
        let content = AppleDouble {
            resource_fork: Some(vec![9u8; 100]),
            attrs: vec![attr("small", b"s")],
            ..AppleDouble::default()
        };
        STORE.store(&x, &content, Mode::Replace).unwrap();

        // Renamed across directories: the inode keeps `small`, the side
        // entry moves along.
        let y = dir.path().join("sub/y");
        fs::rename(&x, &y).unwrap();
        follow_rename(&x, &y).unwrap();
        assert!(!dir.path().join(SIDE_STORE_DIR).exists());
        assert_eq!(STORE.load(&y).unwrap().unwrap(), content);

        // Renamed over: the replaced file's entry is gone.
        let z = dir.path().join("sub/z");
        fs::write(&z, b"z").unwrap();
        STORE.store(&z, &content, Mode::Replace).unwrap();
        fs::write(&x, b"plain").unwrap();
        fs::rename(&x, &z).unwrap();
        follow_rename(&x, &z).unwrap();
        assert_eq!(STORE.load(&z).unwrap(), None);
        assert!(
            dir.path()
                .join("sub")
                .join(SIDE_STORE_DIR)
                .join("y")
                .exists()
        );

        // Removed: the entry goes, and the emptied side store with it.
        fs::remove_file(&y).unwrap();
        drop_for(&y).unwrap();
        assert!(!dir.path().join("sub").join(SIDE_STORE_DIR).exists());

        // A directory left with only its side store counts as empty.
        let sub = dir.path().join("sub");
        fs::create_dir_all(sub.join(SIDE_STORE_DIR)).unwrap();
        fs::write(sub.join(SIDE_STORE_DIR).join("gone"), b"stale").unwrap();
        fs::remove_file(&z).unwrap();
        remove_dir(&sub).unwrap();
        assert!(!sub.exists());
        fs::create_dir_all(dir.path().join(SIDE_STORE_DIR)).unwrap();
        fs::write(dir.path().join("keep"), b"").unwrap();
        assert_eq!(
            remove_dir(dir.path()).unwrap_err().raw_os_error(),
            Some(libc::ENOTEMPTY),
            "a directory with real content stays, side store and all"
        );
        assert!(dir.path().join(SIDE_STORE_DIR).exists());
    }
}
