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

mod attr;
mod ids;

use std::net::IpAddr;

use arcbox_connect::v1::EnsureMachineExportRequest;
pub use attr::IdMap;

/// What the host asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConfig {
    /// The ownership swap between the host user and the guest account.
    pub ids: IdMap,
    /// The only peers the export admits.
    pub client_addresses: Vec<IpAddr>,
}

impl ExportConfig {
    /// Reads the request, refusing one that would admit no peer at all or
    /// names an address that is not one.
    pub fn from_request(req: &EnsureMachineExportRequest) -> Result<Self, String> {
        if req.client_addresses.is_empty() {
            return Err("no client addresses: the export would admit no connection".to_owned());
        }
        let client_addresses = req
            .client_addresses
            .iter()
            .map(|raw| {
                raw.parse::<IpAddr>()
                    .map_err(|e| format!("client address {raw:?}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            ids: IdMap::new(req.host_uid, req.host_gid, req.guest_uid, req.guest_gid),
            client_addresses,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_names_at_least_one_valid_peer() {
        let req = EnsureMachineExportRequest {
            client_addresses: vec!["192.168.64.1".to_owned()],
            host_uid: 501,
            host_gid: 20,
            ..Default::default()
        };
        let config = ExportConfig::from_request(&req).unwrap();
        assert_eq!(
            config.client_addresses,
            vec!["192.168.64.1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(config.ids, IdMap::new(501, 20, 0, 0));

        let empty = EnsureMachineExportRequest::default();
        assert!(ExportConfig::from_request(&empty).is_err());
        let garbage = EnsureMachineExportRequest {
            client_addresses: vec!["bridge100".to_owned()],
            ..Default::default()
        };
        assert!(ExportConfig::from_request(&garbage).is_err());
    }
}
