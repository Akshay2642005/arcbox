//! Mounts every running distro machine's root filesystem on the host.
//!
//! A machine that reaches readiness is asked (`EnsureMachineExport`) to
//! serve its root over NFSv3 on its bridge NIC, and the endpoint is mounted
//! read-write at `<root>/<name>`: `~/ArcBoxMachines/<name>` unless
//! `ARCBOX_MACHINE_MOUNT_DIR` moves the root, which the e2e harness and the
//! dev daemons do to stay inside their data dir. The mount is released when
//! the machine begins to stop, so the unmount still reaches a live server,
//! and swept again once it has stopped or been removed; daemon shutdown
//! unmounts everything before it stops the machines, for the same reason.
//!
//! The loop follows the runtime's event bus the way `machine_dns` does and
//! re-derives what should be mounted from each machine's record, so a
//! lagged receiver is repaired by one pass. [`mount_machine`] and
//! [`unmount_machine`] are the two operations; a lifecycle operation that
//! needs the mount gone before it acts on the machine's disks (export,
//! clone) calls [`unmount_machine`] itself.
//!
//! Why not `~/ArcBox/machines/<name>`: `~/ArcBox` is itself the read-only
//! NFS mount of the System VM's docker data, so nothing can be created
//! inside it. Moving that export to `~/ArcBox/docker` would free the name,
//! but the desktop app maps guest paths onto `~/ArcBox/<rest>` and would
//! have to move with it; that layout change is a decision of its own.

mod export;
