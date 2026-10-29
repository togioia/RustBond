//! macOS menu bar UI: a liquid-glass panel with live per-link traffic.
//!
//! The window is transparent and undecorated; on macOS 26 we ask AppKit for a
//! real `NSGlassEffectView` behind our content (older systems fall back to the
//! classic `NSVisualEffectView` vibrancy). Everything else — cards, pills,
//! specular highlights — is painted by egui on top of that backdrop.

use std::time::Duration;

use anyhow::Result;
use eframe::egui::{
    self, Align2, Color32, FontId, Margin, Rect, RichText, Rounding, Sense, Stroke, Vec2,
};
use rnb_shared::{AppConfig, Proto};

use crate::{config, engine, net, sysproxy};

const RADIUS: f32 = 22.0;
const INNER_RADIUS: f32 = 14.0;
const PANEL_MARGIN: f32 = 8.0;
const FOOTER_HEIGHT: f32 = 132.0;

fn accent() -> Color32 {
    Color32::from_rgb(74, 140, 255)
}
fn green() -> Color32 {
    Color32::from_rgb(64, 214, 130)
}
fn amber() -> Color32 {
    Color32::from_rgb(240, 184, 84)
}
fn muted() -> Color32 {
    Color32::from_rgba_unmultiplied(220, 226, 240, 150)
}
fn text() -> Color32 {
    Color32::from_rgb(238, 242, 250)
}

pub fn run(rt: tokio::runtime::Runtime, cfg: AppConfig) -> Result<()> {
    let status = engine::new_status();
    let stats = engine::StatsHandle::default();
    let ifaces = net::list();
    let app = BondApp::new(rt, cfg, ifaces, status, stats);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 780.0])
            .with_min_inner_size([540.0, 620.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_title("RustBond"),
        ..Default::default()
    };

    eframe::run_native(
        "RustBond",
        options,
        Box::new(move |cc| {
            // NOTE: we do not attach NSVisualEffectView/NSGlassEffectView.
            // eframe's glow renderer paints into the window's content view, and
            // AppKit subviews are composited *above* it: the blur layer would
            // smear the whole UI (regression seen in 0.3.0). The glass look is
            // painted by egui instead; real backdrop blur needs the wgpu
            // renderer, planned separately.
            cc.egui_ctx.set_visuals(glass_visuals());
            cc.egui_ctx.style_mut(|st| {
                st.spacing.item_spacing = Vec2::new(8.0, 6.0);
                st.spacing.button_padding = Vec2::new(10.0, 6.0);
                st.spacing.interact_size.y = 26.0;
            });
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

fn glass_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = Color32::TRANSPARENT;
    v.extreme_bg_color = Color32::from_rgba_unmultiplied(6, 10, 20, 90);
    v.faint_bg_color = Color32::from_white_alpha(6);
    v.override_text_color = Some(text());
    v.window_rounding = Rounding::same(INNER_RADIUS);
    v.menu_rounding = Rounding::same(INNER_RADIUS);

    v.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(16));

    v.widgets.inactive.bg_fill = Color32::from_white_alpha(12);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(26));
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::from_gray(220));

    v.widgets.hovered.bg_fill = Color32::from_white_alpha(22);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(60));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::from_gray(245));

    v.widgets.active.bg_fill = Color32::from_white_alpha(30);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(80));

    v.selection.bg_fill = Color32::from_rgba_unmultiplied(74, 140, 255, 140);
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    v
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

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
    stats: engine::StatsHandle,
    proxy_port: String,
    auto_proxy: bool,
    proxy_guard: Option<sysproxy::Guard>,
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
        stats: engine::StatsHandle,
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
            stats,
            proxy_port: cfg.proxy_port.map(|p| p.to_string()).unwrap_or_default(),
            auto_proxy: cfg.auto_proxy,
            proxy_guard: None,
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
            proxy_port: self.proxy_port.trim().parse().ok(),
            auto_proxy: self.auto_proxy,
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
        if cfg.routes.is_empty() && cfg.proxy_port.is_none() {
            self.message = "add a route or a SOCKS port".into();
            return;
        }

        let auto_proxy = cfg.auto_proxy;
        let proxy_port = cfg.proxy_port;
        let status = self.status.clone();
        let stats = self.stats.clone();
        let handle = self.rt.spawn(async move {
            if let Err(e) = engine::run(cfg, status.clone(), stats).await {
                if let Ok(mut g) = status.lock() {
                    *g = format!("error: {e:#}");
                }
            }
        });
        self.handle = Some(handle.abort_handle());
        self.running = true;
        self.message = "started".into();

        // WireGuard-style: the system proxy follows the tunnel.
        if auto_proxy {
            match proxy_port {
                Some(port) if sysproxy::supported() => {
                    match sysproxy::enable("127.0.0.1", port) {
                        Ok(guard) => {
                            self.message =
                                format!("system proxy → 127.0.0.1:{port} ({} services)", guard.count());
                            self.proxy_guard = Some(guard);
                        }
                        Err(e) => self.message = format!("tunnel up, system proxy failed: {e:#}"),
                    }
                }
                Some(_) => self.message = "started (system proxy: macOS only)".into(),
                None => self.message = "started (no SOCKS port, system proxy off)".into(),
            }
        }
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.stats.disconnect_all();
        let restored = self.proxy_guard.take().is_some();
        self.running = false;
        if let Ok(mut g) = self.status.lock() {
            *g = "stopped".into();
        }
        self.message = if restored {
            "stopped, system proxy restored".into()
        } else {
            "stopped".into()
        };
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

