//! The host side of an NFS mount: what sits at a mount point, and how to
//! mount or unmount it without waiting forever.
//!
//! Shared by the docker-data mount (`nfs_mount`), the per-machine root
//! mounts (`machine_mount`) and the startup release of what a previous
//! daemon left (`startup::host_mounts`). The mount table itself is read by
//! `arcbox_core::host_mount`, which `abctl uninstall` shares. Every
//! external command here is bounded: `mount_nfs` and `umount` both block
//! inside the kernel while an NFS server is unresponsive, and an unbounded
//! wait on either keeps the daemon alive after it has logged "ArcBox daemon
//! stopped".

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
pub use arcbox_core::host_mount::MountInfo;
use arcbox_core::host_mount::MountTable;

/// How long one `mount_nfs` invocation may take before it is killed and the
/// attempt counts as failed. A healthy mount completes in well under a
/// second; a server that stops answering mid-handshake otherwise holds the
/// process — and the thread waiting on it — until the kernel gives up.
pub const MOUNT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
/// Shutdown may make two attempts; their 10s total is part of launchd's
/// 45s budget.
pub const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// The mount whose mount point is `path`, if any; see [`MountTable::at`].
pub fn current_mount_info(path: &Path) -> Option<MountInfo> {
    MountTable::read()?.at(path)
}

/// The mounts below `root`, deepest first; see [`MountTable::under`].
pub fn mounts_under(root: &Path) -> Vec<(PathBuf, MountInfo)> {
    MountTable::read().map_or_else(Vec::new, |table| table.under(root))
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
    run_umount(path, &[]).await
}

/// Forcibly unmounts `path` (`umount -f`): the NFS client abandons its
/// outstanding requests instead of waiting for a server that is gone.
pub async fn unmount_force(path: &Path) -> Result<()> {
    run_umount(path, &["-f"]).await
}

async fn run_umount(path: &Path, flags: &[&str]) -> Result<()> {
    let mut command = tokio::process::Command::new("/sbin/umount");
    command.args(flags).arg(path).kill_on_drop(true);
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
