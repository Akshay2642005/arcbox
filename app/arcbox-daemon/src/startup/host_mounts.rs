//! The host mount root at startup.
//!
//! The root — `~/ArcBox`, or `ARCBOX_HOST_MOUNT_DIR` — is a plain directory
//! this daemon owns: the docker export is mounted at `docker/` and every
//! running machine's root at `machines/<name>` (ADR 0003). Before anything
//! is mounted there, what a previous daemon left is released. Its servers
//! died with it, so each of those mounts is dead weight that hangs whoever
//! touches it — this daemon included, the moment it created a directory
//! beneath one. A daemon from before the single root mounted the export at
//! the root itself and machine roots under `<root>Machines`; both are
//! released and their empty directories removed, so an upgrade asks nothing
//! of the user.

use std::path::Path;

use anyhow::{Context, Result, bail};
use arcbox_constants::paths::HostMountLayout;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;
use crate::host_mount::{self, MountInfo};

/// Releases what a previous daemon left under the host mount root and
/// creates the directories this one mounts into.
///
/// # Errors
///
/// Returns an error when the root is still a mount afterwards — one this
/// daemon did not create, or one it could not release — or when a
/// directory cannot be created. Nothing can be mounted under a root that
/// is itself a mount, and a dead one would hang the attempt.
pub(super) async fn prepare(ctx: &DaemonContext) -> Result<()> {
    let layout = &ctx.host_mounts;
    release_stale(layout).await;

    if let Some(info) = host_mount::current_mount_info(layout.root()) {
        bail!(
            "the host mount root {} is occupied by {} ({}); unmount it or point {} elsewhere",
            layout.root().display(),
            info.source,
            info.fstype,
            arcbox_constants::env::HOST_MOUNT_DIR
        );
    }
    let mut dirs = vec![layout.machines()];
    if ctx.mount_nfs {
        dirs.push(layout.docker());
    }
    for dir in dirs {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(())
}

/// Releases every mount of ours a previous daemon left — under the root,
/// at the root itself, and under the pre-single-root machine mount root —
/// and removes the mount points that are then empty.
///
/// The unmounts are forced from the start: the daemon that made them is
/// gone and its guests with it, so a plain `umount` would only wait out
/// its timeout on a server that cannot answer. A failure is logged, not
/// fatal: [`prepare`] refuses a root that stays a mount, and a machine
/// mount that stays is replaced when its machine next starts.
async fn release_stale(layout: &HostMountLayout) {
    let root = layout.root();
    let mut stale = host_mount::mounts_under(root);
    if let Some(info) = host_mount::current_mount_info(root) {
        stale.push((root.to_path_buf(), info));
    }
    for (path, info) in stale {
        if info.is_docker_export() || info.is_machine_export() {
            release(&path, &info).await;
        } else {
            warn!(
                path = %path.display(),
                source = %info.source,
                fstype = %info.fstype,
                "a mount this daemon did not create sits under the host mount root; leaving it"
            );
        }
    }
    // Below a root that is still a mount nothing is safe to touch.
    if host_mount::current_mount_info(root).is_none() {
        remove_empty_mount_points(&layout.machines());
    }

    let legacy = layout.legacy_machines_root();
    if !legacy.is_dir() {
        return;
    }
    for (path, info) in host_mount::mounts_under(&legacy) {
        if info.is_machine_export() {
            release(&path, &info).await;
        }
    }
    remove_empty_mount_points(&legacy);
    match std::fs::remove_dir(&legacy) {
        Ok(()) => info!(
            path = %legacy.display(),
            "removed the machine mount root of a daemon from before the single host mount root"
        ),
        Err(e) => warn!(
            path = %legacy.display(),
            error = %e,
            "the pre-single-root machine mount root is not empty; leaving it"
        ),
    }
}

async fn release(path: &Path, info: &MountInfo) {
    match host_mount::unmount_force(path).await {
        Ok(()) => info!(
            path = %path.display(),
            source = %info.source,
            "released a mount a previous daemon left behind"
        ),
        Err(e) => warn!(
            path = %path.display(),
            source = %info.source,
            error = %e,
            "could not release a mount a previous daemon left behind"
        ),
    }
}

/// Removes the empty directories directly under `dir` — mount points whose
/// mounts are gone. `dir` itself stays, and so does anything with content.
fn remove_empty_mount_points(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir(&path) {
            Ok(()) => info!(path = %path.display(), "removed a stale mount point"),
            Err(e) => debug!(path = %path.display(), error = %e, "mount point not removed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use arcbox_constants::paths::HostMountLayout;

    use super::{release_stale, remove_empty_mount_points};

    /// With nothing mounted, the release is a directory sweep: the empty
    /// mount points of stopped machines go, a directory with content and
    /// a plain file stay, and the pre-single-root machine mount root is
    /// removed once it is empty.
    #[tokio::test]
    async fn stale_mount_points_are_swept_and_the_legacy_root_removed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = HostMountLayout::new(dir.path().join("ArcBox"));
        std::fs::create_dir_all(layout.machine("gone")).unwrap();
        std::fs::create_dir_all(layout.machine("kept")).unwrap();
        std::fs::write(layout.machine("kept").join("notes"), "the user's").unwrap();
        std::fs::write(layout.machines().join(".DS_Store"), "").unwrap();
        let legacy = layout.legacy_machines_root();
        std::fs::create_dir_all(legacy.join("ubuntu")).unwrap();
        assert_eq!(legacy, dir.path().join("ArcBoxMachines"));

        release_stale(&layout).await;

        assert!(!layout.machine("gone").exists());
        assert!(layout.machine("kept").join("notes").exists());
        assert!(layout.machines().join(".DS_Store").exists());
        assert!(!legacy.exists());
    }

    #[tokio::test]
    async fn a_legacy_root_with_the_users_files_stays() {
        let dir = tempfile::tempdir().unwrap();
        let layout = HostMountLayout::new(dir.path().join("ArcBox"));
        let legacy = layout.legacy_machines_root();
        std::fs::create_dir_all(legacy.join("ubuntu")).unwrap();
        std::fs::write(legacy.join("ubuntu/readme"), "kept").unwrap();

        release_stale(&layout).await;

        assert!(legacy.join("ubuntu/readme").exists());
    }

    #[test]
    fn sweeping_a_missing_directory_is_a_no_op() {
        remove_empty_mount_points(std::path::Path::new("/nonexistent/arcbox-test"));
    }
}
