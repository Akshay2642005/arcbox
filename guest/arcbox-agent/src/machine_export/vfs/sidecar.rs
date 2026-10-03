//! The Mac's AppleDouble sidecars, kept out of the machine.
//!
//! The macOS NFS client cannot store extended attributes on an NFSv3
//! server, so it writes them as a `._<name>` sibling in AppleDouble format,
//! and recent macOS stamps `com.apple.provenance` on every file a process
//! of a downloaded app creates, so every file the Mac writes into a machine
//! comes with one. Left on disk they litter it: `ls -a` shows them, and git
//! takes a `._pack-*.idx` for a pack index and fails. Nothing in Linux wants
//! them.
//!
//! So a `._` file the Mac creates where none exists on disk never reaches
//! the filesystem. It lives in [`Sidecars`], answered to lookups as written
//! but never listed — the Mac sees its extended attributes, not files, as
//! on a native volume, and git's pack scan on the Mac never meets a
//! `._pack-*.idx` either — until the Mac removes it, the table evicts it,
//! or the machine stops. A `._` file that does exist on disk — made inside
//! the machine — is a plain file, listed and served from disk; one
//! appearing on disk under a sidecar's name takes its place. The Mac's
//! metadata on a machine's files thus lasts as long as the machine runs: a
//! dropped sidecar reads as "no attribute", and the Mac writes it again on
//! the next `setxattr`.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::time::{SystemTime, UNIX_EPOCH};

use nfs3_server::nfs3_types::nfs3::{
    fattr3, ftype3, nfstime3, sattr3, set_mode3, set_size3, specdata3,
};

use super::super::attr::IdMap;
use super::write::DEFAULT_FILE_MODE;

/// Sidecars kept at most; past it the oldest goes. Each is a few KiB, so
/// the table stays near 16 MiB however much the Mac writes.
const CAPACITY: usize = 4096;

/// Whether `name` is the AppleDouble sibling of another entry.
pub fn is_sidecar_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 2 && bytes.starts_with(b"._")
}

#[derive(Debug)]
struct Sidecar {
    dir: u64,
    name: OsString,
    data: Vec<u8>,
    mode: u32,
    modified: SystemTime,
}

/// The sidecars the Mac has written, by id and by place.
#[derive(Debug, Default)]
pub struct Sidecars {
    by_id: HashMap<u64, Sidecar>,
    by_name: HashMap<u64, HashMap<OsString, u64>>,
    /// Insertion order, for eviction.
    order: VecDeque<u64>,
}

impl Sidecars {
    fn lookup(&self, dir: u64, name: &OsStr) -> Option<u64> {
        self.by_name.get(&dir)?.get(name).copied()
    }

    fn contains(&self, id: u64) -> bool {
        self.by_id.contains_key(&id)
    }

    /// Starts an empty sidecar under `id`, evicting the oldest past
    /// [`CAPACITY`].
    fn insert(&mut self, id: u64, dir: u64, name: &OsStr) {
        self.by_id.insert(
            id,
            Sidecar {
                dir,
                name: name.to_owned(),
                data: Vec::new(),
                mode: DEFAULT_FILE_MODE,
                modified: SystemTime::now(),
            },
        );
        self.by_name
            .entry(dir)
            .or_default()
            .insert(name.to_owned(), id);
        self.order.push_back(id);
        while self.by_id.len() > CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove(oldest);
        }
    }

    fn remove(&mut self, id: u64) -> bool {
        let Some(sidecar) = self.by_id.remove(&id) else {
            return false;
        };
        if let Some(names) = self.by_name.get_mut(&sidecar.dir) {
            names.remove(&sidecar.name);
            if names.is_empty() {
                self.by_name.remove(&sidecar.dir);
            }
        }
        true
    }

    fn remove_named(&mut self, dir: u64, name: &OsStr) -> bool {
        self.lookup(dir, name).is_some_and(|id| self.remove(id))
    }

    fn rename(&mut self, id: u64, to_dir: u64, to_name: &OsStr) {
        let Some(sidecar) = self.by_id.get_mut(&id) else {
            return;
        };
        let (from_dir, from_name) = (sidecar.dir, std::mem::take(&mut sidecar.name));
        sidecar.dir = to_dir;
        to_name.clone_into(&mut sidecar.name);
        if let Some(names) = self.by_name.get_mut(&from_dir) {
            names.remove(&from_name);
        }
        self.by_name
            .entry(to_dir)
            .or_default()
            .insert(to_name.to_owned(), id);
    }

    fn read(&self, id: u64, offset: u64, count: u32) -> Option<(Vec<u8>, bool)> {
        let data = &self.by_id.get(&id)?.data;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(count as usize).min(data.len());
        Some((data[start..end].to_vec(), end >= data.len()))
    }

    /// Writes `bytes` at `offset`, growing the sidecar with zeros to reach it.
    fn write(&mut self, id: u64, offset: u64, bytes: &[u8], map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get_mut(&id)?;
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(bytes.len())?;
        if sidecar.data.len() < end {
            sidecar.data.resize(end, 0);
        }
        sidecar.data[start..end].copy_from_slice(bytes);
        sidecar.modified = SystemTime::now();
        self.attr(id, map)
    }

    /// Applies what a sidecar can take from `attr` — size and mode — and
    /// reports the result.
    fn setattr(&mut self, id: u64, attr: &sattr3, map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get_mut(&id)?;
        if let set_mode3::Some(mode) = attr.mode {
            sidecar.mode = mode & 0o7777;
        }
        if let set_size3::Some(size) = attr.size {
            sidecar
                .data
                .resize(usize::try_from(size).unwrap_or(usize::MAX), 0);
            sidecar.modified = SystemTime::now();
        }
        self.attr(id, map)
    }

    /// The attributes of sidecar `id`: a plain file of the host user's.
    fn attr(&self, id: u64, map: IdMap) -> Option<fattr3> {
        let sidecar = self.by_id.get(&id)?;
        let (guest_uid, guest_gid) = map.guest_owner();
        let size = sidecar.data.len() as u64;
        let elapsed = sidecar
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time = nfstime3 {
            seconds: u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX),
            nseconds: elapsed.subsec_nanos(),
        };
        Some(fattr3 {
            type_: ftype3::NF3REG,
            mode: sidecar.mode,
            nlink: 1,
            uid: map.uid(guest_uid),
            gid: map.gid(guest_gid),
            size,
            used: size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id,
            atime: time,
            mtime: time,
            ctime: time,
        })
    }

    /// Drops every sidecar in `dir`, for the directory's removal.
    fn remove_in_dir(&mut self, dir: u64) {
        if let Some(names) = self.by_name.remove(&dir) {
            for id in names.into_values() {
                self.by_id.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dot_underscore_prefix_with_a_name_is_a_sidecar() {
        assert!(is_sidecar_name(OsStr::new("._pack.idx")));
        assert!(!is_sidecar_name(OsStr::new("._")));
        assert!(!is_sidecar_name(OsStr::new(".hidden")));
        assert!(!is_sidecar_name(OsStr::new("pack.idx")));
    }

    #[test]
    fn the_oldest_sidecar_goes_when_the_table_is_full() {
        let mut sidecars = Sidecars::default();
        for i in 0..=CAPACITY as u64 {
            sidecars.insert(i + 10, 1, OsStr::new(&format!("._{i}")));
        }
        assert_eq!(sidecars.by_id.len(), CAPACITY);
        assert!(!sidecars.contains(10), "the first one inserted was evicted");
        assert_eq!(sidecars.lookup(1, OsStr::new("._0")), None);
        assert!(sidecars.contains(10 + CAPACITY as u64));
    }
}
