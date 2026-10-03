//! A distro machine's root filesystem, served to the host over NFSv3.
//!
//! The host mounts it under its machine mount root (`~/ArcBoxMachines/<name>`
//! by default) so the machine's files appear on the Mac read-write, the way
//! OrbStack shows a machine under `~/OrbStack/<machine>`. The export rides
//! the bridge NIC the Mac reaches directly, not a vsock relay, so the host's
//! NFS client talks TCP straight to the machine.
//!
//! The server is `nfs3_server` in this process rather than the kernel's
//! nfsd: the machine boots a stock distro image whose kernel nfsd would
//! claim port 2049 and `/etc/exports` from a user's own NFS server, and the
//! shim's EROFS — the only place `rpc.mountd` ships — is gone by the time
//! the agent serves. A userspace server on an ephemeral port owns nothing
//! the distro might want. Its security model is the relay in `server`: only
//! the host's own addresses on the bridge network are admitted. Ownership
//! crosses the wire through `attr::IdMap`; handles come from `ids`.
//!
//! Started once per machine boot by the host's `EnsureMachineExport` RPC,
//! idempotently: the endpoint lives as long as the machine does.

mod ids;
