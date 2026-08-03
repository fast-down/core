//! Enumeration of usable local network interfaces.
//!
//! [`get_available_local_ips`] returns the non-loopback, non-virtual,
//! non-link-local IP addresses of the host. This powers the multi-interface
//! IP-rotation feature of `FastDownPuller`, where each clone can bind
//! to a different local address.

use std::net::IpAddr;

/// # Errors
/// Returns an error when network interface information cannot be read
#[cfg(not(target_family = "wasm"))]
pub fn get_available_local_ips() -> std::io::Result<Vec<InterfaceInfo>> {
    use getifaddrs::{InterfaceFlags, getifaddrs};
    const VIRTUAL_KEYWORDS: &[&str] = &[
        // Docker and its bridges
        "docker",
        "veth",
        "br-",
        // VPN, tunnels, VMs
        "utun",
        "tun",
        "tap",
        // VirtualBox, VMware
        "vboxnet",
        "vmnet",
        // LOOPBACK is already filtered by flags, but double-check
        "lo",
        // Common networking software
        "tailscale",
        "zerotier",
        "bridge",
        // Virtual bridges and dummy devices
        "dummy",
        "virtual",
        "pseudo",
    ];

    let mut ips = Vec::new();
    let interfaces = getifaddrs()?;
    for interface in interfaces {
        if interface.flags.contains(InterfaceFlags::UP)
            && !interface.flags.contains(InterfaceFlags::LOOPBACK)
            && let Some(ip_addr) = interface.address.ip_addr()
            && !ip_addr.is_unspecified()
            && !is_link_local(&ip_addr)
            && {
                let name = interface.name.to_lowercase();
                !VIRTUAL_KEYWORDS.iter().any(|k| name.contains(k))
            }
        {
            ips.push(InterfaceInfo {
                name: interface.name,
                ip: ip_addr,
            });
        }
    }
    Ok(ips)
}

/// Network interface info is unavailable on wasm, so this always returns `Ok(Vec::new())`
///
/// # Errors
/// Never returns Err
#[cfg(target_family = "wasm")]
pub const fn get_available_local_ips() -> std::io::Result<Vec<InterfaceInfo>> {
    Ok(Vec::new())
}

#[cfg(not(target_arch = "wasm32"))]
const fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// A network interface with its name and assigned IP address.
#[derive(Debug)]
pub struct InterfaceInfo {
    pub name: String,
    pub ip: IpAddr,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{get_available_local_ips, is_link_local};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn is_link_local_v4_true() {
        assert!(is_link_local(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
    }

    #[test]
    fn is_link_local_v4_false() {
        assert!(!is_link_local(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_link_local(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_link_local(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn is_link_local_v6_true() {
        assert!(is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn is_link_local_v6_false() {
        assert!(!is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0x2001, 0xdb8, 0, 0, 0, 0, 0, 1
        ))));
        assert!(!is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn is_link_local_v6_boundaries() {
        // fe80::/10 spans fe80::..febf::; the mask is (seg0 & 0xffc0) == 0xfe80.
        // Lower bound inclusive.
        assert!(is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        // Upper bound inclusive (febf is still inside /10).
        assert!(is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0xfebf, 0, 0, 0, 0, 0, 0, 1
        ))));
        // fec0 (old site-local) is just above the /10 -> not link-local.
        assert!(!is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0xfec0, 0, 0, 0, 0, 0, 0, 1
        ))));
        // fe7f is just below the /10 -> not link-local.
        assert!(!is_link_local(&IpAddr::V6(Ipv6Addr::new(
            0xfe7f, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn is_link_local_v4_boundaries() {
        // 169.254.0.0/16 bounds.
        assert!(is_link_local(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 0))));
        assert!(is_link_local(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 255, 255
        ))));
        assert!(!is_link_local(&IpAddr::V4(Ipv4Addr::new(
            169, 253, 255, 255
        ))));
        assert!(!is_link_local(&IpAddr::V4(Ipv4Addr::new(169, 255, 0, 0))));
    }

    #[test]
    fn get_available_local_ips_excludes_loopback() {
        let ips = get_available_local_ips().expect("get_available_local_ips must succeed");
        for iface in &ips {
            assert!(
                !iface.ip.is_loopback(),
                "returned ip {} must not be loopback (filtered by the LOOPBACK flag)",
                iface.ip
            );
        }
    }

    #[test]
    fn get_available_local_ips_satisfies_invariants() {
        let ips = get_available_local_ips().expect("get_available_local_ips must succeed");
        let virtual_keywords = [
            "docker",
            "veth",
            "br-",
            "utun",
            "tun",
            "tap",
            "vboxnet",
            "vmnet",
            "lo",
            "tailscale",
            "zerotier",
            "bridge",
            "dummy",
            "virtual",
            "pseudo",
        ];
        for iface in &ips {
            assert!(
                !iface.ip.is_unspecified(),
                "returned ip {} must not be unspecified",
                iface.ip
            );
            assert!(
                !is_link_local(&iface.ip),
                "returned ip {} must not be link-local (filtered by get_available_local_ips)",
                iface.ip
            );
            let name = iface.name.to_lowercase();
            assert!(
                !virtual_keywords.iter().any(|k| name.contains(k)),
                "returned interface name {name} must not match a virtual-keyword"
            );
        }
    }
}
