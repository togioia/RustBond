//! RustBond relay server.
//!
//! Listens for a single multiplexed TCP session per client transport,
//! authenticates it with a pre-shared key and relays each stream to one of the
//! configured targets (active/passive fallback).

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use rnb_shared::{
    key_ok, kind, parse_dest, parse_route, read_frame, write_frame, AppConfig, Frame, Proto,
};
use tokio::{
    io::{split, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    time::timeout,
};

type WriterTx = mpsc::UnboundedSender<(u32, u8, Vec<u8>)>;

struct Route {
    id: u16,
    proto: Proto,
    targets: Vec<String>,
    /// Index of the currently preferred target (fallback counter).
    active: AtomicUsize,
}

impl Route {
    fn target(&self) -> Option<&str> {
        let n = self.targets.len();
        if n == 0 {
            None
        } else {
            Some(&self.targets[self.active.load(Ordering::Relaxed) % n])
        }
    }

    fn advance(&self) {
        let n = self.targets.len();
        if n > 1 {
            self.active.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("rustbond-server {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let listen = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:5000".into());
    let cfg_path = std::env::var("CONFIG").unwrap_or_else(|_| "/etc/rustbond/config.toml".into());

    let cfg = load_config(&cfg_path);
    let key = match std::env::var("RNB_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ if !cfg.key.is_empty() => cfg.key.clone(),
        _ => {
            eprintln!("no key configured: set RNB_KEY or \"key\" in {cfg_path}");
            std::process::exit(2);
        }
    };

    let routes = Arc::new(build_routes(&cfg));
    eprintln!(
        "rustbond-server {} | listen={} | routes={} | config={}",
        env!("CARGO_PKG_VERSION"),
        listen,
        routes.len(),
        cfg_path
    );

    spawn_healthcheck(routes.clone());

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;

    loop {
        let (sock, peer) = listener.accept().await?;
        let routes = routes.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(sock, peer, key, routes).await {
                eprintln!("[{peer}] connection closed: {e:#}");
            }
        });
    }
}

fn load_config(path: &str) -> AppConfig {
    match std::fs::read_to_string(path) {
        Ok(txt) => match toml::from_str::<AppConfig>(&txt) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("invalid config {path}: {e}");
                std::process::exit(3);
            }
        },
        Err(e) => {
            eprintln!("cannot read config {path}: {e} (starting with no routes)");
            AppConfig {
                server_addr: String::new(),
                key: String::new(),
                interfaces: Vec::new(),
                proxy_port: None,
                routes: Vec::new(),
            }
        }
    }
}

fn build_routes(cfg: &AppConfig) -> HashMap<u16, Arc<Route>> {
    let mut out = HashMap::new();
    for r in &cfg.routes {
        if out.contains_key(&r.id) {
            eprintln!("duplicated route id {}, ignoring", r.id);
            continue;
        }
        out.insert(
            r.id,
            Arc::new(Route {
                id: r.id,
                proto: r.proto,
                targets: r.targets.clone(),
                active: AtomicUsize::new(0),
            }),
        );
    }
    out
}

/// Keeps the active target honest: every few seconds we probe it and move to
/// the next one when it stops accepting TCP connections.
fn spawn_healthcheck(routes: Arc<HashMap<u16, Arc<Route>>>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            for route in routes.values() {
                let Some(target) = route.target() else { continue };
                let target = target.to_string();
                let ok = match route.proto {
                    Proto::Tcp => timeout(Duration::from_secs(2), TcpStream::connect(&target))
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false),
                    // UDP has no handshake: best effort, keep the target as is.
                    Proto::Udp => true,
                };
                if !ok {
                    eprintln!("[route {}] target {target} unreachable, switching", route.id);
                    route.advance();
                }
            }
        }
    });
}

async fn dial(route: &Route) -> Result<(TcpStream, String)> {
    let n = route.targets.len();
    if n == 0 {
        anyhow::bail!("route {} has no targets", route.id);
    }
    let start = route.active.load(Ordering::Relaxed) % n;
    let mut last_err = anyhow::anyhow!("no target reachable");
    for step in 0..n {
        let idx = (start + step) % n;
        let target = route.targets[idx].clone();
        match timeout(Duration::from_secs(3), TcpStream::connect(&target)).await {
            Ok(Ok(stream)) => {
                route.active.store(idx, Ordering::Relaxed);
                return Ok((stream, target));
            }
            Ok(Err(e)) => last_err = anyhow::anyhow!("{target}: {e}"),
            Err(_) => last_err = anyhow::anyhow!("{target}: connect timeout"),
        }
    }
    Err(last_err)
}

