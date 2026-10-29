//! Multiplexing engine.
//!
//! One transport per selected interface; every local TCP/UDP stream is pinned
//! to a transport chosen round-robin. This gives both aggregation (streams are
//! spread over the links) and failover (a dead link is reconnected by its own
//! supervisor while the others keep working).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use rnb_shared::{dest_payload, kind, read_frame, route_payload, write_frame, AppConfig, Frame, Proto};
use tokio::{
    io::{split, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};

use crate::net;

pub type WriterTx = mpsc::UnboundedSender<(u32, u8, Vec<u8>)>;
pub type StatusHandle = Arc<StdMutex<String>>;

pub fn new_status() -> StatusHandle {
    Arc::new(StdMutex::new("idle".to_string()))
}

// ---------------------------------------------------------------------------
// Per-link traffic counters (shown in the GUI)
// ---------------------------------------------------------------------------

/// Live counters for one transport (one interface).
#[derive(Default)]
pub struct LinkStat {
    pub label: String,
    pub tx: AtomicU64,
    pub rx: AtomicU64,
    pub connected: AtomicBool,
}

#[derive(Clone, Debug, Default)]
pub struct LinkSnapshot {
    pub label: String,
    pub connected: bool,
    pub tx: u64,
    pub rx: u64,
}

/// Shared list of [`LinkStat`]s between the engine and the UI. Entries are
/// keyed by interface label and survive stop/start, so the totals stay
/// cumulative until the user resets them.
#[derive(Clone, Default)]
pub struct StatsHandle(Arc<StdMutex<Vec<Arc<LinkStat>>>>);

impl StatsHandle {
    pub fn snapshot(&self) -> Vec<LinkSnapshot> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| LinkSnapshot {
                label: s.label.clone(),
                connected: s.connected.load(Ordering::Relaxed),
                tx: s.tx.load(Ordering::Relaxed),
                rx: s.rx.load(Ordering::Relaxed),
            })
            .collect()
    }

    pub fn reset(&self) {
        for s in self.0.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            s.tx.store(0, Ordering::Relaxed);
            s.rx.store(0, Ordering::Relaxed);
        }
    }

    /// Mark every known link as disconnected (called when the engine stops).
    pub fn disconnect_all(&self) {
        for s in self.0.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            s.connected.store(false, Ordering::Relaxed);
        }
    }

    fn entry(&self, label: &str) -> Arc<LinkStat> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = guard.iter().find(|s| s.label == label) {
            return found.clone();
        }
        let stat = Arc::new(LinkStat {
            label: label.to_string(),
            ..Default::default()
        });
        guard.push(stat.clone());
        stat
    }
}

fn set_status(handle: &StatusHandle, msg: impl Into<String>) {
    if let Ok(mut g) = handle.lock() {
        *g = msg.into();
    }
}

struct Shared {
    /// Pending open/bind acknowledgements.
    open_ack: Mutex<HashMap<u32, oneshot::Sender<bool>>>,
    /// Frames from the server, per stream.
    tcp_in: Mutex<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>,
    /// Last local UDP peer seen for a stream.
    udp_peer: Mutex<HashMap<u32, SocketAddr>>,
    /// Local UDP socket for a stream.
    udp_sock: Mutex<HashMap<u32, Arc<UdpSocket>>>,
}

struct Slot {
    label: String,
    local_ip: Option<IpAddr>,
    tx: Mutex<Option<WriterTx>>,
    stat: Arc<LinkStat>,
}

#[derive(Clone)]
struct Pool {
    slots: Arc<Vec<Arc<Slot>>>,
    rr: Arc<AtomicUsize>,
}

impl Pool {
    fn len(&self) -> usize {
        self.slots.len()
    }

    fn next(&self) -> usize {
        self.rr.fetch_add(1, Ordering::Relaxed) % self.len()
    }

    async fn send_on(&self, idx: usize, stream_id: u32, kind: u8, payload: Vec<u8>) -> bool {
        let slot = &self.slots[idx % self.len()];
        let mut guard = slot.tx.lock().await;
        if let Some(tx) = guard.as_ref() {
            if !tx.is_closed() && tx.send((stream_id, kind, payload)).is_ok() {
                return true;
            }
        }
        *guard = None;
        false
    }

