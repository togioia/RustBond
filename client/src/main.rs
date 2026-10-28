//! RustBond — macOS menu bar client for a bonded TCP/UDP tunnel.

mod config;
mod engine;
mod net;

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

    rt.block_on(engine::run(cfg, engine::new_status()))
}
