//! Network interface discovery and interface-bound connections.
//!
//! Binding the client socket to the address of a specific interface is what
//! makes bonding work on macOS: each transport connection egresses from the
//! interface the user selected (Wi-Fi, Ethernet, iPhone hotspot, ...).

use std::net::{IpAddr, SocketAddr};

use anyhow::{anyhow, Result};
use tokio::net::{TcpSocket, TcpStream};

#[derive(Debug, Clone)]
pub struct Iface {
    pub name: String,
    pub ip: IpAddr,
}

/// All usable IPv4 interfaces, one entry per interface name.
pub fn list() -> Vec<Iface> {
    let mut out: Vec<Iface> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter(|i| i.addr.ip().is_ipv4())
        .map(|i| Iface {
            name: i.name,
            ip: i.addr.ip(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
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
