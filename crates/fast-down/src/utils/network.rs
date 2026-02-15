use getifaddrs::{InterfaceFlags, getifaddrs};
use std::net::IpAddr;

pub fn get_available_local_ips() -> std::io::Result<Vec<InterfaceInfo>> {
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

fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

pub struct InterfaceInfo {
    pub name: String,
    pub ip: IpAddr,
}
