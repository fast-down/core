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

#[derive(Debug)]
pub struct InterfaceInfo {
    pub name: String,
    pub ip: IpAddr,
}
