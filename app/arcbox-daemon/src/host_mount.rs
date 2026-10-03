//! The host side of an NFS mount: what sits at a mount point, and how to
//! mount or unmount it without waiting forever.
//!
//! Shared by the `~/ArcBox` docker-data mount (`nfs_mount`) and the
//! per-machine root mounts (`machine_mount`). Every external command here is
//! bounded: `mount_nfs` and `umount` both block inside the kernel while an
//! NFS server is unresponsive, and an unbounded wait on either keeps the
//! daemon alive after it has logged "ArcBox daemon stopped".

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// How long one `mount_nfs` invocation may take before it is killed and the
/// attempt counts as failed. A healthy mount completes in well under a
/// second; a server that stops answering mid-handshake otherwise holds the
/// process — and the thread waiting on it — until the kernel gives up.
pub const MOUNT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Shutdown may make two attempts; their 10s total is part of launchd's
/// 45s budget.
pub const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// What is mounted at a path: the `mount` command's source and fstype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub source: String,
    pub fstype: String,
}

/// The mount whose mount point is exactly `path`, if any.
///
/// The path is canonicalized before the comparison. The kernel names a
/// mount point by its resolved path — `/private/var/folders/...` for a data
/// dir under `/var/folders` — so a literal comparison never matched one,
/// and a daemon whose mount point sat behind a symlink neither recognized
/// its own stale mount nor unmounted it at shutdown (the e2e harness then
/// hung in `TempDir::drop` on the orphaned mount). A path that does not
/// exist has nothing mounted at it.
pub fn current_mount_info(path: &Path) -> Option<MountInfo> {
    let target = std::fs::canonicalize(path).ok()?;
    let output = Command::new("/sbin/mount").output().ok()?;
    if !output.status.success() {
        return None;
    }
    find_mount(&String::from_utf8_lossy(&output.stdout), &target)
}

/// Finds the mount whose mount point is `target` (already canonical) in
/// `/sbin/mount` output.
fn find_mount(mount_output: &str, target: &Path) -> Option<MountInfo> {
    mount_output
        .lines()
        .find_map(|line| match parse_mount_line(line) {
            Some((mountpoint, info)) if Path::new(mountpoint) == target => Some(info),
            _ => None,
        })
}

/// Parses one `/sbin/mount` line: `SOURCE on MOUNTPOINT (fstype, opts…)`.
pub fn parse_mount_line(line: &str) -> Option<(&str, MountInfo)> {
    let (source, rest) = line.split_once(" on ")?;
    let (mountpoint, suffix) = rest.split_once(" (")?;
    let fstype = suffix
        .split([',', ')'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    Some((
        mountpoint,
        MountInfo {
            source: source.to_string(),
            fstype,
        },
    ))
}

/// Runs `mount_nfs -o <opts> <source> <mount_path>` once.
///
/// The process is killed when it outlives [`MOUNT_ATTEMPT_TIMEOUT`]; the
/// caller decides whether to retry. The error carries `mount_nfs`'s own
/// diagnostic, which is the only place the kernel's refusal is spelled out.
pub async fn mount_nfs(opts: &str, source: &str, mount_path: &Path) -> Result<()> {
    let mut command = tokio::process::Command::new("/sbin/mount_nfs");
    command
        .arg("-o")
        .arg(opts)
        .arg(source)
        .arg(mount_path)
        .kill_on_drop(true);
    let output = tokio::time::timeout(MOUNT_ATTEMPT_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("mount_nfs did not return within {MOUNT_ATTEMPT_TIMEOUT:?}"))?
        .context("failed to execute mount_nfs")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "mount_nfs exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// Unmounts `path`, waiting at most [`UNMOUNT_TIMEOUT`].
pub async fn unmount(path: &Path) -> Result<()> {
    let mut command = tokio::process::Command::new("/sbin/umount");
    command.arg(path).kill_on_drop(true);
    let output = tokio::time::timeout(UNMOUNT_TIMEOUT, command.output())
        .await
        .context("umount timed out")??;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "umount exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{MountInfo, find_mount, parse_mount_line};

    #[test]
    fn parse_mount_line_extracts_source_and_fstype() {
        let line =
            "127.0.0.1:/run/arcbox/nfs-export/docker on /Users/t/ArcBox (nfs, nodev, read-only)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, "/Users/t/ArcBox");
        assert_eq!(
            info,
            MountInfo {
                source: "127.0.0.1:/run/arcbox/nfs-export/docker".to_string(),
                fstype: "nfs".to_string(),
            }
        );
    }

    /// The kernel reports a mount point under `/var/folders` by its resolved
    /// `/private/var/folders` path; the lookup compares against that form
    /// and matches the mount point exactly, never a parent or a child.
    #[test]
    fn find_mount_matches_the_canonical_mount_point_exactly() {
        let output = "/dev/disk3s1 on / (apfs, local, journaled)\n\
                      ArcBox:/ on /private/var/folders/x/arcbox-e2e/ArcBox (nfs, nodev, read-only)\n\
                      ArcBox:/containerd on /private/var/folders/x/arcbox-e2e/ArcBox/containerd (nfs, automounted)\n";
        let found = find_mount(
            output,
            Path::new("/private/var/folders/x/arcbox-e2e/ArcBox"),
        );
        assert_eq!(
            found,
            Some(MountInfo {
                source: "ArcBox:/".to_string(),
                fstype: "nfs".to_string(),
            })
        );
        assert_eq!(
            find_mount(output, Path::new("/private/var/folders/x/arcbox-e2e")),
            None
        );
        assert_eq!(
            find_mount(output, Path::new("/var/folders/x/arcbox-e2e/ArcBox")),
            None,
            "the caller canonicalizes; the uncanonical spelling is not in the table"
        );
    }
}
