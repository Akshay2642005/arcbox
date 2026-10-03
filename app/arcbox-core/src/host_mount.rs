//! The host's mount table: what sits at a mount point, and what sits under
//! a directory, read from `/sbin/mount` without touching the mounts.
//!
//! Shared by the daemon, which mounts the guest's docker data and every
//! running machine's root under the host mount root
//! (`arcbox_constants::paths::HostMountLayout`), and by `abctl uninstall`,
//! which removes what a daemon left there. Both must tell their own mounts
//! from a user's — [`MountInfo::is_docker_export`] and
//! [`MountInfo::is_machine_export`] — and both must do so without a `stat`
//! of the mount point: that `stat` is answered by the filesystem mounted
//! there, and hangs when its server is gone, which for an NFS mount a dead
//! daemon left is always.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What is mounted at a path: the `mount` command's source and fstype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub source: String,
    pub fstype: String,
}

impl MountInfo {
    /// The shape of the docker data export: NFS from the loopback proxy,
    /// named by address or by the `ArcBox` hosts alias, at the v4
    /// pseudo-root or below it — the containerd child export the client
    /// mounts on its own under the root has the same host. A stale export
    /// must be reclaimable under either source spelling.
    #[must_use]
    pub fn is_docker_export(&self) -> bool {
        self.fstype == "nfs"
            && self.source.split_once(":/").is_some_and(|(host, _)| {
                host == "127.0.0.1" || host == arcbox_helper::HOSTS_ALIAS_NAME
            })
    }

    /// The shape of a machine root export: NFS from the root of a server
    /// named by a non-loopback IPv4 address — a machine's bridge address,
    /// never the loopback proxy the docker export mounts through. Checked
    /// only under the host mount root, which is the daemon's, so it cannot
    /// mistake a user's mount for one of ours.
    #[must_use]
    pub fn is_machine_export(&self) -> bool {
        self.fstype == "nfs"
            && self
                .source
                .strip_suffix(":/")
                .and_then(|host| host.parse::<Ipv4Addr>().ok())
                .is_some_and(|host| !host.is_loopback())
    }
}

/// Symlinks followed at most while resolving a mount point.
const MAX_SYMLINK_HOPS: usize = 16;

/// A snapshot of the mount table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountTable {
    mounts: Vec<(PathBuf, MountInfo)>,
}

