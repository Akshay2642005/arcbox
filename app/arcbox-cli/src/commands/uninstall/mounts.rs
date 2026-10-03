//! What a daemon left mounted under the host mount root, and its removal.
//!
//! The daemon is already stopped when this runs, so every server behind a
//! mount it left is gone: a plain `umount` would wait out its timeout on
//! answers that never come, and so would a `stat` of the mount point. The
//! unmounts are therefore forced, and the mount table is read from
//! `/sbin/mount` through the [`Host`] seam rather than by touching the
//! mount points.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arcbox_core::host_mount::{MountInfo, MountTable};

use super::host::{Host, run_checked};
use super::steps::{Outcome, skipped};

/// Unmounts every mount of ours under `root` — `root` itself included,
/// deepest first — then removes the empty directories beneath it and the
/// root. `ours` decides which mounts are the daemon's; a mount of another
/// shape, or a directory with content, is left alone, and so is everything
/// above it.
pub(super) fn remove_host_mounts(
    host: &dyn Host,
    root: &Path,
    ours: fn(&MountInfo) -> bool,
) -> Result<Outcome> {
    if root.symlink_metadata().is_err() {
        return Ok(skipped("absent"));
    }
    let listing = run_checked(host, "/sbin/mount", &[])?;
    let table = MountTable::parse(&String::from_utf8_lossy(&listing.stdout));
    let mut mounts = table.under(root);
    if let Some(info) = table.at(root) {
        mounts.push((canonical_root(root)?, info));
    }

    let mut kept = Vec::new();
    let mut foreign = Vec::new();
    for (mountpoint, info) in mounts {
        if ours(&info) {
            run_checked(
                host,
                "/sbin/umount",
                &[OsStr::new("-f"), mountpoint.as_os_str()],
            )?;
        } else {
            kept.push(format!(
                "{} ({}) at {}",
                info.source,
                info.fstype,
                mountpoint.display()
            ));
            foreign.push(mountpoint);
        }
    }
    // The table names mount points canonically; sweep the same spelling so
    // a foreign mount point is recognized and never descended into.
    remove_empty_directories(&canonical_root(root)?, &foreign, &mut kept)?;
    if kept.is_empty() {
        Ok(Outcome::Done)
    } else {
        Ok(skipped(format!("left alone: {}", kept.join(", "))))
    }
}

/// `root` as the mount table names it: its parent canonicalized, itself
/// never `stat`'d.
fn canonical_root(root: &Path) -> Result<PathBuf> {
    let parent = root
        .parent()
        .with_context(|| format!("{} has no parent", root.display()))?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("could not resolve {}", parent.display()))?;
    let name = root
        .file_name()
        .with_context(|| format!("{} has no name", root.display()))?;
    Ok(parent.join(name))
}

/// Removes `dir` and the directories under it, deepest first, as far as
/// they are empty. A directory with content of its own, or a mount point
/// in `foreign`, stays and is reported in `kept`; so does everything above
/// it, which is only kept by it and so is not reported again. Returns
/// whether `dir` is gone.
fn remove_empty_directories(
    dir: &Path,
    foreign: &[PathBuf],
    kept: &mut Vec<String>,
) -> Result<bool> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("could not read {}", dir.display()))?;
    let mut own_content = false;
    let mut kept_below = false;
    for entry in entries {
        let entry = entry.with_context(|| format!("could not read {}", dir.display()))?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            own_content = true;
        } else if foreign.contains(&path) || !remove_empty_directories(&path, foreign, kept)? {
            // A foreign mount point is never descended into.
            kept_below = true;
        }
    }
    if own_content {
        kept.push(dir.display().to_string());
    }
    if own_content || kept_below {
        return Ok(false);
    }
    std::fs::remove_dir(dir).with_context(|| format!("could not remove {}", dir.display()))?;
    Ok(true)
}
