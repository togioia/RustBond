//! RustBond — macOS menu bar client for a bonded TCP/UDP tunnel.

mod config;
mod engine;
mod net;
mod sysproxy;

#[cfg(feature = "gui")]
mod gui;

use anyhow::Result;

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let cfg = config::load();
    let headless = std::env::args().any(|a| a == "--headless");

    #[cfg(feature = "gui")]
    {
        if !headless {
            return gui::run(rt, cfg);
        }
    }
    let _ = &headless;

    eprintln!(
        "rustbond {} headless: server={} transports={} routes={}",
        env!("CARGO_PKG_VERSION"),
        cfg.server_addr,
        if cfg.interfaces.is_empty() {
            "auto".to_string()
        } else {
            cfg.interfaces.join(",")
        },
        cfg.routes.len()
    );

    // Headless behaves like the GUI: the system proxy follows the tunnel.
    let _proxy = if cfg.auto_proxy {
        match cfg.proxy_port {
            Some(port) => match sysproxy::enable("127.0.0.1", port) {
                Ok(g) => Some(g),
                Err(e) => {
                    eprintln!("system proxy: {e:#}");
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    rt.block_on(engine::run(
        cfg,
        engine::new_status(),
        engine::StatsHandle::default(),
    ))
}