    async fn send_any(&self, stream_id: u32, kind: u8, payload: Vec<u8>) -> bool {
        let n = self.len();
        let start = self.next();
        for step in 0..n {
            if self
                .send_on((start + step) % n, stream_id, kind, payload.clone())
                .await
            {
                return true;
            }
        }
        false
    }
}

async fn open_transport(
    server: &str,
    key: &str,
    local_ip: Option<IpAddr>,
    shared: Arc<Shared>,
    stat: Arc<LinkStat>,
) -> Result<WriterTx> {
    let stream = net::connect(server, local_ip).await?;
    let (mut rh, mut wh) = split(stream);
    write_frame(&mut wh, 0, kind::AUTH, key.as_bytes()).await?;
    let reply = timeout(Duration::from_secs(5), read_frame(&mut rh))
        .await
        .map_err(|_| anyhow!("auth timeout"))??;
    if reply.kind != kind::AUTH_OK {
        return Err(anyhow!(
            "auth rejected: {}",
            String::from_utf8_lossy(&reply.payload)
        ));
    }

    stat.connected.store(true, Ordering::Relaxed);

    let (tx, mut rx) = mpsc::unbounded_channel::<(u32, u8, Vec<u8>)>();
    let tx_stat = stat.clone();
    tokio::spawn(async move {
        while let Some((stream_id, kind, payload)) = rx.recv().await {
            if write_frame(&mut wh, stream_id, kind, &payload).await.is_err() {
                break;
            }
            tx_stat
                .tx
                .fetch_add(payload.len() as u64 + 9, Ordering::Relaxed);
        }
        tx_stat.connected.store(false, Ordering::Relaxed);
    });

    let rx_stat = stat;
    tokio::spawn(async move {
        loop {
            match read_frame(&mut rh).await {
                Ok(frame) => {
                    rx_stat
                        .rx
                        .fetch_add(frame.payload.len() as u64 + 9, Ordering::Relaxed);
                    dispatch(frame, &shared).await;
                }
                Err(_) => break,
            }
        }
        rx_stat.connected.store(false, Ordering::Relaxed);
    });

    Ok(tx)
}

async fn dispatch(frame: Frame, shared: &Arc<Shared>) {
    match frame.kind {
        kind::TCP_OPEN_OK => {
            if let Some(tx) = shared.open_ack.lock().await.remove(&frame.stream_id) {
                let _ = tx.send(true);
            }
        }
        kind::TCP_OPEN_ERR => {
            eprintln!(
                "stream {} open error: {}",
                frame.stream_id,
                String::from_utf8_lossy(&frame.payload)
            );
            if let Some(tx) = shared.open_ack.lock().await.remove(&frame.stream_id) {
                let _ = tx.send(false);
            }
        }
        kind::TCP_DATA => {
            if let Some(tx) = shared.tcp_in.lock().await.get(&frame.stream_id) {
                let _ = tx.send(frame.payload);
            }
        }
        kind::TCP_CLOSE | kind::UDP_CLOSE => {
            shared.tcp_in.lock().await.remove(&frame.stream_id);
            shared.open_ack.lock().await.remove(&frame.stream_id);
        }
        kind::UDP_DATA => {
            let sock = shared.udp_sock.lock().await.get(&frame.stream_id).cloned();
            let peer = shared.udp_peer.lock().await.get(&frame.stream_id).copied();
            if let (Some(sock), Some(peer)) = (sock, peer) {
                let _ = sock.send_to(&frame.payload, peer).await;
            }
        }
        _ => {}
    }
}

