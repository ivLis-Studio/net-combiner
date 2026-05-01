use std::net::IpAddr;

use anyhow::Result;
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterAddress {
    pub name: String,
    pub ip: IpAddr,
    pub mac: Option<String>,
    pub index: Option<u32>,
    pub is_loopback: bool,
    pub is_private: bool,
    pub is_link_local: bool,
}

impl AdapterAddress {
    pub fn family_label(&self) -> &'static str {
        match self.ip {
            IpAddr::V4(_) => "IPv4",
            IpAddr::V6(_) => "IPv6",
        }
    }

    pub fn scope_label(&self) -> &'static str {
        if self.is_loopback {
            "loopback"
        } else if self.is_link_local {
            "link-local"
        } else if self.is_private {
            "private"
        } else {
            "public"
        }
    }

    pub fn stable_id(&self) -> String {
        format!("{}|{}", self.name, self.ip)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressTarget {
    pub name: String,
    pub ip: IpAddr,
    pub weight: u16,
}

pub fn list_adapters() -> Result<Vec<AdapterAddress>> {
    let mut output = Vec::new();

    for interface in NetworkInterface::show()? {
        let name = interface.name;
        let mac = interface.mac_addr.map(|mac| mac.to_string());
        let index = Some(interface.index);

        for addr in interface.addr {
            let ip = match addr {
                Addr::V4(v4) => IpAddr::V4(v4.ip),
                Addr::V6(v6) => IpAddr::V6(v6.ip),
            };
            if ip.is_unspecified() {
                continue;
            }

            output.push(AdapterAddress {
                name: name.clone(),
                ip,
                mac: mac.clone(),
                index,
                is_loopback: ip.is_loopback(),
                is_private: is_private_ip(ip),
                is_link_local: is_link_local_ip(ip),
            });
        }
    }

    output.sort_by(|a, b| {
        a.is_loopback
            .cmp(&b.is_loopback)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.ip.cmp(&b.ip))
    });
    Ok(output)
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            (segments[0] & 0xfe00) == 0xfc00
        }
    }
}

fn is_link_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_link_local(),
        IpAddr::V6(ip) => (ip.segments()[0] & 0xffc0) == 0xfe80,
    }
}
