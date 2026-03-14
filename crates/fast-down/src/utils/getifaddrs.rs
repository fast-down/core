use std::net::IpAddr;

/// # Errors
/// 无法读取网卡信息时报错
#[cfg(not(target_family = "wasm"))]
pub fn get_available_local_ips() -> std::io::Result<Vec<InterfaceInfo>> {
    use getifaddrs::{InterfaceFlags, getifaddrs};
    const VIRTUAL_KEYWORDS: &[&str] = &[
        // Docker 及其网桥
        "docker",
        "veth",
        "br-",
        // VPN, 隧道, 虚拟机
        "utun",
        "tun",
        "tap",
        // VirtualBox, VMware
        "vboxnet",
        "vmnet",
        // 虽然已经过滤了 LOOPBACK 标志，但双重保险
        "lo",
        // 常见的组网软件
        "tailscale",
        "zerotier",
        "bridge",
        // 虚拟桥接和空设备
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

/// 无法在 wasm 上获取网卡信息，所以永远返回 `Ok(Vec::new())`
///
/// # Errors
/// 永远不会返回 Err
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