fn spawn_supervisors(
    pool: &Pool,
    server: String,
    key: String,
    shared: Arc<Shared>,
    status: StatusHandle,
) {
    for slot in pool.slots.iter() {
        let slot = slot.clone();
        let server = server.clone();
        let key = key.clone();
        let shared = shared.clone();
        let status = status.clone();
        tokio::spawn(async move {
            loop {
                let alive = {
                    let guard = slot.tx.lock().await;
                    guard
                        .as_ref()
                        .map(|tx| !tx.is_closed())
                        .unwrap_or(false)
                };
                if !alive {
                    *slot.tx.lock().await = None;
                    match open_transport(
                        &server,
                        &key,
                        slot.local_ip,
                        shared.clone(),
                        slot.stat.clone(),
                    )
                    .await
                    {
                        Ok(tx) => {
                            *slot.tx.lock().await = Some(tx);
                            eprintln!("[{}] connected to {server}", slot.label);
                            set_status(&status, format!("connected [{}]", slot.label));
                        }
                        Err(e) => {
                            eprintln!("[{}] {e:#}", slot.label);
                            set_status(&status, format!("[{}] {e:#}", slot.label));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

/// Open a stream on `idx`, or on any other transport if that one is dead.
async fn open_stream(
    pool: &Pool,
    shared: &Arc<Shared>,
    stream_id: u32,
    kind_open: u8,
    payload: Vec<u8>,
) -> Option<usize> {
    let n = pool.len();
    let start = pool.next();
    for step in 0..n {
        let idx = (start + step) % n;
        let (ack_tx, ack_rx) = oneshot::channel();
        shared.open_ack.lock().await.insert(stream_id, ack_tx);
        if !pool
            .send_on(idx, stream_id, kind_open, payload.clone())
            .await
        {
            shared.open_ack.lock().await.remove(&stream_id);
            continue;
        }
        match timeout(Duration::from_secs(5), ack_rx).await {
            Ok(Ok(true)) => return Some(idx),
            // The server explicitly rejected this stream (bad route, allowlist,
            // target down): retrying on another transport would just duplicate
            // the request, so give up here.
            Ok(Ok(false)) => return None,
            _ => {
                shared.open_ack.lock().await.remove(&stream_id);
            }
        }
    }
    None
}

async fn serve_tcp(
    route: rnb_shared::RouteCfg,
    pool: Pool,
    shared: Arc<Shared>,
    next_sid: Arc<AtomicU32>,
    status: StatusHandle,
) -> Result<()> {
    let addr = format!("127.0.0.1:{}", route.local_port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow!("cannot listen on {addr}: {e}"))?;
    eprintln!(
        "route {} tcp  {addr} -> {:?}",
        route.id, route.targets
    );
    set_status(&status, format!("listening {addr}"));

    loop {
        let (local, peer) = listener.accept().await?;
        local.set_nodelay(true).ok();
        let pool = pool.clone();
        let shared = shared.clone();
        let status = status.clone();
        let stream_id = next_sid.fetch_add(2, Ordering::Relaxed);
        let route_id = route.id;

        tokio::spawn(async move {
            // Register the inbound channel BEFORE opening the stream: the target
            // may answer immediately (TLS alert, HTTP 404) and those first
            // frames would otherwise be dropped.
            let (dtx, mut drx) = mpsc::unbounded_channel::<Vec<u8>>();
            shared.tcp_in.lock().await.insert(stream_id, dtx);

            let Some(idx) = open_stream(&pool, &shared, stream_id, kind::TCP_OPEN, route_payload(route_id)).await
            else {
                shared.tcp_in.lock().await.remove(&stream_id);
                eprintln!("stream {stream_id} ({peer}): no transport available");
                set_status(&status, "no transport available".to_string());
                return;
            };

            let (mut lr, mut lw) = split(local);

            // local -> transport
            let up = {
                let pool = pool.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 32 * 1024];
                    loop {
                        match lr.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if !pool
                                    .send_on(idx, stream_id, kind::TCP_DATA, buf[..n].to_vec())
                                    .await
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = pool.send_on(idx, stream_id, kind::TCP_CLOSE, Vec::new()).await;
                })
            };

            // transport -> local
            let down = tokio::spawn(async move {
                while let Some(chunk) = drx.recv().await {
                    if lw.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                let _ = lw.shutdown().await;
            });

            let _ = tokio::join!(up, down);
            shared.tcp_in.lock().await.remove(&stream_id);
        });
    }
}

async fn serve_udp(
    route: rnb_shared::RouteCfg,
    pool: Pool,
    shared: Arc<Shared>,
    status: StatusHandle,
) -> Result<()> {
    let addr = format!("127.0.0.1:{}", route.local_port);
    let sock = Arc::new(UdpSocket::bind(&addr).await?);
    let stream_id = 0x4000_0000 | route.id as u32;
    eprintln!("route {} udp  {addr} -> {:?}", route.id, route.targets);
    set_status(&status, format!("listening {addr}/udp"));

    shared.udp_sock.lock().await.insert(stream_id, sock.clone());

    // Retry until a transport accepts the bind: at startup the supervisor may
    // still be connecting, and returning here would leave UDP dead forever.
    let mut bound_on = loop {
        match open_stream(&pool, &shared, stream_id, kind::UDP_BIND, route_payload(route.id)).await {
            Some(idx) => break idx,
            None => {
                set_status(&status, format!("udp route {}: waiting for transport", route.id));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };

    let mut buf = vec![0u8; 65535];
    loop {
        let (n, peer) = sock.recv_from(&mut buf).await?;
        shared.udp_peer.lock().await.insert(stream_id, peer);
        if !pool
            .send_on(bound_on, stream_id, kind::UDP_DATA, buf[..n].to_vec())
            .await
        {
            // Transport died: the server-side UDP socket is gone too, rebind.
            set_status(&status, format!("udp route {}: rebinding", route.id));
            match open_stream(&pool, &shared, stream_id, kind::UDP_BIND, route_payload(route.id)).await {
                Some(idx) => bound_on = idx,
                None => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

/// Build the transport pool from the configured interfaces.
fn build_slots(interfaces: &[String], stats: &StatsHandle) -> Vec<Arc<Slot>> {
    let available = net::list();
    let mut slots = Vec::new();

    let add = |slots: &mut Vec<Arc<Slot>>, label: String, local_ip: Option<IpAddr>| {
        slots.push(Arc::new(Slot {
            stat: stats.entry(&label),
            label,
            local_ip,
            tx: Mutex::new(None),
        }));
    };

    if interfaces.is_empty() {
        add(&mut slots, "auto".into(), None);
        return slots;
    }

    for name in interfaces {
        match available.iter().find(|i| &i.name == name) {
            Some(iface) => {
                let Some(ip) = iface.ip else {
                    eprintln!("interface {name} has no IPv4 address, skipping");
                    continue;
                };
                add(&mut slots, iface.display(), Some(ip));
            }
            None => eprintln!("interface {name} not found, skipping"),
        }
    }

    if slots.is_empty() {
        eprintln!("none of the selected interfaces exists, falling back to auto");
        add(&mut slots, "auto".into(), None);
    }
    slots
}

/// Run the engine until the future is dropped (i.e. the task is aborted).
pub async fn run(cfg: AppConfig, status: StatusHandle, stats: StatsHandle) -> Result<()> {
    if cfg.key.is_empty() {
        return Err(anyhow!("empty key"));
    }
    if cfg.routes.is_empty() && cfg.proxy_port.is_none() {
        return Err(anyhow!("no routes and no proxy_port configured"));
    }

    // Keep the cumulative totals, but no link is connected until a transport
    // comes up again.
    stats.disconnect_all();

    let shared = Arc::new(Shared {
        open_ack: Mutex::new(HashMap::new()),
        tcp_in: Mutex::new(HashMap::new()),
        udp_peer: Mutex::new(HashMap::new()),
        udp_sock: Mutex::new(HashMap::new()),
    });

    let pool = Pool {
        slots: Arc::new(build_slots(&cfg.interfaces, &stats)),
        rr: Arc::new(AtomicUsize::new(0)),
    };
    eprintln!(
        "engine start: server={} transports={} routes={}",
        cfg.server_addr,
        pool.len(),
        cfg.routes.len()
    );

    spawn_supervisors(
        &pool,
        cfg.server_addr.clone(),
        cfg.key.clone(),
        shared.clone(),
        status.clone(),
    );

    // Wait for the first transport before binding local listeners: UDP routes
    // bind only once (unlike TCP, which retries per accepted connection).
    {
        let mut waited = 0u64;
        while waited < 15_000 {
            let mut any = false;
            for slot in pool.slots.iter() {
                if slot.tx.lock().await.as_ref().map(|t| !t.is_closed()) == Some(true) {
                    any = true;
                }
            }
            if any {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            waited += 250;
        }
        if waited >= 15_000 {
            set_status(&status, "waiting for server...".to_string());
        }
    }

    let next_sid = Arc::new(AtomicU32::new(1));

    for route in cfg.routes.clone() {
        let pool = pool.clone();
        let shared = shared.clone();
        let next_sid = next_sid.clone();
        let status = status.clone();
        match route.proto {
            Proto::Tcp => {
                tokio::spawn(async move {
                    if let Err(e) = serve_tcp(route, pool, shared, next_sid, status.clone()).await {
                        eprintln!("tcp route error: {e:#}");
                        set_status(&status, format!("tcp error: {e:#}"));
                    }
                });
            }
            Proto::Udp => {
                tokio::spawn(async move {
                    if let Err(e) = serve_udp(route, pool, shared, status.clone()).await {
                        eprintln!("udp route error: {e:#}");
                        set_status(&status, format!("udp error: {e:#}"));
                    }
                });
            }
        }
    }

    if let Some(port) = cfg.proxy_port {
        let pool = pool.clone();
        let shared = shared.clone();
        let next_sid = next_sid.clone();
        let status = status.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_socks(port, pool, shared, next_sid, status.clone()).await {
                eprintln!("socks error: {e:#}");
                set_status(&status, format!("socks error: {e:#}"));
            }
        });
    }

    // Keep-alive on the control stream.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                pool.send_any(0, kind::PING, Vec::new()).await;
            }
        });
    }

    set_status(&status, "running".to_string());
    std::future::pending::<()>().await;
    Ok(())
}

/// Minimal SOCKS5 proxy (CONNECT only, no auth) on 127.0.0.1.
async fn serve_socks(
    port: u16,
    pool: Pool,
    shared: Arc<Shared>,
    next_sid: Arc<AtomicU32>,
    status: StatusHandle,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    eprintln!("socks5 in ascolto su 127.0.0.1:{port}");
    set_status(&status, format!("socks5 127.0.0.1:{port}"));

    loop {
        let (local, peer) = listener.accept().await?;
        local.set_nodelay(true).ok();
        let pool = pool.clone();
        let shared = shared.clone();
        let next_sid = next_sid.clone();
        tokio::spawn(async move {
            if let Err(e) = socks_conn(local, pool, shared, next_sid).await {
                eprintln!("socks {peer}: {e:#}");
            }
        });
    }
}

async fn socks_conn(
    mut local: TcpStream,
    pool: Pool,
    shared: Arc<Shared>,
    next_sid: Arc<AtomicU32>,
) -> Result<()> {
    // greeting
    let mut hdr = [0u8; 2];
    local.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 {
        bail!("not a SOCKS5 client");
    }
    let mut methods = vec![0u8; hdr[1] as usize];
    local.read_exact(&mut methods).await?;
    local.write_all(&[0x05, 0x00]).await?; // no auth

    // request
    let mut req = [0u8; 4];
    local.read_exact(&mut req).await?;
    if req[0] != 0x05 || req[1] != 0x01 {
        local
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .ok();
        bail!("only CONNECT is supported");
    }

    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            local.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            local.read_exact(&mut len).await?;
            let mut host = vec![0u8; len[0] as usize];
            local.read_exact(&mut host).await?;
            String::from_utf8(host)?
        }
        0x04 => {
            let mut a = [0u8; 16];
            local.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            local
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            bail!("unsupported address type");
        }
    };
    let mut port_bytes = [0u8; 2];
    local.read_exact(&mut port_bytes).await?;
    let port = u16::from_be_bytes(port_bytes);

    let stream_id = next_sid.fetch_add(2, Ordering::Relaxed);
    let (dtx, mut drx) = mpsc::unbounded_channel::<Vec<u8>>();
    shared.tcp_in.lock().await.insert(stream_id, dtx);

    let Some(idx) = open_stream(
        &pool,
        &shared,
        stream_id,
        kind::TCP_OPEN_TO,
        dest_payload(&host, port),
    )
    .await
    else {
        shared.tcp_in.lock().await.remove(&stream_id);
        local
            .write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .ok();
        bail!("cannot open {host}:{port}");
    };

    local
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    let (mut lr, mut lw) = split(local);
    let up = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match lr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if !pool
                            .send_on(idx, stream_id, kind::TCP_DATA, buf[..n].to_vec())
                            .await
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = pool.send_on(idx, stream_id, kind::TCP_CLOSE, Vec::new()).await;
        })
    };
    let down = tokio::spawn(async move {
        while let Some(chunk) = drx.recv().await {
            if lw.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = lw.shutdown().await;
    });
    let _ = tokio::join!(up, down);
    shared.tcp_in.lock().await.remove(&stream_id);
    Ok(())
}
