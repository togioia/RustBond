//! RustBond wire protocol and shared configuration types.
//!
//! Every message is a frame:
//!
//! ```text
//! [stream_id: u32 BE][kind: u8][payload_len: u32 BE][payload]
//! ```
//!
//! Stream `0` is the control stream (auth, ping). The client allocates stream
//! ids; the server only echoes them back.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Hard cap on a single frame payload. UDP datagrams are smaller anyway.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

pub mod kind {
    pub const AUTH: u8 = 0x01;
    pub const AUTH_OK: u8 = 0x02;
    pub const AUTH_ERR: u8 = 0x03;

    pub const TCP_OPEN: u8 = 0x04;
    pub const TCP_OPEN_OK: u8 = 0x05;
    pub const TCP_OPEN_ERR: u8 = 0x06;
    pub const TCP_DATA: u8 = 0x07;
    pub const TCP_CLOSE: u8 = 0x08;

    pub const UDP_BIND: u8 = 0x09;
    pub const UDP_DATA: u8 = 0x0a;
    pub const UDP_CLOSE: u8 = 0x0d;

    pub const PING: u8 = 0x0b;
    pub const PONG: u8 = 0x0c;
}

#[derive(Debug)]
pub struct Frame {
    pub stream_id: u32,
    pub kind: u8,
    pub payload: Vec<u8>,
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Frame> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr).await?;
    let stream_id = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let kind = hdr[4];
    let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
    if len > MAX_FRAME {
        return Err(anyhow!("frame too large: {len}"));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Frame {
        stream_id,
        kind,
        payload,
    })
}

pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    stream_id: u32,
    kind: u8,
    payload: &[u8],
) -> Result<()> {
    let mut hdr = [0u8; 9];
    hdr[0..4].copy_from_slice(&stream_id.to_be_bytes());
    hdr[4] = kind;
    hdr[5..9].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    w.flush().await?;
    Ok(())
}

pub fn route_payload(route_id: u16) -> Vec<u8> {
    route_id.to_be_bytes().to_vec()
}

pub fn parse_route(payload: &[u8]) -> Result<u16> {
    if payload.len() != 2 {
        return Err(anyhow!("route payload must be 2 bytes"));
    }
    Ok(u16::from_be_bytes([payload[0], payload[1]]))
}

// ---------------------------------------------------------------------------
// Configuration (shared file format between client and server)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

/// One forwarded port.
///
/// `id` must be unique and stable: it is the identifier on the wire.
/// `local_port` is only used by the client; `targets` only by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteCfg {
    pub id: u16,
    pub proto: Proto,
    #[serde(default)]
    pub local_port: u16,
    #[serde(default)]
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server_addr: String,
    pub key: String,
    /// Interfaces used by the macOS client to reach the server (bonding).
    /// Empty = let the OS choose the route.
    #[serde(default)]
    pub interfaces: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RouteCfg>,
}

impl AppConfig {
    pub fn example() -> Self {
        Self {
            server_addr: "127.0.0.1:5000".into(),
            key: "change-me-please".into(),
            interfaces: vec![],
            routes: vec![
                RouteCfg {
                    id: 1,
                    proto: Proto::Tcp,
                    local_port: 8888,
                    targets: vec!["10.11.13.5:5432".into(), "10.11.13.1:5432".into()],
                },
                RouteCfg {
                    id: 2,
                    proto: Proto::Udp,
                    local_port: 8889,
                    targets: vec!["10.11.13.5:5353".into()],
                },
            ],
        }
    }
}

/// Constant-time-ish comparison for the PSK. Good enough for v1 (no TLS).
pub fn key_ok(provided: &[u8], expected: &[u8]) -> bool {
    if provided.len() != expected.len() || provided.is_empty() {
        return false;
    }
    let mut acc = 0u8;
    for (a, b) in provided.iter().zip(expected.iter()) {
        acc |= a ^ b;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(1024);
            write_frame(&mut a, 7, kind::TCP_DATA, b"hello").await.unwrap();
            let f = read_frame(&mut b).await.unwrap();
            assert_eq!(f.stream_id, 7);
            assert_eq!(f.kind, kind::TCP_DATA);
            assert_eq!(f.payload, b"hello");
        });
    }

    #[test]
    fn keys_match() {
        assert!(key_ok(b"secret", b"secret"));
        assert!(!key_ok(b"secreT", b"secret"));
        assert!(!key_ok(b"", b""));
        assert!(!key_ok(b"short", b"longer"));
    }
}
