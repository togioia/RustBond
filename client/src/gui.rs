//! macOS menu bar UI: a tray icon plus a small settings window.

use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use rnb_shared::{AppConfig, Proto};

use crate::{config, engine, net};

pub fn run(rt: tokio::runtime::Runtime, cfg: AppConfig) -> Result<()> {
    let status = engine::new_status();
    let ifaces = net::list();
    let app = BondApp::new(rt, cfg, ifaces, status);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([460.0, 420.0])
            .with_title("RustBond"),
        ..Default::default()
    };

    eframe::run_native(
        "RustBond",
        options,
        Box::new(move |_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct RouteRow {
    id: u16,
    proto: Proto,
    local_port: u16,
    targets: String,
}

#[cfg(feature = "tray")]
struct Tray {
    _icon: tray_icon::TrayIcon,
    start: tray_icon::menu::MenuItem,
    stop: tray_icon::menu::MenuItem,
    show: tray_icon::menu::MenuItem,
    quit: tray_icon::menu::MenuItem,
}

/// 16x16 RGBA icon: a simple rounded blue dot, no asset files needed.
#[cfg(feature = "tray")]
fn tray_rgba() -> Vec<u8> {
    let (w, h) = (16u32, 16u32);
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    let c = 8.0f32;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = if d <= 6.0 {
                255
            } else if d <= 7.0 {
                (255.0 * (7.0 - d)).max(0.0) as u8
            } else {
                0
            };
            buf.extend_from_slice(&[0x3b, 0x82, 0xf6, a]);
        }
    }
    buf
}

#[cfg(feature = "tray")]
fn build_tray() -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let menu = Menu::new();
    let start = MenuItem::new("Start", true, None);
    let stop = MenuItem::new("Stop", true, None);
    let show = MenuItem::new("Open RustBond", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[&start, &stop, &show, &quit]).ok()?;

    let icon = Icon::from_rgba(tray_rgba(), 16, 16).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("RustBond")
        .with_icon(icon)
        .build()
        .ok()?;

    Some(Tray {
        _icon: tray,
        start,
        stop,
        show,
        quit,
    })
}

struct BondApp {
    rt: tokio::runtime::Runtime,
    server_addr: String,
    key: String,
    interfaces: Vec<net::Iface>,
    selected: Vec<bool>,
    routes: Vec<RouteRow>,
    status: engine::StatusHandle,
    handle: Option<tokio::task::AbortHandle>,
    running: bool,
    visible: bool,
    message: String,
    #[cfg(feature = "tray")]
    tray: Option<Tray>,
    #[cfg(feature = "tray")]
    tray_ready: bool,
}

impl BondApp {
    fn new(
        rt: tokio::runtime::Runtime,
        cfg: AppConfig,
        interfaces: Vec<net::Iface>,
        status: engine::StatusHandle,
    ) -> Self {
        let selected = interfaces
            .iter()
            .map(|i| cfg.interfaces.iter().any(|n| n == &i.name))
            .collect();
        let routes = cfg
            .routes
            .iter()
            .map(|r| RouteRow {
                id: r.id,
                proto: r.proto,
                local_port: r.local_port,
                targets: r.targets.join(", "),
            })
            .collect();

        Self {
            rt,
            server_addr: cfg.server_addr,
            key: cfg.key,
            interfaces,
            selected,
            routes,
            status,
            handle: None,
            running: false,
            visible: true,
            message: String::new(),
            #[cfg(feature = "tray")]
            tray: None,
            #[cfg(feature = "tray")]
            tray_ready: false,
        }
    }

    fn to_config(&self) -> AppConfig {
        AppConfig {
            server_addr: self.server_addr.trim().to_string(),
            key: self.key.clone(),
            interfaces: self
                .interfaces
                .iter()
                .zip(self.selected.iter())
                .filter(|(_, on)| **on)
                .map(|(i, _)| i.name.clone())
                .collect(),
            routes: self
                .routes
                .iter()
                .map(|r| rnb_shared::RouteCfg {
                    id: r.id,
                    proto: r.proto,
                    local_port: r.local_port,
                    targets: r
                        .targets
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                })
                .collect(),
        }
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        let cfg = self.to_config();
        if cfg.server_addr.is_empty() || cfg.key.is_empty() {
            self.message = "server and key are required".into();
            return;
        }
        if cfg.routes.is_empty() {
            self.message = "add at least one route".into();
            return;
        }

        let status = self.status.clone();
        let handle = self.rt.spawn(async move {
            if let Err(e) = engine::run(cfg, status.clone()).await {
                if let Ok(mut g) = status.lock() {
                    *g = format!("error: {e:#}");
                }
            }
        });
        self.handle = Some(handle.abort_handle());
        self.running = true;
        self.message = "started".into();
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.running = false;
        if let Ok(mut g) = self.status.lock() {
            *g = "stopped".into();
        }
        self.message = "stopped".into();
    }