fn allowlist() -> Vec<String> {
    std::env::var("RNB_ALLOW")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn dest_allowed(allow: &[String], host: &str) -> bool {
    if allow.iter().any(|a| a == "*") {
        return true;
    }
    let host = host.to_ascii_lowercase();
    allow
        .iter()
        .any(|a| host == *a || host.ends_with(&format!(".{a}")))
}

async fn dial_host(host: &str, port: u16) -> Result<(TcpStream, String)> {
    let target = format!("{host}:{port}");
    match timeout(Duration::from_secs(5), TcpStream::connect(&target)).await {
        Ok(Ok(stream)) => Ok((stream, target)),
        Ok(Err(e)) => Err(anyhow::anyhow!("{target}: {e}")),
        Err(_) => Err(anyhow::anyhow!("{target}: connect timeout")),
    }
}

async fn handle_client(
    sock: TcpStream,
    peer: SocketAddr,
    key: String,
    routes: Arc<HashMap<u16, Arc<Route>>>,
) -> Result<()> {
    sock.set_nodelay(true).ok();
    let (rh, wh) = split(sock);
    let mut rh = rh;
    let mut wh = wh;

    // --- handshake ---------------------------------------------------------
    let first = read_frame(&mut rh).await?;
    if first.stream_id != 0 || first.kind != kind::AUTH || !key_ok(&first.payload, key.as_bytes()) {
        write_frame(&mut wh, 0, kind::AUTH_ERR, b"invalid key").await.ok();
        anyhow::bail!("authentication failed");
    }
    write_frame(&mut wh, 0, kind::AUTH_OK, &[]).await?;
    eprintln!("[{peer}] authenticated");

    // Single writer task: every stream funnels frames through this channel so
    // that frames never interleave on the wire.
    let (wtx, mut wrx): (WriterTx, mpsc::UnboundedReceiver<(u32, u8, Vec<u8>)>) =
        mpsc::unbounded_channel();
    let writer = tokio::spawn(async move {
        while let Some((stream_id, kind, payload)) = wrx.recv().await {
            if write_frame(&mut wh, stream_id, kind, &payload).await.is_err() {
                break;
            }
        }
    });

    let mut tcp_sinks: HashMap<u32, mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut udp_sinks: HashMap<u32, mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut tasks: HashMap<u32, Vec<tokio::task::AbortHandle>> = HashMap::new();

    loop {
        let frame = match read_frame(&mut rh).await {
            Ok(f) => f,
            Err(_) => break,
        };

        match frame.kind {
            kind::PING => {
                let _ = wtx.send((0, kind::PONG, Vec::new()));
            }
            kind::TCP_OPEN => {
                let route_id = parse_route(&frame.payload)?;
                let stream_id = frame.stream_id;
                let Some(route) = routes.get(&route_id).cloned() else {
                    let _ = wtx.send((stream_id, kind::TCP_OPEN_ERR, b"unknown route".to_vec()));
                    continue;
                };
                if route.proto != Proto::Tcp {
                    let _ = wtx.send((stream_id, kind::TCP_OPEN_ERR, b"route is not tcp".to_vec()));
                    continue;
                }
                match dial(&route).await {
                    Ok((target, addr)) => {
                        eprintln!("[{peer}] stream {stream_id} -> {addr}");
                        let _ = wtx.send((stream_id, kind::TCP_OPEN_OK, Vec::new()));
                        tasks.insert(stream_id, spawn_tcp_relay(stream_id, target, wtx.clone(), &mut tcp_sinks));
                    }
                    Err(e) => {
                        let _ = wtx.send((
                            stream_id,
                            kind::TCP_OPEN_ERR,
                            format!("{e:#}").into_bytes(),
                        ));
                    }
                }
            }
            kind::TCP_DATA => {
                if let Some(tx) = tcp_sinks.get(&stream_id_of(&frame)) {
                    let _ = tx.send(frame.payload);
                }
            }
            kind::TCP_CLOSE => {
                close_stream(&mut tasks, &mut tcp_sinks, &mut udp_sinks, frame.stream_id);
            }
            kind::TCP_OPEN_TO => {
                let (host, port) = parse_dest(&frame.payload)?;
                let stream_id = frame.stream_id;
                let allow = allowlist();
                if !dest_allowed(&allow, &host) {
                    eprintln!("[{peer}] stream {stream_id}: destination {host} not allowed");
                    let _ = wtx.send((
                        stream_id,
                        kind::TCP_OPEN_ERR,
                        b"destination not allowed".to_vec(),
                    ));
                    continue;
                }
                match dial_host(&host, port).await {
                    Ok((target, addr)) => {
                        eprintln!("[{peer}] stream {stream_id} -> {addr} (socks)");
                        let _ = wtx.send((stream_id, kind::TCP_OPEN_OK, Vec::new()));
                        tasks.insert(
                            stream_id,
                            spawn_tcp_relay(stream_id, target, wtx.clone(), &mut tcp_sinks),
                        );
                    }
                    Err(e) => {
                        let _ = wtx.send((
                            stream_id,
                            kind::TCP_OPEN_ERR,
                            format!("{e:#}").into_bytes(),
                        ));
                    }
                }
            }
            kind::UDP_BIND => {
                let route_id = parse_route(&frame.payload)?;
                let stream_id = frame.stream_id;
                let Some(route) = routes.get(&route_id).cloned() else {
                    let _ = wtx.send((stream_id, kind::TCP_OPEN_ERR, b"unknown route".to_vec()));
                    continue;
                };
                let Some(target) = route.target().map(str::to_string) else {
                    let _ = wtx.send((stream_id, kind::TCP_OPEN_ERR, b"route has no targets".to_vec()));
                    continue;
                };
                match bind_udp(&target).await {
                    Ok(sock) => {
                        let _ = wtx.send((stream_id, kind::TCP_OPEN_OK, Vec::new()));
                        tasks.insert(
                            stream_id,
                            spawn_udp_relay(stream_id, sock, wtx.clone(), &mut udp_sinks),
                        );
                    }
                    Err(e) => {
                        let _ = wtx.send((
                            stream_id,
                            kind::TCP_OPEN_ERR,
                            format!("{e:#}").into_bytes(),
                        ));
                    }
                }
            }
            kind::UDP_DATA => {
                if let Some(tx) = udp_sinks.get(&frame.stream_id) {
                    let _ = tx.send(frame.payload);
                }
            }
            kind::UDP_CLOSE => {
                close_stream(&mut tasks, &mut tcp_sinks, &mut udp_sinks, frame.stream_id);
            }
            other => eprintln!("[{peer}] unknown frame 0x{other:02x}"),
        }
    }

    for handles in tasks.into_values() {
        for h in handles {
            h.abort();
        }
    }
    writer.abort();
    eprintln!("[{peer}] disconnected");
    Ok(())
}

fn stream_id_of(frame: &Frame) -> u32 {
    frame.stream_id
}

fn close_stream(
    tasks: &mut HashMap<u32, Vec<tokio::task::AbortHandle>>,
    tcp_sinks: &mut HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
    udp_sinks: &mut HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
    stream_id: u32,
) {
    if let Some(handles) = tasks.remove(&stream_id) {
        for h in handles {
            h.abort();
        }
    }
    tcp_sinks.remove(&stream_id);
    udp_sinks.remove(&stream_id);
}

fn spawn_tcp_relay(
    stream_id: u32,
    target: TcpStream,
    wtx: WriterTx,
    sinks: &mut HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
) -> Vec<tokio::task::AbortHandle> {
    let (mut tr, mut tw) = split(target);
    let (stx, mut srx) = mpsc::unbounded_channel::<Vec<u8>>();
    sinks.insert(stream_id, stx);

    // target -> client
    let up = {
        let wtx = wtx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match tr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if wtx
                            .send((stream_id, kind::TCP_DATA, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = wtx.send((stream_id, kind::TCP_CLOSE, Vec::new()));
        })
    };

    // client -> target
    let down = tokio::spawn(async move {
        while let Some(chunk) = srx.recv().await {
            if tw.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = tw.shutdown().await;
    });

    vec![up.abort_handle(), down.abort_handle()]
}

async fn bind_udp(target: &str) -> Result<UdpSocket> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect(target).await?;
    Ok(sock)
}

fn spawn_udp_relay(
    stream_id: u32,
    sock: UdpSocket,
    wtx: WriterTx,
    sinks: &mut HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
) -> Vec<tokio::task::AbortHandle> {
    let sock = Arc::new(sock);
    let (stx, mut srx) = mpsc::unbounded_channel::<Vec<u8>>();
    sinks.insert(stream_id, stx);

    // target -> client
    let up = {
        let sock = sock.clone();
        let wtx = wtx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match sock.recv(&mut buf).await {
                    Ok(n) => {
                        if wtx
                            .send((stream_id, kind::UDP_DATA, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = wtx.send((stream_id, kind::UDP_CLOSE, Vec::new()));
        })
    };

    // client -> target
    let down = tokio::spawn(async move {
        while let Some(datagram) = srx.recv().await {
            if sock.send(&datagram).await.is_err() {
                break;
            }
        }
    });

    vec![up.abort_handle(), down.abort_handle()]
}
