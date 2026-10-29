//! Network interface discovery and interface-bound connections.
//!
//! Binding the client socket to the address of a specific interface is what
//! makes bonding work on macOS: each transport connection egresses from the
//! interface the user selected (Wi-Fi, Ethernet, iPhone USB, ...).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use anyhow::{anyhow, Result};
use tokio::net::{TcpSocket, TcpStream};

#[derive(Debug, Clone)]
pub struct Iface {
    /// Device name, e.g. `en0`, `en6`.
    pub name: String,
    /// Human readable port name, e.g. `Wi-Fi`, `iPhone USB`.
    pub label: String,
    /// IPv4 address, if the interface currently has one.
    pub ip: Option<IpAddr>,
}

impl Iface {
    pub fn display(&self) -> String {
        match self.ip {
            Some(ip) => format!("{} ({}) · {}", self.label, self.name, ip),
            None => format!("{} ({}) · senza IP", self.label, self.name),
        }
    }
}

fn is_virtual(name: &str) -> bool {
    const PREFIXES: [&str; 12] = [
        "utun", "bridge", "feth", "gif", "stf", "awdl", "llw", "ap", "anpi", "vmenet", "p2p",
        "ipsec",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// macOS: `Hardware Port` <-> device (`en0` = Wi-Fi, `en6` = iPhone USB, ...).
#[cfg(target_os = "macos")]
fn hardware_ports() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = std::process::Command::new("networksetup")
        .arg("-listallhardwareports")
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Hardware Port: ") {
            current = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Device: ") {
            if let Some(label) = current.take() {
                map.insert(rest.trim().to_string(), label);
            }
        }
    }
    map
}

#[cfg(not(target_os = "macos"))]
fn hardware_ports() -> HashMap<String, String> {
    HashMap::new()
}

/// Physical interfaces, with friendly names. Interfaces without an IPv4 are
/// kept (so the UI can say "senza IP") but cannot be selected.
pub fn list() -> Vec<Iface> {
    let ports = hardware_ports();
    let mut out: Vec<Iface> = Vec::new();

    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if i.is_loopback() || is_virtual(&i.name) {
            continue;
        }
        let ip = i.addr.ip();
        match ip {
            IpAddr::V4(v4) if !v4.is_link_local() => {}
            _ => continue,
        }
        if out.iter().any(|s| s.name == i.name) {
            continue;
        }
        let label = ports
            .get(&i.name)
            .cloned()
            .unwrap_or_else(|| i.name.clone());
        out.push(Iface {
            name: i.name,
            label,
            ip: Some(ip),
        });
    }

    // Ports that exist but have no IPv4 right now (e.g. iPhone USB with the
    // hotspot off). Shown as "senza IP" so the cause is obvious.
    for (dev, label) in ports {
        if is_virtual(&dev) || out.iter().any(|s| s.name == dev) {
            continue;
        }
        out.push(Iface {
            name: dev,
            label,
            ip: None,
        });
    }

    out.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Resolve `server` and connect from `local_ip` (if given).
pub async fn connect(server: &str, local_ip: Option<IpAddr>) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(server)
        .await
        .map_err(|e| anyhow!("cannot resolve {server}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(anyhow!("no address found for {server}"));
    }

    let mut last = anyhow!("cannot connect to {server}");
    for addr in addrs {
        let sock = match addr {
            SocketAddr::V4(_) => TcpSocket::new_v4()?,
            SocketAddr::V6(_) => TcpSocket::new_v6()?,
        };
        if let Some(ip) = local_ip {
            if ip.is_ipv4() == addr.is_ipv4() {
                if let Err(e) = sock.bind(SocketAddr::new(ip, 0)) {
                    last = anyhow!("bind {ip}: {e}");
                    continue;
                }
            }
        }
        match sock.connect(addr).await {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                return Ok(stream);
            }
            Err(e) => last = anyhow!("{addr}: {e}"),
        }
    }
    Err(last)
}