impl Drop for BondApp {
    fn drop(&mut self) {
        // The tunnel dies with the process; make sure the system proxy does not
        // stay pointed at a dead port.
        self.proxy_guard.take();
    }
}

impl eframe::App for BondApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

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

        let screen = ctx.screen_rect();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(Color32::TRANSPARENT)
                    .inner_margin(Margin::same(26.0)),
            )
            .show(ctx, |ui| {
                paint_backdrop(ui.painter(), screen.shrink(PANEL_MARGIN));
                self.body(ui, ctx);
            });

        // Traffic counters must tick even when no widget is hovered.
        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

impl BondApp {
    fn body(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let header = self.header(ui);
        let drag = ui.interact(header.rect, ui.id().with("drag"), Sense::drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        ui.add_space(10.0);

        let body_h = (ui.available_height() - FOOTER_HEIGHT).max(120.0);
        egui::ScrollArea::vertical()
            .max_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.connection_card(ui);
                ui.add_space(12.0);
                self.links_card(ui);
                ui.add_space(12.0);
                self.routes_card(ui);
            });

        ui.add_space(10.0);
        self.footer(ui);
    }

    fn header(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let connected = self
            .stats
            .snapshot()
            .iter()
            .filter(|s| s.connected)
            .count();

        ui.horizontal(|ui| {
            let (logo, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
            paint_logo(ui.painter(), logo);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.label(RichText::new("RustBond").size(20.0).strong().color(text()));
                ui.label(
                    RichText::new("bonded tunnel")
                        .size(11.0)
                        .color(muted()),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_pill(ui, self.running, connected);
            });
        })
        .response
    }

    fn connection_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            section(ui, "CONNECTION");
            field(ui, "Server", &mut self.server_addr, "host:port", false);
            field(ui, "Pre-shared key", &mut self.key, "PSK", true);

            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut self.auto_proxy,
                    RichText::new("System SOCKS proxy").size(12.5),
                );
                ui.add_space(10.0);
                ui.label(RichText::new("port").size(11.5).color(muted()));
                ui.add(
                    egui::TextEdit::singleline(&mut self.proxy_port)
                        .desired_width(64.0)
                        .hint_text("11080")
                        .margin(Margin::symmetric(8.0, 5.0)),
                );
            });
            ui.label(
                RichText::new(
                    "On start the tunnel points macOS at this SOCKS port; on stop it restores \
                     the previous proxy settings.",
                )
                .size(10.5)
                .color(Color32::from_rgba_unmultiplied(220, 226, 240, 105)),
            );
        });
    }

    fn links_card(&mut self, ui: &mut egui::Ui) {
        let snaps = self.stats.snapshot();
        let total_tx: u64 = self
            .interfaces
            .iter()
            .zip(self.selected.iter())
            .filter(|(_, on)| **on)
            .filter_map(|(iface, _)| find_stat(&snaps, iface))
            .map(|s| s.tx)
            .sum();

        card(ui, |ui| {
            ui.horizontal(|ui| {
                section(ui, "LINKS (BONDING)");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if mini_button(ui, "reset").clicked() {
                        self.stats.reset();
                    }
                });
            });
            ui.add_space(4.0);

            if self.interfaces.is_empty() {
                ui.colored_label(amber(), "No IPv4 interface found");
            }

            for (iface, on) in self.interfaces.iter().zip(self.selected.iter_mut()) {
                let stat = find_stat(&snaps, iface).cloned();
                link_row(ui, iface, on, stat.as_ref(), total_tx);
                ui.add_space(4.0);
            }

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let (rx, tx) = self
                    .stats
                    .snapshot()
                    .iter()
                    .fold((0u64, 0u64), |(rx, tx), s| (rx + s.rx, tx + s.tx));
                ui.label(
                    RichText::new(format!("Total  ↑ {}   ↓ {}", fmt_bytes(tx), fmt_bytes(rx)))
                        .size(11.0)
                        .color(muted()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if mini_button(ui, "refresh").clicked() {
                        self.interfaces = net::list();
                        self.selected = vec![false; self.interfaces.len()];
                    }
                    if mini_button(ui, "auto").clicked() {
                        for on in self.selected.iter_mut() {
                            *on = false;
                        }
                    }
                });
            });
        });
    }

    fn routes_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            egui::CollapsingHeader::new(
                RichText::new("ROUTES")
                    .size(10.0)
                    .strong()
                    .color(muted()),
            )
            .default_open(true)
            .show(ui, |ui| {
                for route in self.routes.iter_mut() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("#{}", route.id))
                                .size(11.5)
                                .color(muted()),
                        );
                        let proto = match route.proto {
                            Proto::Tcp => "tcp",
                            Proto::Udp => "udp",
                        };
                        ui.label(RichText::new(proto).size(11.5).color(muted()));
                        ui.add(
                            egui::TextEdit::singleline(&mut route.targets)
                                .desired_width(f32::INFINITY)
                                .hint_text("host:port, backup:port")
                                .margin(Margin::symmetric(8.0, 4.0)),
                        );
                    });
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        if self.running {
            if glass_button(ui, "Disconnect", BtnKind::Danger).clicked() {
                self.stop();
            }
        } else if glass_button(ui, "Connect", BtnKind::Primary).clicked() {
            self.start();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ghost_button(ui, "Save").clicked() {
                match config::save(&self.to_config()) {
                    Ok(p) => self.message = format!("saved {}", p.display()),
                    Err(e) => self.message = format!("save error: {e:#}"),
                }
            }
            if ghost_button(ui, "Hide").clicked() {
                self.visible = false;
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, "Quit").clicked() {
                    self.stop();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });

        ui.add_space(6.0);
        let msg = if self.message.is_empty() {
            self.status_text()
        } else {
            self.message.clone()
        };
        ui.label(RichText::new(msg).size(10.5).color(muted()));
        ui.label(
            RichText::new(format!("config: {}", config::path().display()))
                .size(9.5)
                .color(Color32::from_rgba_unmultiplied(220, 226, 240, 80)),
        );
    }
}

