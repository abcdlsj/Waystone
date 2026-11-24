//! Picks the address to advertise when the operator configured none.
//!
//! `localhost` in a pairing URI is useless on a phone, so the daemon falls back
//! to the machine's primary LAN IPv4: the first interface address that is not
//! loopback, link-local or point-to-point, preferring private (RFC 1918) ranges
//! and physical-looking interfaces over container bridges and VPN tunnels.

use std::net::Ipv4Addr;

/// An interface address the daemon could advertise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub interface: String,
    pub ip: Ipv4Addr,
}

/// Interface name prefixes of virtual devices that a phone cannot reach.
const VIRTUAL_PREFIXES: &[&str] = &[
    "docker",
    "br-",
    "veth",
    "virbr",
    "vmnet",
    "vboxnet",
    "utun",
    "tun",
    "tap",
    "bridge",
    "awdl",
    "llw",
    "cni",
    "flannel",
    "tailscale",
    "wg",
    "zt",
];

/// The primary LAN IPv4 of this machine, if any.
pub fn primary_ipv4() -> Option<Candidate> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    pick(interfaces.into_iter().filter_map(|interface| {
        let down = matches!(
            interface.oper_status,
            if_addrs::IfOperStatus::Down | if_addrs::IfOperStatus::NotPresent
        );
        if interface.is_loopback() || interface.is_p2p() || down {
            return None;
        }
        match interface.addr {
            if_addrs::IfAddr::V4(v4) => Some(Candidate {
                interface: interface.name,
                ip: v4.ip,
            }),
            _ => None,
        }
    }))
}

/// Chooses among `candidates`, keeping the listing order within a rank.
pub fn pick(candidates: impl IntoIterator<Item = Candidate>) -> Option<Candidate> {
    candidates
        .into_iter()
        .filter(|candidate| {
            let ip = candidate.ip;
            !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_multicast())
        })
        .enumerate()
        .min_by_key(|(index, candidate)| {
            let virtual_interface = VIRTUAL_PREFIXES
                .iter()
                .any(|prefix| candidate.interface.starts_with(prefix));
            (virtual_interface, !candidate.ip.is_private(), *index)
        })
        .map(|(_, candidate)| candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(interface: &str, ip: [u8; 4]) -> Candidate {
        Candidate {
            interface: interface.into(),
            ip: Ipv4Addr::from(ip),
        }
    }

    #[test]
    fn prefers_a_private_physical_address() {
        let chosen = pick([
            candidate("lo0", [127, 0, 0, 1]),
            candidate("en5", [169, 254, 3, 4]),
            candidate("docker0", [172, 17, 0, 1]),
            candidate("en1", [203, 0, 113, 9]),
            candidate("en0", [192, 168, 1, 10]),
        ]);
        assert_eq!(chosen, Some(candidate("en0", [192, 168, 1, 10])));
    }

    #[test]
    fn falls_back_to_public_then_virtual() {
        assert_eq!(
            pick([
                candidate("docker0", [172, 17, 0, 1]),
                candidate("eth0", [203, 0, 113, 9]),
            ]),
            Some(candidate("eth0", [203, 0, 113, 9]))
        );
        assert_eq!(
            pick([candidate("docker0", [172, 17, 0, 1])]),
            Some(candidate("docker0", [172, 17, 0, 1]))
        );
        assert_eq!(pick([candidate("lo", [127, 0, 0, 1])]), None);
    }
}
