//! The host DNS name of a distro machine.
//!
//! A running machine resolves as `<name>.<local domain>` at its bridge NIC
//! address — the address the Mac reaches directly, unlike the uplink's
//! `10.0.2.x` that every machine shares behind its own NAT. The entries go
//! through the same ownership table as containers and sandboxes, under an
//! owner key with its own prefix so the Docker host-networking reconciler,
//! which treats every unprefixed owner as a container, never tears a machine
//! down as a vanished container.

use std::net::IpAddr;

use super::Runtime;

/// Owner-key prefix of machine DNS entries; see the module docs.
pub(super) const MACHINE_DNS_OWNER_PREFIX: &str = "machine:";

impl Runtime {
    /// Publishes `machine` at `ip`, replacing any earlier address.
    pub async fn register_machine_dns(&self, machine: &str, ip: IpAddr) {
        self.register_dns(&Self::machine_dns_owner(machine), &[machine.to_owned()], ip)
            .await;
    }

    /// Withdraws `machine`'s name; a no-op when it was never published.
    pub async fn deregister_machine_dns(&self, machine: &str) {
        self.deregister_dns_by_id(&Self::machine_dns_owner(machine))
            .await;
    }

    /// The machines whose names are currently published.
    pub async fn registered_machine_dns_names(&self) -> Vec<String> {
        self.dns_entries
            .read()
            .await
            .keys()
            .filter_map(|owner| owner.strip_prefix(MACHINE_DNS_OWNER_PREFIX))
            .map(str::to_owned)
            .collect()
    }

    /// The name `machine` is published under, or `None` when the daemon
    /// serves no local domain.
    #[must_use]
    pub fn machine_dns_name(&self, machine: &str) -> Option<String> {
        self.network_manager
            .dns_domain()
            .map(|domain| format!("{machine}.{domain}"))
    }

    fn machine_dns_owner(machine: &str) -> String {
        format!("{MACHINE_DNS_OWNER_PREFIX}{machine}")
    }
}