// ---------------------------------------------------------------------------
// Painting helpers
// ---------------------------------------------------------------------------

enum BtnKind {
    Primary,
    Danger,
}

fn paint_backdrop(painter: &egui::Painter, rect: Rect) {
    for (grow, alpha) in [(3.0f32, 24u8), (7.0, 14), (12.0, 8), (18.0, 4)] {
        painter.rect_filled(
            rect.expand(grow),
            Rounding::same(RADIUS + grow),
            Color32::from_black_alpha(alpha),
        );
    }

    // Opaque: text must stay crisp. The "glass" is in the highlights,
    // translucency of the cards and the specular edges.
    let base = Color32::from_rgb(15, 19, 30);
    painter.rect(
        rect,
        Rounding::same(RADIUS),
        base,
        Stroke::new(1.0_f32, Color32::from_white_alpha(34)),
    );

    // Specular highlight along the top edge + a faint glass sheen.
    painter.line_segment(
        [
            rect.left_top() + Vec2::new(RADIUS, 1.0),
            rect.right_top() + Vec2::new(-RADIUS, 1.0),
        ],
        Stroke::new(1.0_f32, Color32::from_white_alpha(84)),
    );
    painter.rect_filled(
        Rect::from_min_max(
            rect.min + Vec2::splat(1.0),
            egui::pos2(rect.max.x - 1.0, rect.min.y + 96.0),
        ),
        Rounding {
            nw: RADIUS,
            ne: RADIUS,
            sw: 0.0,
            se: 0.0,
        },
        Color32::from_white_alpha(6),
    );
}

