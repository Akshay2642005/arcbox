# ADR 0003: One host mount root — `~/ArcBox` is a plain directory, with the docker export at `docker/` and every running machine's root at `machines/<name>`

- Status: accepted (2026-10-04); supersedes ADR 0002 decisions 4 and 5
- Deciders: Xuan
- Commits: the `feat/arcbox-single-mount-root` series (`arcbox_constants::paths::HostMountLayout`, `app/arcbox-daemon/src/startup/host_mounts.rs`, `app/arcbox-daemon/src/nfs_mount.rs`, `app/arcbox-daemon/src/machine_mount/`, `app/arcbox-cli/src/commands/uninstall/mounts.rs`, `engine/arcbox-engine/src/machine/host_hold.rs`); arcbox-desktop `feat/docker-export-under-arcbox-root`
- Evidence: `tests/e2e/tests/machine.rs` (`machine_root_mounted_on_the_host`) and `tests/e2e/src/boot_assets.rs` (`verify_nfs_export`) for the layout; the upgrade from the two-root layout and the `remove --force` timing were verified by hand against a dev daemon on 2026-10-04 (the commit series' final report)

## Context

ArcBox showed guest filesystems in two places on the Mac. `~/ArcBox` was
itself the read-only NFSv4 mount of the System VM's docker data (the
kernel's nfsd behind a vsock relay), and `~/ArcBoxMachines/<name>` was a
read-write NFSv3 mount per running machine (ADR 0002). ADR 0002's decision
5 explained why the machines could not live under `~/ArcBox`: nothing can
be created inside a read-only NFS mount, and moving the docker export out
of the way would move the paths the desktop app maps guest paths onto.

The user decided there must not be two roots: one `ArcBox` folder in
Finder, with everything under it.

Two more facts shaped the layout. A test or development daemon relocates
its mounts with environment variables, and there were two of them
(`ARCBOX_HOST_MOUNT_DIR`, `ARCBOX_MACHINE_MOUNT_DIR`). And a daemon that
dies — `kill -9`, a crash — leaves its NFS mounts behind with no server:
every process that touches one hangs, the next daemon included the moment
it creates a directory beneath one, so a layout change has to come with
the cleanup of what the previous layout left.

## Decision

1. `~/ArcBox` is a plain directory the daemon creates and owns. One
   variable, `ARCBOX_HOST_MOUNT_DIR`, relocates the whole root; every path
   under it derives from `arcbox_constants::paths::HostMountLayout`.
   `ARCBOX_MACHINE_MOUNT_DIR` no longer exists.
2. The docker data export is mounted at `<root>/docker`; its containerd
   child export, which the NFSv4 client mounts on its own, lands at
   `docker/containerd`. A running machine's root is mounted at
   `<root>/machines/<name>`. Machine names need no reserved word.
3. Before it mounts anything, a daemon releases what a previous daemon
   left (`release_stale_resources`): the docker export and machine roots
   under the root, deepest first; the export a pre-0003 daemon mounted at
   the root itself; and the machine roots a pre-0003 daemon mounted under
   `<root>Machines`, whose empty directories it removes. The unmounts are
   forced from the start — the servers died with that daemon. A root that
   is still a mount afterwards fails startup with the occupant named:
   nothing can be mounted beneath it.
4. `abctl uninstall` removes the same set and the pre-0003 roots, from the
   mount table `/sbin/mount` reports, never by touching a mount point; a
   mount of another shape or a directory with the user's files stays, and
   so does everything above it.
5. A force stop of a machine (`machine stop --force`, `machine remove
   --force`) publishes `MachineStopping` and then waits, at most 10 s, for
   the host to release what it holds of the machine before it kills the
   VM (`MachineManager::host_hold`). The mount loop holds while a root is
   mounted and lets go on its first release attempt. A graceful stop does
   not wait: the guest's own shutdown outlasts the release.
6. The desktop app maps `/var/lib/docker/<rest>` onto
   `~/ArcBox/docker/<rest>` and `/var/lib/containerd/<rest>` onto
   `~/ArcBox/docker/containerd/<rest>`.

## Consequences

- ADR 0002's decisions 4 and 5 are superseded; its export mechanism,
  admission rule, ownership mapping and sidecar handling stand.
- Finder shows one `ArcBox` folder. In its Locations sidebar the docker
  export still appears as the `ArcBox` server (the `/etc/hosts` alias),
  now mounted at `~/ArcBox/docker`; each running machine appears by its
  bridge address.
- An upgrade needs nothing from the user: the first daemon of this layout
  removes the old one's residue and logs what it did. A dev daemon's stop
  script must look for mounts *under* `<DATA_DIR>/ArcBox/`, not at it.
- Mount-point directories outlive their mounts only while the daemon is
  down: `docker/` and `machines/` stay as empty directories after a clean
  shutdown, and a plain `rm -rf ~/ArcBox` removes everything.
- `docs/data-directories.md` and the README describe the layout; the e2e
  harness derives the paths from `arcbox_e2e::daemon::host_mounts`.
- Changing the root, the names `docker` and `machines`, or the startup
  release is a new ADR; so is a stop path that kills a VM while the host
  still holds one of its mounts.

## Alternatives considered

- **Keep the two roots**: the user's decision was one folder.
- **Machines under `~/ArcBox/machines` with the export still at
  `~/ArcBox`**: impossible — the root was a read-only NFS mount (ADR 0002
  decision 5).
- **A manual migration (fail startup and tell the user what to unmount)**:
  the residue is ours and the release is mechanical; failing is kept for
  the one case that is not ours, a foreign mount at the root.
- **A plain `umount` first, forcing only on failure, in the startup
  release**: the servers are gone by construction, so the plain attempt
  can only wait out its timeout.
- **Making the engine unmount the machine's root itself before a force
  stop**: the mounts are the daemon's (`app/`), and the engine must not
  know them; a hold the holder registers keeps the layers apart.