    #[allow(unused_mut)]
    fn poll_menu(&mut self, ctx: &egui::Context) {
        let mut want_start = false;
        let mut want_stop = false;
        let mut want_show = false;
        let mut want_quit = false;

        #[cfg(feature = "tray")]
        if let Some(tray) = &self.tray {
            while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if event.id == tray.start.id() {
                    want_start = true;
                } else if event.id == tray.stop.id() {
                    want_stop = true;
                } else if event.id == tray.show.id() {
                    want_show = true;
                } else if event.id == tray.quit.id() {
                    want_quit = true;
                }
            }
        }

        if want_start {
            self.start();
        }
        if want_stop {
            self.stop();
        }
        if want_show {
            self.visible = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if want_quit {
            self.stop();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn status_text(&self) -> String {
        self.status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "?".into())
    }
}

impl eframe::App for BondApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // tray-icon touches NSApplication: it must not run before winit owns the
        // event loop, or winit aborts with "requires control over the principal
        // class". The first update() is safely inside the loop.
        #[cfg(feature = "tray")]
        if !self.tray_ready {
            self.tray = build_tray();
            self.tray_ready = true;
        }

        self.poll_menu(ctx);

        // Closing the window only hides it: RustBond lives in the menu bar.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RustBond");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let color = if self.running {
                        egui::Color32::from_rgb(0x22, 0xc5, 0x5e)
                    } else {
                        egui::Color32::GRAY
                    };
                    ui.colored_label(color, if self.running { "● running" } else { "○ stopped" });
                });
            });
            ui.label(self.status_text());
            ui.separator();

            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Server (host:port)");
                    ui.text_edit_singleline(&mut self.server_addr);
                    ui.end_row();

                    ui.label("Key (PSK)");
                    ui.add(egui::TextEdit::singleline(&mut self.key).password(true));
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.label("Connections (bonding)");
            ui.label(
                egui::RichText::new("Each checked interface opens its own transport; streams are spread round-robin.")
                    .small()
                    .weak(),
            );

            if self.interfaces.is_empty() {
                ui.colored_label(egui::Color32::YELLOW, "no IPv4 interface found");
            }
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .id_source("ifaces")
                .show(ui, |ui| {
                    for (iface, on) in self.interfaces.iter().zip(self.selected.iter_mut()) {
                        ui.checkbox(on, format!("{}  ({})", iface.name, iface.ip));
                    }
                });
            ui.horizontal(|ui| {
                if ui.button("Refresh interfaces").clicked() {
                    self.interfaces = net::list();
                    self.selected = vec![false; self.interfaces.len()];
                }
                if ui.button("Auto (system route)").clicked() {
                    for on in self.selected.iter_mut() {
                        *on = false;
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.label("Routes");
            ui.label(
                egui::RichText::new("Format: id | proto | local_port | targets (comma separated, first = primary)")
                    .small()
                    .weak(),
            );

            egui::ScrollArea::vertical()
                .max_height(160.0)
                .id_source("routes")
                .show(ui, |ui| {
                    for route in self.routes.iter_mut() {
                        ui.horizontal(|ui| {
                            ui.label(format!("#{}", route.id));
                            let proto = match route.proto {
                                Proto::Tcp => "tcp",
                                Proto::Udp => "udp",
                            };
                            ui.label(proto);
                            ui.label(format!(":{}", route.local_port));
                            ui.add(
                                egui::TextEdit::singleline(&mut route.targets)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("host:port, backup:port"),
                            );
                        });
                    }
                });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new("Start"))
                    .clicked()
                {
                    self.start();
                }
                if ui
                    .add_enabled(self.running, egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop();
                }
                if ui.button("Save config").clicked() {
                    match config::save(&self.to_config()) {
                        Ok(p) => self.message = format!("saved {}", p.display()),
                        Err(e) => self.message = format!("save error: {e:#}"),
                    }
                }
                if ui.button("Hide window").clicked() {
                    self.visible = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            });

            if !self.message.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&self.message).small().weak());
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("config: {}", config::path().display()))
                    .small()
                    .weak(),
            );
        });

        // The tray menu events are polled, so keep repainting at low rate.
        ctx.request_repaint_after(Duration::from_millis(400));
    }
}