impl MountTable {
    /// Reads the table from `/sbin/mount`; `None` when it cannot be read.
    #[must_use]
    pub fn read() -> Option<Self> {
        let output = Command::new("/sbin/mount").output().ok()?;
        output
            .status
            .success()
            .then(|| Self::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Parses a `/sbin/mount` listing: one `SOURCE on MOUNTPOINT (fstype,
    /// opts…)` line per mount.
    #[must_use]
    pub fn parse(listing: &str) -> Self {
        Self {
            mounts: listing.lines().filter_map(parse_mount_line).collect(),
        }
    }

    /// The mount whose mount point is `path`, if any.
    ///
    /// The kernel names a mount point by its resolved path — `/private/var/
    /// folders/...` for a directory under `/var/folders` — so a literal
    /// comparison never matched one, and a daemon whose mount point sat
    /// behind a symlink neither recognized its own stale mount nor unmounted
    /// it at shutdown (the e2e harness then hung in `TempDir::drop` on the
    /// orphaned mount). But the path is never resolved by a `stat` of the
    /// mount point itself (see the module docs). So only the parent is
    /// canonicalized, the final component is looked up in the table as is,
    /// and `lstat`'d — local for anything but a mount point — only when the
    /// table has no entry, in case it is a symlink to the real mount point.
    /// A path that does not exist has nothing mounted at it.
    #[must_use]
    pub fn at(&self, path: &Path) -> Option<MountInfo> {
        let mut path = path.to_path_buf();
        for _ in 0..MAX_SYMLINK_HOPS {
            let candidate = canonical_mount_point(&path)?;
            if let Some(info) = self.find(&candidate) {
                return Some(info);
            }
            // Not a mount point, so this `lstat` is answered locally.
            let meta = std::fs::symlink_metadata(&candidate).ok()?;
            if !meta.file_type().is_symlink() {
                return None;
            }
            path = candidate
                .parent()?
                .join(std::fs::read_link(&candidate).ok()?);
        }
        None
    }

    /// The mounts below `root` — never `root` itself — deepest first, so a
    /// caller releasing them never meets a mount still covered by a child.
    /// `root` is resolved like a mount point in [`Self::at`]: its parent
    /// canonicalized, itself never `stat`'d, so a root that is a dead mount
    /// is safe to ask about. The mount points come back as the table names
    /// them, canonical.
    #[must_use]
    pub fn under(&self, root: &Path) -> Vec<(PathBuf, MountInfo)> {
        let Some(root) = canonical_mount_point(root) else {
            return Vec::new();
        };
        let mut mounts: Vec<(PathBuf, MountInfo)> = self
            .mounts
            .iter()
            .filter(|(mountpoint, _)| mountpoint.starts_with(&root) && *mountpoint != root)
            .cloned()
            .collect();
        mounts.sort_by_key(|(mountpoint, _)| std::cmp::Reverse(mountpoint.components().count()));
        mounts
    }

    /// The mount whose mount point is exactly `target` (already canonical).
    fn find(&self, target: &Path) -> Option<MountInfo> {
        self.mounts
            .iter()
            .find(|(mountpoint, _)| mountpoint == target)
            .map(|(_, info)| info.clone())
    }
}

/// `path` as the mount table names it: its parent canonicalized and its
/// final component as given, which is never `stat`'d.
fn canonical_mount_point(path: &Path) -> Option<PathBuf> {
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    Some(parent.join(path.file_name()?))
}

/// Parses one `/sbin/mount` line: `SOURCE on MOUNTPOINT (fstype, opts…)`.
fn parse_mount_line(line: &str) -> Option<(PathBuf, MountInfo)> {
    let (source, rest) = line.split_once(" on ")?;
    let (mountpoint, suffix) = rest.split_once(" (")?;
    let fstype = suffix
        .split([',', ')'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((
        PathBuf::from(mountpoint),
        MountInfo {
            source: source.to_string(),
            fstype,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MountInfo, MountTable, parse_mount_line};

    fn nfs(source: &str) -> MountInfo {
        MountInfo {
            source: source.to_string(),
            fstype: "nfs".to_string(),
        }
    }

    /// Both source spellings are ours — loopback (no hosts alias) and the
    /// branded alias — at the pseudo-root and at the containerd child the
    /// client mounts beneath it; a user's own mounts are not.
    #[test]
    fn the_docker_export_is_known_by_its_loopback_or_alias_source() {
        for source in [
            "127.0.0.1:/",
            "ArcBox:/",
            "ArcBox:/containerd",
            "127.0.0.1:/containerd",
        ] {
            assert!(nfs(source).is_docker_export(), "{source}");
            assert!(!nfs(source).is_machine_export(), "{source}");
        }
        assert!(!nfs("fileserver:/export/home").is_docker_export());
        assert!(!nfs("192.168.64.7:/").is_docker_export());
        let smb = MountInfo {
            source: "//user@server/share".to_string(),
            fstype: "smbfs".to_string(),
        };
        assert!(!smb.is_docker_export());
        assert!(!smb.is_machine_export());
    }

    #[test]
    fn only_an_nfs_root_from_an_address_is_a_machine_export() {
        assert!(nfs("192.168.64.7:/").is_machine_export());
        for source in [
            "ArcBox:/",
            "127.0.0.1:/",
            "fileserver:/export",
            "192.168.64.7:/srv",
        ] {
            assert!(!nfs(source).is_machine_export(), "{source}");
        }
    }

    /// The mount point is found through `/var` → `/private/var` and
    /// through a symlink at the final component, without the mount point
    /// itself ever being `stat`'d: nothing is mounted at it here, yet the
    /// table entry built from its parent's canonical path matches.
    #[test]
    fn a_mount_point_resolves_through_symlinks_above_and_at_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap().join("real");
        let table = MountTable::parse(&format!(
            "/dev/disk3s1 on / (apfs, local, journaled)\n\
             192.168.64.3:/ on {} (nfs, nodev, nosuid, mounted by Xuan)\n",
            canonical.display()
        ));
        let info = nfs("192.168.64.3:/");
        assert_eq!(table.at(&dir.path().join("real")), Some(info.clone()));
        assert_eq!(table.at(&dir.path().join("link")), Some(info));
        assert_eq!(table.at(&dir.path().join("missing")), None);
        assert_eq!(table.at(dir.path()), None, "a plain directory");
    }

    #[test]
    fn parse_mount_line_extracts_source_and_fstype() {
        let line =
            "127.0.0.1:/run/arcbox/nfs-export/docker on /Users/t/ArcBox (nfs, nodev, read-only)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, Path::new("/Users/t/ArcBox"));
        assert_eq!(info, nfs("127.0.0.1:/run/arcbox/nfs-export/docker"));
    }

    /// The kernel reports a mount point under `/var/folders` by its resolved
    /// `/private/var/folders` path; the lookup compares against that form
    /// and matches the mount point exactly, never a parent or a child.
    #[test]
    fn find_matches_the_canonical_mount_point_exactly() {
        let table = MountTable::parse(
            "/dev/disk3s1 on / (apfs, local, journaled)\n\
             ArcBox:/ on /private/var/folders/x/arcbox-e2e/ArcBox (nfs, nodev, read-only)\n\
             ArcBox:/containerd on /private/var/folders/x/arcbox-e2e/ArcBox/containerd (nfs, automounted)\n",
        );
        assert_eq!(
            table.find(Path::new("/private/var/folders/x/arcbox-e2e/ArcBox")),
            Some(nfs("ArcBox:/"))
        );
        assert_eq!(
            table.find(Path::new("/private/var/folders/x/arcbox-e2e")),
            None
        );
        assert_eq!(
            table.find(Path::new("/var/folders/x/arcbox-e2e/ArcBox")),
            None,
            "the caller canonicalizes; the uncanonical spelling is not in the table"
        );
    }

    /// Everything below the root, deepest first, so the containerd child
    /// is released before the export it hangs under; the root itself and a
    /// sibling whose name merely extends the root's are not below it.
    #[test]
    fn under_lists_the_descendants_deepest_first() {
        // `/Users` is real on every Mac, so the root's parent canonicalizes.
        let table = MountTable::parse(
            "/dev/disk3s1 on / (apfs, local, journaled)\n\
             ArcBox:/ on /Users/ArcBox (nfs, nodev, read-only)\n\
             ArcBox:/ on /Users/ArcBox/docker (nfs, nodev, read-only)\n\
             192.168.64.7:/ on /Users/ArcBox/machines/ubuntu (nfs, nodev)\n\
             ArcBox:/containerd on /Users/ArcBox/docker/containerd (nfs, automounted)\n\
             192.168.64.8:/ on /Users/ArcBoxMachines/alpine (nfs, nodev)\n",
        );
        let under: Vec<PathBuf> = table
            .under(Path::new("/Users/ArcBox"))
            .into_iter()
            .map(|(mountpoint, _)| mountpoint)
            .collect();
        let position = |path: &str| {
            under
                .iter()
                .position(|mountpoint| mountpoint == Path::new(path))
                .unwrap_or_else(|| panic!("{path} is under the root: {under:?}"))
        };
        assert_eq!(under.len(), 3, "{under:?}");
        assert!(
            position("/Users/ArcBox/docker/containerd") < position("/Users/ArcBox/docker"),
            "a child is released before the mount it hangs under: {under:?}"
        );
        position("/Users/ArcBox/machines/ubuntu");
        assert_eq!(
            table
                .under(Path::new("/Users/ArcBoxMachines"))
                .into_iter()
                .map(|(_, info)| info.source)
                .collect::<Vec<_>>(),
            ["192.168.64.8:/"]
        );
        assert_eq!(
            table.under(Path::new("/Users/Other")).len(),
            0,
            "a root nothing is mounted under"
        );
    }
}