fn paint_logo(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, Rounding::same(12.0), Color32::from_rgb(44, 96, 210));
    painter.rect(
        rect,
        Rounding::same(12.0),
        Color32::TRANSPARENT,
        Stroke::new(1.0_f32, Color32::from_white_alpha(60)),
    );
    painter.circle_stroke(
        rect.center(),
        9.0,
        Stroke::new(2.5_f32, Color32::from_white_alpha(235)),
    );
    painter.circle_filled(rect.center(), 2.8, Color32::from_white_alpha(235));
}

fn status_pill(ui: &mut egui::Ui, running: bool, connected: usize) {
    let (label, color) = if running && connected > 0 {
        (format!("Live · {connected}"), green())
    } else if running {
        ("Connecting".to_string(), amber())
    } else {
        ("Offline".to_string(), Color32::from_gray(150))
    };

    let galley = ui.painter().layout_no_wrap(
        label.clone(),
        FontId::proportional(11.5),
        Color32::WHITE,
    );
    let size = Vec2::new(galley.size().x + 34.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        Rounding::same(13.0),
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 34),
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 130)),
    );
    let dot = rect.left_center() + Vec2::new(13.0, 0.0);
    painter.circle_filled(dot, 6.0, Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 60));
    painter.circle_filled(dot, 3.2, color);
    painter.galley(
        rect.left_top() + Vec2::new(24.0, (rect.height() - galley.size().y) * 0.5),
        galley,
        Color32::from_gray(230),
    );
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(10.0).strong().color(muted()));
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, password: bool) {
    ui.label(RichText::new(label).size(11.0).color(muted()));
    let mut te = egui::TextEdit::singleline(value)
        .hint_text(hint)
        .desired_width(f32::INFINITY)
        .margin(Margin::symmetric(10.0, 7.0));
    if password {
        te = te.password(true);
    }
    ui.add(te);
    ui.add_space(8.0);
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 24)))
        .rounding(Rounding::same(16.0))
        .inner_margin(Margin::same(14.0))
        .shadow(egui::epaint::Shadow {
            offset: Vec2::new(0.0, 8.0),
            blur: 20.0,
            spread: 0.0,
            color: Color32::from_black_alpha(45),
        })
        .show(ui, add)
        .inner
}

fn link_row(
    ui: &mut egui::Ui,
    iface: &net::Iface,
    on: &mut bool,
    stat: Option<&engine::LinkSnapshot>,
    total_tx: u64,
) {
    let enabled = iface.ip.is_some();
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 58.0),
        Sense::click(),
    );
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered()) * if enabled { 1.0 } else { 0.0 };
    let selected = *on && enabled;

    if resp.clicked() && enabled {
        *on = !*on;
    }

    let fill = if selected {
        Color32::from_rgba_unmultiplied(74, 140, 255, 40)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, (6.0 + hover * 10.0) as u8)
    };
    let stroke = if selected {
        Color32::from_rgba_unmultiplied(120, 172, 255, 110)
    } else {
        Color32::from_white_alpha(18)
    };
    let painter = ui.painter();
    painter.rect(rect, Rounding::same(12.0), fill, Stroke::new(1.0_f32, stroke));

    // Checkbox / radio styled as a glass square.
    let cb = Rect::from_center_size(rect.left_center() + Vec2::new(22.0, 0.0), Vec2::splat(18.0));
    let cb_round = Rounding::same(5.0);
    if selected {
        painter.rect_filled(cb, cb_round, accent());
        let c = cb.center();
        painter.line_segment(
            [c + Vec2::new(-4.0, 0.0), c + Vec2::new(-1.0, 3.5)],
            Stroke::new(2.0_f32, Color32::WHITE),
        );
        painter.line_segment(
            [c + Vec2::new(-1.0, 3.5), c + Vec2::new(4.5, -3.5)],
            Stroke::new(2.0_f32, Color32::WHITE),
        );
    } else {
        painter.rect(
            cb,
            cb_round,
            Color32::from_black_alpha(40),
            Stroke::new(1.0_f32, Color32::from_white_alpha(if enabled { 70 } else { 30 })),
        );
    }

    let title_color = if enabled { text() } else { Color32::from_gray(120) };
    let base = rect.left_top() + Vec2::new(42.0, 12.0);
    painter.text(
        base,
        Align2::LEFT_TOP,
        format!("{} ({})", iface.label, iface.name),
        FontId::proportional(13.5),
        title_color,
    );

    let sub = match iface.ip {
        Some(ip) => ip.to_string(),
        None => "senza IP".to_string(),
    };
    painter.text(
        base + Vec2::new(0.0, 20.0),
        Align2::LEFT_TOP,
        sub,
        FontId::proportional(10.5),
        Color32::from_rgba_unmultiplied(210, 218, 235, if enabled { 130 } else { 80 }),
    );

    if let Some(s) = stat {
        let right = rect.right_top() + Vec2::new(-14.0, 13.0);
        painter.text(
            right,
            Align2::RIGHT_TOP,
            format!("↑ {}   ↓ {}", fmt_bytes(s.tx), fmt_bytes(s.rx)),
            FontId::monospace(11.0),
            Color32::from_gray(215),
        );

        // Share-of-traffic bar.
        let bar = Rect::from_min_size(
            egui::pos2(rect.right() - 134.0, rect.bottom() - 16.0),
            Vec2::new(120.0, 4.0),
        );
        painter.rect_filled(bar, Rounding::same(2.0), Color32::from_white_alpha(26));
        let pct = if total_tx > 0 {
            (s.tx as f32 / total_tx as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if pct > 0.0 {
            painter.rect_filled(
                Rect::from_min_size(bar.min, Vec2::new(bar.width() * pct, bar.height())),
                Rounding::same(2.0),
                accent(),
            );
        }
        painter.text(
            bar.left_top() + Vec2::new(0.0, -3.0),
            Align2::LEFT_BOTTOM,
            format!("{:.0}%", pct * 100.0),
            FontId::proportional(9.0),
            Color32::from_rgba_unmultiplied(200, 210, 230, 120),
        );

        if s.connected {
            let dot = rect.right_top() + Vec2::new(-14.0, 40.0);
            painter.circle_filled(dot, 3.5, green());
        }
    }

    if hover > 0.1 && enabled {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

fn glass_button(ui: &mut egui::Ui, label: &str, kind: BtnKind) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 46.0),
        Sense::click(),
    );
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
    let pressed = resp.is_pointer_button_down_on();

    let (base, hi) = match kind {
        BtnKind::Primary => (
            Color32::from_rgb(40, 104, 235),
            Color32::from_rgb(104, 168, 255),
        ),
        BtnKind::Danger => (
            Color32::from_rgb(178, 52, 68),
            Color32::from_rgb(232, 104, 118),
        ),
    };
    let mut bg = mix(base, hi, hover * 0.85);
    if pressed {
        bg = bg.gamma_multiply(0.88);
    }

    ui.painter().rect_filled(
        rect.translate(Vec2::new(0.0, 4.0)),
        Rounding::same(INNER_RADIUS),
        Color32::from_black_alpha(60),
    );
    ui.painter().rect(
        rect,
        Rounding::same(INNER_RADIUS),
        bg,
        Stroke::new(1.0_f32, Color32::from_white_alpha(56)),
    );
    ui.painter().rect_filled(
        Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.center().y)),
        Rounding {
            nw: INNER_RADIUS,
            ne: INNER_RADIUS,
            sw: 0.0,
            se: 0.0,
        },
        Color32::from_white_alpha(16),
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(15.0),
        Color32::from_gray(248),
    );

    if hover > 0.2 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn ghost_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(12.0), text());
    let size = Vec2::new(galley.size().x + 22.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
    ui.painter().rect(
        rect,
        Rounding::same(9.0),
        Color32::from_white_alpha((8.0 + hover * 14.0) as u8),
        Stroke::new(1.0_f32, Color32::from_white_alpha(24)),
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(12.0),
        Color32::from_gray(228),
    );
    if hover > 0.2 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn mini_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(10.0), muted());
    let size = Vec2::new(galley.size().x + 16.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
    ui.painter().rect(
        rect,
        Rounding::same(7.0),
        Color32::from_white_alpha((6.0 + hover * 12.0) as u8),
        Stroke::new(1.0_f32, Color32::from_white_alpha(20)),
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(10.0),
        muted(),
    );
    if hover > 0.2 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

fn find_stat<'a>(
    snaps: &'a [engine::LinkSnapshot],
    iface: &net::Iface,
) -> Option<&'a engine::LinkSnapshot> {
    let prefix = format!("{} ({})", iface.label, iface.name);
    snaps.iter().find(|s| s.label.starts_with(&prefix))
}

fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / (K * K))
    } else {
        format!("{:.2} GB", f / (K * K * K))
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}
