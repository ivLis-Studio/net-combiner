use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui;
use tokio_util::sync::CancellationToken;

use crate::adapter::{AdapterAddress, EgressTarget};
use crate::proxy::{self, ProxyConfig};
use crate::tray::{self, TrayEvent, TrayHandle, TrayLanguage};
use crate::vpn::{DnsStrategy, VpnConfig, VpnProcess};

const MAX_LOG_LINES: usize = 800;
const MAX_CONNECTION_ROWS: usize = 5_000;
const PAGE_MAX_WIDTH: f32 = 760.0;

pub fn run_gui() -> Result<()> {
    let icon = load_icon_data();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([880.0, 820.0])
        .with_min_inner_size([520.0, 580.0]);
    if let Some(icon) = icon.clone() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "net-combiner",
        options,
        Box::new(move |cc| {
            install_cjk_font_fallbacks(&cc.egui_ctx);
            install_base_style(&cc.egui_ctx);
            Ok(Box::new(NetCombinerApp::new(cc, icon)))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    English,
    Korean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Proxy,
    Vpn,
}

struct ModeCardSpec<'a> {
    mode: Mode,
    icon: &'a str,
    title: &'a str,
    body: &'a str,
    suited_for: &'a str,
}

#[derive(Debug, Clone)]
enum AppStatus {
    Idle,
    ProxyRunning,
    VpnRunning,
    AdapterRefreshFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionMonitorFilter {
    Active,
    Recent,
    All,
}

enum UpdateMessage {
    Check(Result<crate::update::UpdateInfo, String>),
    Install(Result<String, String>),
}

struct NetCombinerApp {
    adapters: Vec<AdapterRow>,
    language: Language,
    theme_mode: ThemeMode,
    mode: Mode,
    show_all_adapters: bool,
    show_advanced: bool,
    listen_ip: String,
    listen_port: String,
    udp_enabled: bool,
    setup_routes: bool,
    enable_ipv6: bool,
    dns_strategy: DnsStrategy,
    bypass_text: String,
    tun2proxy_path: String,
    proxy: Option<ManagedProxy>,
    vpn: Option<VpnProcess>,
    log_rx: Option<mpsc::Receiver<String>>,
    log_tx: Option<mpsc::Sender<String>>,
    connection_rx: Option<mpsc::Receiver<proxy::ConnectionEvent>>,
    connection_tx: Option<mpsc::Sender<proxy::ConnectionEvent>>,
    connections: VecDeque<ConnectionRow>,
    logs: VecDeque<String>,
    log_path: PathBuf,
    log_file: Option<fs::File>,
    show_logs: bool,
    show_connection_window: bool,
    connection_monitor_filter: ConnectionMonitorFilter,
    show_tray_options: bool,
    close_to_tray: bool,
    tray_available: bool,
    window_visible: bool,
    allow_exit: bool,
    tray: Option<TrayHandle>,
    tray_rx: Option<mpsc::Receiver<TrayEvent>>,
    tray_language: TrayLanguage,
    update_rx: Option<mpsc::Receiver<UpdateMessage>>,
    update_status: String,
    update_busy: bool,
    update_auto_checked: bool,
    last_refresh: Option<Instant>,
    started_at: Instant,
    status: AppStatus,
    icon_texture: Option<egui::TextureHandle>,
}

impl NetCombinerApp {
    fn new(cc: &eframe::CreationContext<'_>, icon: Option<egui::IconData>) -> Self {
        let icon_texture = icon.as_ref().map(|icon| {
            let image = egui::ColorImage::from(icon);
            cc.egui_ctx
                .load_texture("net-combiner-icon", image, egui::TextureOptions::LINEAR)
        });
        let (log_path, log_file, log_error) = open_log_file();
        let language = Language::English;
        let tray_language = tray_language_for(language);
        let (tray_tx, tray_rx) = mpsc::channel();
        let tray_wake = {
            let ctx = cc.egui_ctx.clone();
            Arc::new(move || ctx.request_repaint()) as Arc<dyn Fn() + Send + Sync>
        };
        let tray_result = if tray::is_supported() {
            TrayHandle::new(tray_language, tray_tx, tray_wake)
        } else {
            Err(anyhow!("system tray is not supported on this platform"))
        };
        let tray_available = tray_result.is_ok();
        let (tray, tray_error) = match tray_result {
            Ok(handle) => (Some(handle), None),
            Err(error) => (None, Some(error.to_string())),
        };

        let mut app = Self {
            adapters: Vec::new(),
            language,
            theme_mode: ThemeMode::Light,
            mode: Mode::Proxy,
            show_all_adapters: false,
            show_advanced: false,
            listen_ip: "127.0.0.1".to_owned(),
            listen_port: "1080".to_owned(),
            udp_enabled: true,
            setup_routes: true,
            enable_ipv6: false,
            dns_strategy: DnsStrategy::Virtual,
            bypass_text:
                "127.0.0.0/8\n::1/128\n10.0.0.0/8\n172.16.0.0/12\n192.168.0.0/16\n169.254.0.0/16\n224.0.0.0/4\n255.255.255.255/32\nfe80::/10\nff00::/8"
                    .to_owned(),
            tun2proxy_path: String::new(),
            proxy: None,
            vpn: None,
            log_rx: None,
            log_tx: None,
            connection_rx: None,
            connection_tx: None,
            connections: VecDeque::new(),
            logs: VecDeque::new(),
            log_path,
            log_file,
            show_logs: false,
            show_connection_window: false,
            connection_monitor_filter: ConnectionMonitorFilter::Active,
            show_tray_options: false,
            close_to_tray: true,
            tray_available,
            window_visible: true,
            allow_exit: false,
            tray,
            tray_rx: if tray_available { Some(tray_rx) } else { None },
            tray_language,
            update_rx: None,
            update_status: "Update check will run automatically.".to_owned(),
            update_busy: false,
            update_auto_checked: false,
            last_refresh: None,
            started_at: Instant::now(),
            status: AppStatus::Idle,
            icon_texture,
        };
        if let Some(error) = log_error {
            app.push_log(error);
        } else {
            app.push_log(format!("logging to {}", app.log_path.display()));
        }
        if let Some(error) = tray_error {
            app.push_log(format!("tray unavailable: {error}"));
        }
        app.refresh_adapters();
        app
    }

    fn refresh_adapters(&mut self) {
        let previous = self
            .adapters
            .iter()
            .map(|row| (row.adapter.stable_id(), row.selected, row.weight))
            .collect::<Vec<_>>();

        match crate::adapter::list_adapters() {
            Ok(adapters) => {
                self.adapters = adapters
                    .into_iter()
                    .map(|adapter| {
                        let id = adapter.stable_id();
                        let old = previous.iter().find(|(old_id, _, _)| *old_id == id);
                        AdapterRow {
                            selected: old
                                .map(|(_, selected, _)| *selected)
                                .unwrap_or_else(|| default_selected(&adapter)),
                            weight: old.map(|(_, _, weight)| *weight).unwrap_or(1),
                            adapter,
                        }
                    })
                    .collect();
                self.last_refresh = Some(Instant::now());
                if matches!(self.status, AppStatus::AdapterRefreshFailed) {
                    self.status = AppStatus::Idle;
                }
            }
            Err(error) => {
                self.push_log(format!("adapter refresh failed: {error}"));
                self.status = AppStatus::AdapterRefreshFailed;
            }
        }
    }

    fn selected_egress(&self) -> Vec<EgressTarget> {
        self.adapters
            .iter()
            .filter(|row| row.selected)
            .map(|row| EgressTarget {
                name: row.adapter.name.clone(),
                ip: row.adapter.ip,
                weight: row.weight.max(1),
            })
            .collect()
    }

    fn selected_count(&self) -> usize {
        self.adapters.iter().filter(|r| r.selected).count()
    }

    fn visible_adapter_indices(&self) -> Vec<usize> {
        self.adapters
            .iter()
            .enumerate()
            .filter(|(_, row)| self.show_all_adapters || visible_by_default(&row.adapter))
            .map(|(index, _)| index)
            .collect()
    }

    fn hidden_adapter_count(&self) -> usize {
        self.adapters
            .iter()
            .filter(|row| !visible_by_default(&row.adapter))
            .count()
    }

    fn proxy_config(&self) -> Result<ProxyConfig> {
        let listen_ip = self.listen_ip.trim().parse::<IpAddr>()?;
        let listen_port = self.listen_port.trim().parse::<u16>()?;
        let egress = self.selected_egress();
        if egress.is_empty() {
            return Err(anyhow!("select at least one adapter"));
        }
        Ok(ProxyConfig {
            listen_ip,
            listen_port,
            egress,
            udp_enabled: self.udp_enabled,
        })
    }

    fn vpn_config(&self) -> VpnConfig {
        let tun2proxy_path = if self.tun2proxy_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.tun2proxy_path.trim()))
        };
        VpnConfig {
            tun2proxy_path,
            setup_routes: self.setup_routes,
            bypass: self
                .bypass_text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            dns_strategy: self.dns_strategy,
            enable_ipv6: self.enable_ipv6,
        }
    }

    fn ensure_log_channel(&mut self) -> mpsc::Sender<String> {
        if let Some(tx) = &self.log_tx {
            return tx.clone();
        }
        let (tx, rx) = mpsc::channel();
        self.log_tx = Some(tx.clone());
        self.log_rx = Some(rx);
        tx
    }

    fn ensure_connection_channel(&mut self) -> mpsc::Sender<proxy::ConnectionEvent> {
        if let Some(tx) = &self.connection_tx {
            return tx.clone();
        }
        let (tx, rx) = mpsc::channel();
        self.connection_tx = Some(tx.clone());
        self.connection_rx = Some(rx);
        tx
    }

    fn start_proxy(&mut self) {
        if self.proxy.is_some() {
            return;
        }
        let config = match self.proxy_config() {
            Ok(config) => config,
            Err(error) => {
                self.push_log(format!("cannot start proxy: {error}"));
                return;
            }
        };
        let tx = self.ensure_log_channel();
        let connection_tx = self.ensure_connection_channel();
        self.connections.clear();
        match ManagedProxy::start(config, tx, connection_tx) {
            Ok(proxy) => {
                self.proxy = Some(proxy);
                self.status = AppStatus::ProxyRunning;
            }
            Err(error) => self.push_log(format!("cannot start proxy: {error}")),
        }
    }

    fn stop_proxy(&mut self) {
        self.stop_vpn();
        if let Some(mut proxy) = self.proxy.take() {
            proxy.stop();
            self.status = AppStatus::Idle;
            self.mark_active_connections_closed("proxy stopped");
            self.push_log("proxy stopped");
        }
    }

    fn start_vpn(&mut self) {
        if self.vpn.is_some() {
            return;
        }
        if self.proxy.is_none() {
            self.start_proxy();
        }
        if self.proxy.is_none() {
            return;
        }
        let port = match self.listen_port.trim().parse::<u16>() {
            Ok(port) => port,
            Err(error) => {
                self.push_log(format!("invalid proxy port: {error}"));
                return;
            }
        };
        let tx = self.ensure_log_channel();
        match VpnProcess::start(port, self.vpn_config(), tx) {
            Ok(process) => {
                self.vpn = Some(process);
                self.status = AppStatus::VpnRunning;
            }
            Err(error) => self.push_log(format!("cannot start VPN sidecar: {error}")),
        }
    }

    fn stop_vpn(&mut self) {
        if let Some(mut vpn) = self.vpn.take() {
            if let Err(error) = vpn.stop() {
                self.push_log(format!("failed to stop VPN sidecar: {error}"));
            } else {
                self.push_log("VPN sidecar stopped");
            }
        }
        if self.proxy.is_some() {
            self.status = AppStatus::ProxyRunning;
        }
    }

    fn start_current_mode(&mut self) {
        match self.mode {
            Mode::Proxy => self.start_proxy(),
            Mode::Vpn => self.start_vpn(),
        }
    }

    fn stop_all(&mut self) {
        self.stop_proxy();
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        let mut pending_logs = Vec::new();
        if let Some(rx) = &self.log_rx {
            while let Ok(line) = rx.try_recv() {
                pending_logs.push(line);
            }
        }
        for line in pending_logs {
            self.push_log(line);
        }

        let mut pending_connections = Vec::new();
        if let Some(rx) = &self.connection_rx {
            while let Ok(event) = rx.try_recv() {
                pending_connections.push(event);
            }
        }
        for event in pending_connections {
            self.apply_connection_event(event);
        }

        let mut pending_tray = Vec::new();
        if let Some(rx) = &self.tray_rx {
            while let Ok(event) = rx.try_recv() {
                pending_tray.push(event);
            }
        }
        for event in pending_tray {
            self.handle_tray_event(ctx, event);
        }

        self.sync_tray_language();
        self.poll_update_messages();
        if !self.update_auto_checked && self.started_at.elapsed() >= Duration::from_secs(2) {
            self.update_auto_checked = true;
            self.start_update_check();
        }
        if self.show_connection_window
            || self.is_running()
            || self.update_busy
            || !self.update_auto_checked
        {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        if self.proxy.as_ref().is_some_and(ManagedProxy::is_finished) {
            if let Some(mut proxy) = self.proxy.take() {
                proxy.join_finished();
                self.push_log("proxy task exited");
                self.mark_active_connections_closed("proxy task exited");
                if self.vpn.is_none() {
                    self.status = AppStatus::Idle;
                }
            }
        }

        let vpn_status = if let Some(vpn) = self.vpn.as_mut() {
            match vpn.try_wait() {
                Ok(Some(status)) => Some(format!("VPN sidecar exited: {status}")),
                Ok(None) => None,
                Err(error) => Some(format!("VPN sidecar status check failed: {error}")),
            }
        } else {
            None
        };
        if let Some(message) = vpn_status {
            self.vpn = None;
            self.push_log(message);
            self.status = if self.proxy.is_some() {
                AppStatus::ProxyRunning
            } else {
                AppStatus::Idle
            };
        }
    }

    fn push_log(&mut self, message: impl Into<String>) {
        let line = format!("[{}] {}", log_timestamp(), message.into());
        if let Some(file) = self.log_file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        self.logs.push_back(line);
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
    }

    fn apply_connection_event(&mut self, event: proxy::ConnectionEvent) {
        match event {
            proxy::ConnectionEvent::Opened(opened) => {
                self.connections.retain(|row| row.id != opened.id);
                self.connections.push_front(ConnectionRow::from(opened));
            }
            proxy::ConnectionEvent::Closed(closed) => {
                if let Some(row) = self.connections.iter_mut().find(|row| row.id == closed.id) {
                    row.up_bytes = closed.up_bytes;
                    row.down_bytes = closed.down_bytes;
                    row.reason = closed.reason;
                    row.closed_at = Some(closed.closed_at);
                }
            }
        }
        self.prune_connections();
    }

    fn prune_connections(&mut self) {
        while self.connections.len() > MAX_CONNECTION_ROWS {
            self.connections.pop_back();
        }
    }

    fn active_connection_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|row| row.closed_at.is_none())
            .count()
    }

    fn recent_connection_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|row| row.closed_at.is_some())
            .count()
    }

    fn connection_count_for(&self, filter: ConnectionMonitorFilter) -> usize {
        self.connections
            .iter()
            .filter(|row| connection_matches_filter(row, filter))
            .count()
    }

    fn mark_active_connections_closed(&mut self, reason: &str) {
        let now = Instant::now();
        for row in &mut self.connections {
            if row.closed_at.is_none() {
                row.closed_at = Some(now);
                row.reason = reason.to_owned();
            }
        }
    }

    fn handle_tray_event(&mut self, ctx: &egui::Context, event: TrayEvent) {
        match event {
            TrayEvent::ShowMain => self.show_main_window(ctx),
            TrayEvent::OpenOptions => {
                self.show_main_window(ctx);
                self.show_tray_options = true;
            }
            TrayEvent::OpenConnections => {
                self.show_main_window(ctx);
                self.show_connection_window = true;
            }
            TrayEvent::Quit => {
                self.allow_exit = true;
                self.stop_all();
                ctx.send_viewport_cmd_to(
                    connection_monitor_viewport_id(),
                    egui::ViewportCommand::Close,
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.close_to_tray && self.tray_available && !self.allow_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray(ctx);
        }
    }

    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        if self.window_visible {
            self.push_log("window hidden to tray");
        }
        self.show_connection_window = false;
        self.show_tray_options = false;
        self.window_visible = false;
        ctx.send_viewport_cmd_to(
            connection_monitor_viewport_id(),
            egui::ViewportCommand::Close,
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn show_main_window(&mut self, ctx: &egui::Context) {
        self.window_visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn sync_tray_language(&mut self) {
        let language = tray_language_for(self.language);
        if language == self.tray_language {
            return;
        }
        self.tray_language = language;
        if let Some(tray) = &self.tray {
            tray.set_language(language);
        }
    }

    fn poll_update_messages(&mut self) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.update_rx {
            while let Ok(message) = rx.try_recv() {
                messages.push(message);
            }
        }
        for message in messages {
            self.update_busy = false;
            match message {
                UpdateMessage::Check(Ok(info)) => {
                    if info.available {
                        self.update_status = format!(
                            "Update available: {} -> {}",
                            info.current_version, info.latest_version
                        );
                    } else {
                        self.update_status = format!("Up to date: {}", info.current_version);
                    }
                }
                UpdateMessage::Check(Err(error)) => {
                    self.update_status = format!("Update check failed: {error}");
                }
                UpdateMessage::Install(Ok(status)) => {
                    self.update_status = format!("Update finished: {status}. Restart the app.");
                }
                UpdateMessage::Install(Err(error)) => {
                    self.update_status = format!("Update failed: {error}");
                }
            }
        }
    }

    fn start_update_check(&mut self) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.update_status = "Checking for updates...".to_owned();
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let result = crate::update::check().map_err(|error| error.to_string());
            let _ = tx.send(UpdateMessage::Check(result));
        });
    }

    fn start_update_install(&mut self) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.update_status = "Installing update...".to_owned();
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let result = crate::update::install_latest().map_err(|error| error.to_string());
            let _ = tx.send(UpdateMessage::Install(result));
        });
    }

    fn is_running(&self) -> bool {
        self.proxy.is_some() || self.vpn.is_some()
    }
}

impl eframe::App for NetCombinerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);
        self.handle_close_request(ctx);

        let theme = Theme::for_mode(self.theme_mode);
        apply_theme_visuals(ctx, &theme);
        let t = Texts::new(self.language);

        egui::TopBottomPanel::top("top_bar")
            .exact_height(64.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.surface)
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 24,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| {
                self.draw_top_bar(ui, &theme, &t);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme.bg))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("page_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(12.0);
                        self.draw_centered(ui, |app, ui| {
                            app.draw_step_progress(ui, &theme, &t);
                            ui.add_space(20.0);
                            app.draw_step_intro(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_step_adapters(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_step_mode(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_step_settings(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_step_run(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_connections_section(ui, &theme, &t);
                            ui.add_space(16.0);
                            app.draw_logs_section(ui, &theme, &t);
                            ui.add_space(28.0);
                        });
                    });
            });

        self.draw_connection_monitor_window(ctx, &theme, &t);
        self.draw_tray_options_window(ctx, &theme, &t);
    }
}

// =====================================================================
// Layout helpers
// =====================================================================

impl NetCombinerApp {
    fn draw_centered<F>(&mut self, ui: &mut egui::Ui, f: F)
    where
        F: FnOnce(&mut Self, &mut egui::Ui),
    {
        let avail = ui.available_width();
        let pad = ((avail - PAGE_MAX_WIDTH) / 2.0).max(16.0);
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.allocate_ui_with_layout(
                egui::vec2((avail - pad * 2.0).max(320.0), ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| f(self, ui),
            );
            ui.add_space(pad);
        });
    }

    fn draw_top_bar(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        ui.horizontal_centered(|ui| {
            if let Some(texture) = &self.icon_texture {
                ui.add(egui::Image::from_texture((
                    texture.id(),
                    egui::vec2(30.0, 30.0),
                )));
            } else {
                draw_logo_mark(ui, theme, 30.0);
            }
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("net-combiner")
                        .size(17.0)
                        .strong()
                        .color(theme.text),
                );
                ui.label(
                    egui::RichText::new(t.tagline)
                        .size(11.5)
                        .color(theme.text_muted),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.draw_theme_toggle(ui, theme);
                ui.add_space(8.0);
                self.draw_language_toggle(ui, theme);
                ui.add_space(12.0);
                draw_status_pill(ui, theme, &self.status, t, self.is_running());
            });
        });
    }

    fn draw_theme_toggle(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let label = match self.theme_mode {
            ThemeMode::Light => "🌙",
            ThemeMode::Dark => "☀",
        };
        if pill_button(ui, theme, label, false).clicked() {
            self.theme_mode = match self.theme_mode {
                ThemeMode::Light => ThemeMode::Dark,
                ThemeMode::Dark => ThemeMode::Light,
            };
        }
    }

    fn draw_language_toggle(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let active_en = matches!(self.language, Language::English);
        ui.horizontal(|ui| {
            if pill_button(ui, theme, "한국어", !active_en).clicked() {
                self.language = Language::Korean;
            }
            if pill_button(ui, theme, "EN", active_en).clicked() {
                self.language = Language::English;
            }
        });
    }
}

// =====================================================================
// Steps
// =====================================================================

impl NetCombinerApp {
    fn draw_step_progress(&self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        let steps = [
            t.step_label_intro,
            t.step_label_adapters,
            t.step_label_mode,
            t.step_label_settings,
            t.step_label_run,
        ];
        let current = self.current_step();

        ui.horizontal(|ui| {
            for (idx, label) in steps.iter().enumerate() {
                let active = idx == current;
                let done = idx < current;
                draw_step_chip(ui, theme, idx + 1, label, active, done);
                if idx < steps.len() - 1 {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(18.0, 2.0), egui::Sense::hover());
                    let color = if done { theme.primary } else { theme.border };
                    ui.painter().rect_filled(rect, 1.0, color);
                }
            }
        });
    }

    fn current_step(&self) -> usize {
        if self.is_running() {
            return 4;
        }
        if self.selected_count() == 0 {
            return 1;
        }
        2
    }

    fn draw_step_intro(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            step_header(ui, theme, 1, t.step1_title, t.step1_subtitle);
            ui.add_space(12.0);

            let avail = ui.available_width();
            let diagram_w = avail.min(560.0);
            ui.vertical_centered(|ui| {
                draw_combine_diagram(
                    ui,
                    theme,
                    self.started_at.elapsed().as_secs_f32(),
                    egui::vec2(diagram_w, 150.0),
                    self.selected_count().clamp(2, 4),
                );
            });
            ui.add_space(12.0);
            bullet_line(ui, theme, t.intro_bullet1);
            bullet_line(ui, theme, t.intro_bullet2);
            bullet_line(ui, theme, t.intro_bullet3);
        });
    }

    fn draw_step_adapters(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                step_header(ui, theme, 2, t.step2_title, t.step2_subtitle);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pill_button(ui, theme, t.refresh, false).clicked() {
                        self.refresh_adapters();
                    }
                    if let Some(last) = self.last_refresh {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}s",
                                t.refreshed_ago,
                                last.elapsed().as_secs()
                            ))
                            .size(11.5)
                            .color(theme.text_muted),
                        );
                    }
                });
            });
            ui.add_space(8.0);
            wrapped_label(ui, t.adapters_help, 13.0, theme.text_muted);
            ui.add_space(12.0);

            // Toolbar: count summary + show-all toggle
            ui.horizontal(|ui| {
                let count = self.selected_count();
                let summary = if count == 0 {
                    t.adapters_none_selected.to_string()
                } else {
                    format!("{} {}", count, t.adapters_n_selected)
                };
                tinted_pill(ui, theme, &summary, theme.primary_soft, theme.primary);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.show_all_adapters {
                        t.hide_extra
                    } else {
                        t.show_all
                    };
                    if pill_button(ui, theme, label, self.show_all_adapters).clicked() {
                        self.show_all_adapters = !self.show_all_adapters;
                    }
                    let hidden = self.hidden_adapter_count();
                    if hidden > 0 && !self.show_all_adapters {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!("({} {})", hidden, t.hidden_adapters_n))
                                .size(11.5)
                                .color(theme.text_muted),
                        );
                    }
                });
            });
            ui.add_space(10.0);

            let visible = self.visible_adapter_indices();
            if visible.is_empty() {
                let _ = empty_state(ui, theme, t.no_adapters);
            } else {
                for index in visible {
                    self.draw_adapter_card(ui, theme, t, index);
                    ui.add_space(6.0);
                }
            }
        });
    }

    fn draw_adapter_card(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>, index: usize) {
        let row = &mut self.adapters[index];
        let selected = row.selected;
        let fill = if selected {
            theme.primary_soft
        } else {
            theme.surface_alt
        };
        let stroke = egui::Stroke::new(
            if selected { 1.6 } else { 1.0 },
            if selected {
                theme.primary
            } else {
                theme.border
            },
        );

        let inner = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(10)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    check_dot(ui, theme, selected);
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(trim_middle(&row.adapter.name, 24))
                            .size(13.5)
                            .strong()
                            .color(theme.text),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(row.adapter.ip.to_string())
                            .size(12.5)
                            .monospace()
                            .color(theme.text_muted),
                    );

                    let mut drag_rect: Option<egui::Rect> = None;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if selected {
                            let resp = ui.add(
                                egui::DragValue::new(&mut row.weight)
                                    .range(1..=100)
                                    .speed(1.0)
                                    .prefix(format!("{} ", t.weight)),
                            );
                            drag_rect = Some(resp.rect);
                            ui.add_space(8.0);
                        }
                        scope_pill(ui, theme, row.adapter.scope_label());
                    });
                    drag_rect
                })
                .inner
            });

        let drag_rect = inner.inner;
        let response = ui.interact(
            inner.response.rect,
            ui.id().with(("adapter-card", index)),
            egui::Sense::click(),
        );
        let on_drag = response
            .interact_pointer_pos()
            .zip(drag_rect)
            .map(|(pos, rect)| rect.contains(pos))
            .unwrap_or(false);
        if response.clicked() && !on_drag {
            self.adapters[index].selected = !selected;
        }
        if response.hovered() && !on_drag {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    fn draw_step_mode(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            step_header(ui, theme, 3, t.step3_title, t.step3_subtitle);
            ui.add_space(10.0);
            self.draw_mode_card(
                ui,
                theme,
                ModeCardSpec {
                    mode: Mode::Proxy,
                    icon: "🔌",
                    title: t.mode_proxy_title,
                    body: t.mode_proxy_body,
                    suited_for: t.mode_proxy_for,
                },
            );
            ui.add_space(10.0);
            self.draw_mode_card(
                ui,
                theme,
                ModeCardSpec {
                    mode: Mode::Vpn,
                    icon: "🛡",
                    title: t.mode_vpn_title,
                    body: t.mode_vpn_body,
                    suited_for: t.mode_vpn_for,
                },
            );
        });
    }

    fn draw_mode_card(&mut self, ui: &mut egui::Ui, theme: &Theme, spec: ModeCardSpec<'_>) {
        let ModeCardSpec {
            mode,
            icon,
            title,
            body,
            suited_for,
        } = spec;
        let active = self.mode == mode;
        let fill = if active {
            theme.primary_soft
        } else {
            theme.surface_alt
        };
        let stroke = egui::Stroke::new(
            if active { 1.8 } else { 1.0 },
            if active { theme.primary } else { theme.border },
        );

        let inner = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(12)
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                // Title row
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(icon).size(22.0).color(theme.primary));
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(title)
                            .size(15.0)
                            .strong()
                            .color(theme.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if active {
                            tinted_pill(ui, theme, "✓ 선택됨", theme.primary, egui::Color32::WHITE);
                        }
                    });
                });
                ui.add_space(8.0);
                wrapped_label(ui, body, 13.0, theme.text);
                ui.add_space(4.0);
                wrapped_label(ui, suited_for, 11.5, theme.text_muted);
            });

        let response = ui.interact(
            inner.response.rect,
            ui.id().with((
                "mode-card",
                match mode {
                    Mode::Proxy => "proxy",
                    Mode::Vpn => "vpn",
                },
            )),
            egui::Sense::click(),
        );
        if response.clicked() {
            self.mode = mode;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    fn draw_step_settings(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                let (title, subtitle) = match self.mode {
                    Mode::Proxy => (t.step4_proxy_title, t.step4_proxy_subtitle),
                    Mode::Vpn => (t.step4_vpn_title, t.step4_vpn_subtitle),
                };
                step_header(ui, theme, 4, title, subtitle);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.show_advanced {
                        t.hide_advanced
                    } else {
                        t.show_advanced
                    };
                    if pill_button(ui, theme, label, self.show_advanced).clicked() {
                        self.show_advanced = !self.show_advanced;
                    }
                });
            });
            ui.add_space(12.0);

            match self.mode {
                Mode::Proxy => self.draw_proxy_settings(ui, theme, t),
                Mode::Vpn => self.draw_vpn_settings(ui, theme, t),
            }
        });
    }

    fn draw_proxy_settings(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        ui.horizontal(|ui| {
            labeled_field(ui, theme, t.listen_address, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.listen_ip).desired_width(170.0));
            });
            ui.add_space(12.0);
            labeled_field(ui, theme, t.port, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.listen_port).desired_width(86.0));
            });
        });
        ui.add_space(6.0);
        wrapped_label(ui, t.listen_help, 11.5, theme.text_muted);

        if self.show_advanced {
            ui.add_space(14.0);
            divider(ui, theme);
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(t.advanced_title)
                    .size(13.0)
                    .strong()
                    .color(theme.text),
            );
            ui.add_space(6.0);
            ui.checkbox(&mut self.udp_enabled, t.udp_associate);
            ui.add_space(2.0);
            wrapped_label(ui, t.udp_associate_help, 11.0, theme.text_muted);
        }
    }

    fn draw_vpn_settings(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        ui.checkbox(&mut self.setup_routes, t.configure_routes);
        ui.add_space(2.0);
        wrapped_label(ui, t.configure_routes_help, 11.0, theme.text_muted);

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t.dns)
                    .size(12.5)
                    .color(theme.text_muted),
            );
            ui.add_space(6.0);
            egui::ComboBox::from_id_salt("dns_combo")
                .selected_text(self.dns_strategy.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.dns_strategy, DnsStrategy::Virtual, "virtual");
                    ui.selectable_value(&mut self.dns_strategy, DnsStrategy::OverTcp, "over-tcp");
                    ui.selectable_value(&mut self.dns_strategy, DnsStrategy::Direct, "direct");
                });
        });
        ui.add_space(2.0);
        wrapped_label(ui, t.dns_help, 11.0, theme.text_muted);

        if self.show_advanced {
            ui.add_space(14.0);
            divider(ui, theme);
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(t.advanced_title)
                    .size(13.0)
                    .strong()
                    .color(theme.text),
            );
            ui.add_space(8.0);

            ui.checkbox(&mut self.enable_ipv6, t.enable_ipv6);
            ui.add_space(8.0);
            ui.checkbox(&mut self.udp_enabled, t.udp_associate);
            ui.add_space(2.0);
            wrapped_label(ui, t.udp_associate_help, 11.0, theme.text_muted);

            ui.add_space(10.0);
            labeled_field(ui, theme, t.tun2proxy_path, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.tun2proxy_path)
                        .hint_text(t.tun2proxy_hint)
                        .desired_width(f32::INFINITY),
                );
            });

            ui.add_space(10.0);
            labeled_field(ui, theme, t.bypass_cidrs, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.bypass_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(2.0);
            wrapped_label(ui, t.bypass_help, 11.0, theme.text_muted);

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                labeled_field(ui, theme, t.internal_listen_address, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.listen_ip).desired_width(170.0));
                });
                ui.add_space(12.0);
                labeled_field(ui, theme, t.internal_port, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.listen_port).desired_width(86.0));
                });
            });
            ui.add_space(2.0);
            wrapped_label(ui, t.internal_listen_help, 11.0, theme.text_muted);
        }
    }

    fn draw_step_run(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            step_header(ui, theme, 5, t.step5_title, t.step5_subtitle);
            ui.add_space(14.0);

            let running = self.is_running();
            let mode_label = match self.mode {
                Mode::Proxy => t.mode_proxy_title,
                Mode::Vpn => t.mode_vpn_title,
            };

            // Status row: pulse + title only (no overflow risk)
            ui.horizontal(|ui| {
                draw_pulse_indicator(ui, theme, self.started_at.elapsed().as_secs_f32(), running);
                ui.add_space(12.0);
                let title = if running {
                    format!("{} / {}", t.status_running, mode_label)
                } else {
                    t.status_ready.to_string()
                };
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(theme.text),
                );
            });
            ui.add_space(4.0);

            // Detail line - full width, wraps cleanly
            let detail = if running {
                format!(
                    "{}:{}  /  {} {}",
                    self.listen_ip,
                    self.listen_port,
                    self.selected_count(),
                    t.adapters_n_selected
                )
            } else if self.selected_count() == 0 {
                t.run_help_no_adapter.to_string()
            } else {
                format!(
                    "{}  /  {}:{}",
                    t.run_help_ready, self.listen_ip, self.listen_port
                )
            };
            wrapped_label(ui, &detail, 12.0, theme.text_muted);

            ui.add_space(14.0);

            // Action button row - full width, button on the right
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if danger_button(ui, theme, t.stop).clicked() {
                            self.stop_all();
                        }
                    } else {
                        let enabled = self.selected_count() > 0;
                        let label = match self.mode {
                            Mode::Proxy => t.start_proxy,
                            Mode::Vpn => t.start_vpn,
                        };
                        if primary_button(ui, theme, label, enabled).clicked() {
                            self.start_current_mode();
                        }
                    }
                });
            });

            if matches!(self.mode, Mode::Vpn) && !running {
                ui.add_space(10.0);
                warning_callout(ui, theme, t.vpn_admin_warning);
            }
        });
    }

    fn draw_connections_section(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(t.connections_title)
                            .size(16.0)
                            .strong()
                            .color(theme.text),
                    );
                    ui.label(
                        egui::RichText::new(t.connections_subtitle)
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pill_button(ui, theme, t.open_connection_monitor, false).clicked() {
                        self.show_connection_window = true;
                    }
                    tinted_pill(
                        ui,
                        theme,
                        &format!(
                            "{} {}",
                            self.recent_connection_count(),
                            t.connections_recent
                        ),
                        theme.surface_alt,
                        theme.text_muted,
                    );
                    tinted_pill(
                        ui,
                        theme,
                        &format!(
                            "{} {}",
                            self.active_connection_count(),
                            t.connections_active
                        ),
                        theme.success_soft,
                        theme.success,
                    );
                });
            });
            ui.add_space(10.0);

            if self.connections.is_empty() {
                let response = empty_state(ui, theme, t.connections_empty);
                if response.clicked() {
                    self.show_connection_window = true;
                }
                return;
            }

            let response = egui::Frame::new()
                .fill(theme.surface_alt)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(10)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("connections_scroll")
                        .max_height(260.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for row in &self.connections {
                                draw_connection_row(ui, theme, t, row);
                                ui.add_space(6.0);
                            }
                        });
                })
                .response;
            if response.clicked() {
                self.show_connection_window = true;
            }
        });
    }

    fn draw_connection_monitor_window(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        t: &Texts<'_>,
    ) {
        if !self.show_connection_window {
            return;
        }

        let viewport_id = connection_monitor_viewport_id();
        let builder = egui::ViewportBuilder::default()
            .with_title(t.connection_monitor_title)
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([760.0, 420.0]);

        let close_requested = ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
            apply_theme_visuals(ctx, theme);
            let close_requested = ctx.input(|input| input.viewport().close_requested());
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme.bg))
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                    ui.add_space(4.0);
                    ui.heading(
                        egui::RichText::new(t.connection_monitor_title)
                            .size(18.0)
                            .color(theme.text),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if pill_button(
                            ui,
                            theme,
                            &format!(
                                "{} {}",
                                self.active_connection_count(),
                                t.connections_active
                            ),
                            self.connection_monitor_filter == ConnectionMonitorFilter::Active,
                        )
                        .clicked()
                        {
                            self.connection_monitor_filter = ConnectionMonitorFilter::Active;
                        }
                        if pill_button(
                            ui,
                            theme,
                            &format!(
                                "{} {}",
                                self.recent_connection_count(),
                                t.connections_recent
                            ),
                            self.connection_monitor_filter == ConnectionMonitorFilter::Recent,
                        )
                        .clicked()
                        {
                            self.connection_monitor_filter = ConnectionMonitorFilter::Recent;
                        }
                        if pill_button(
                            ui,
                            theme,
                            &format!("{} {}", self.connections.len(), t.connections_all),
                            self.connection_monitor_filter == ConnectionMonitorFilter::All,
                        )
                        .clicked()
                        {
                            self.connection_monitor_filter = ConnectionMonitorFilter::All;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if pill_button(ui, theme, t.clear_all_connections, false).clicked() {
                                self.connections.clear();
                            }
                            if pill_button(ui, theme, t.clear_recent_connections, false).clicked() {
                                self.connections.retain(|row| row.closed_at.is_none());
                            }
                        });
                    });
                    ui.add_space(8.0);
                    self.draw_connection_table(ui, theme, t);
                });
            close_requested
        });

        if close_requested {
            self.show_connection_window = false;
            ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Close);
        }
    }

    fn draw_connection_table(&self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        let visible_count = self.connection_count_for(self.connection_monitor_filter);
        egui::Frame::new()
            .fill(theme.log_bg)
            .stroke(egui::Stroke::new(1.0, theme.border))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                if visible_count == 0 {
                    ui.add_space(18.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(t.connections_empty)
                                .size(13.0)
                                .color(theme.text_muted),
                        );
                    });
                    return;
                }

                egui::ScrollArea::both()
                    .id_salt("connection_monitor_table_scroll")
                    .auto_shrink([false, false])
                    .max_width(ui.available_width())
                    .max_height(ui.available_height())
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        egui::Grid::new("connection_monitor_table")
                            .striped(true)
                            .num_columns(11)
                            .min_col_width(56.0)
                            .spacing(egui::vec2(12.0, 1.0))
                            .show(ui, |ui| {
                                table_header(ui, theme, t.column_id);
                                table_header(ui, theme, t.column_state);
                                table_header(ui, theme, t.column_proto);
                                table_header(ui, theme, t.column_age);
                                table_header(ui, theme, t.column_client);
                                table_header(ui, theme, t.column_target);
                                table_header(ui, theme, t.column_adapter);
                                table_header(ui, theme, t.column_egress_ip);
                                table_header(ui, theme, t.column_up);
                                table_header(ui, theme, t.column_down);
                                table_header(ui, theme, t.column_reason);
                                ui.end_row();

                                for row in self.connections.iter().filter(|row| {
                                    connection_matches_filter(row, self.connection_monitor_filter)
                                }) {
                                    draw_connection_table_row(ui, theme, t, row);
                                }
                            });
                    });
            });
    }

    fn draw_tray_options_window(&mut self, ctx: &egui::Context, theme: &Theme, t: &Texts<'_>) {
        if !self.show_tray_options {
            return;
        }

        let mut open = self.show_tray_options;
        let mut quit = false;
        egui::Window::new(t.options_title)
            .open(&mut open)
            .resizable(true)
            .default_size(egui::vec2(460.0, 420.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(t.tray_section_title)
                        .size(14.0)
                        .strong()
                        .color(theme.text),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.close_to_tray, t.close_to_tray);
                wrapped_label(ui, t.close_to_tray_help, 11.0, theme.text_muted);
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{}: {}",
                        t.tray_status,
                        if self.tray_available {
                            t.tray_available
                        } else {
                            t.tray_unavailable
                        }
                    ))
                    .size(12.0)
                    .color(theme.text_muted),
                );

                ui.add_space(14.0);
                divider(ui, theme);
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(t.quick_options)
                        .size(14.0)
                        .strong()
                        .color(theme.text),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.show_all_adapters, t.show_all);
                ui.checkbox(&mut self.udp_enabled, t.udp_associate);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t.language)
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                    if pill_button(
                        ui,
                        theme,
                        "English",
                        matches!(self.language, Language::English),
                    )
                    .clicked()
                    {
                        self.language = Language::English;
                    }
                    if pill_button(
                        ui,
                        theme,
                        "한국어",
                        matches!(self.language, Language::Korean),
                    )
                    .clicked()
                    {
                        self.language = Language::Korean;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(t.theme)
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                    if pill_button(
                        ui,
                        theme,
                        t.light,
                        matches!(self.theme_mode, ThemeMode::Light),
                    )
                    .clicked()
                    {
                        self.theme_mode = ThemeMode::Light;
                    }
                    if pill_button(
                        ui,
                        theme,
                        t.dark,
                        matches!(self.theme_mode, ThemeMode::Dark),
                    )
                    .clicked()
                    {
                        self.theme_mode = ThemeMode::Dark;
                    }
                });

                ui.add_space(14.0);
                divider(ui, theme);
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(t.update_title)
                        .size(14.0)
                        .strong()
                        .color(theme.text),
                );
                wrapped_label(
                    ui,
                    &format!("{}: {}", t.update_repo, crate::update::repo_label()),
                    11.0,
                    theme.text_muted,
                );
                wrapped_label(ui, &self.update_status, 11.0, theme.text_muted);
                ui.horizontal(|ui| {
                    if pill_button(ui, theme, t.update_check, false).clicked() {
                        self.start_update_check();
                    }
                    if primary_button(ui, theme, t.update_install, !self.update_busy).clicked() {
                        self.start_update_install();
                    }
                });

                ui.add_space(14.0);
                divider(ui, theme);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, theme, t.open_connection_monitor, true).clicked() {
                        self.show_connection_window = true;
                    }
                    if danger_button(ui, theme, t.quit_app).clicked() {
                        quit = true;
                    }
                });
            });
        self.show_tray_options = open;

        if quit {
            self.allow_exit = true;
            self.stop_all();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn draw_logs_section(&mut self, ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>) {
        section_card(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                let arrow = if self.show_logs { "▾" } else { "▸" };
                if pill_button(
                    ui,
                    theme,
                    &format!("{} {}", arrow, t.logs_title),
                    self.show_logs,
                )
                .clicked()
                {
                    self.show_logs = !self.show_logs;
                }
                ui.label(
                    egui::RichText::new(format!("({})", self.logs.len()))
                        .size(11.5)
                        .color(theme.text_muted),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pill_button(ui, theme, t.clear, false).clicked() {
                        self.logs.clear();
                    }
                });
            });

            ui.add_space(6.0);
            wrapped_label(
                ui,
                &format!("{}: {}", t.log_file, self.log_path.display()),
                11.0,
                theme.text_muted,
            );

            if self.show_logs {
                ui.add_space(8.0);
                let log_bg = theme.log_bg;
                egui::Frame::new()
                    .fill(log_bg)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("logs_scroll")
                            .stick_to_bottom(true)
                            .max_height(200.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.logs.is_empty() {
                                    ui.label(
                                        egui::RichText::new(t.no_logs)
                                            .size(12.5)
                                            .color(theme.text_muted),
                                    );
                                }
                                for line in &self.logs {
                                    ui.label(
                                        egui::RichText::new(line)
                                            .size(12.0)
                                            .monospace()
                                            .color(theme.log_text),
                                    );
                                }
                            });
                    });
            }
        });
    }
}

// =====================================================================
// Theme
// =====================================================================

#[derive(Clone)]
struct Theme {
    bg: egui::Color32,
    surface: egui::Color32,
    surface_alt: egui::Color32,
    border: egui::Color32,
    text: egui::Color32,
    text_muted: egui::Color32,
    primary: egui::Color32,
    primary_soft: egui::Color32,
    success: egui::Color32,
    success_soft: egui::Color32,
    warning: egui::Color32,
    warning_soft: egui::Color32,
    danger: egui::Color32,
    log_bg: egui::Color32,
    log_text: egui::Color32,
}

impl Theme {
    fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Light => Self {
                bg: egui::Color32::from_rgb(244, 246, 251),
                surface: egui::Color32::from_rgb(255, 255, 255),
                surface_alt: egui::Color32::from_rgb(244, 247, 252),
                border: egui::Color32::from_rgb(225, 231, 240),
                text: egui::Color32::from_rgb(15, 23, 42),
                text_muted: egui::Color32::from_rgb(100, 116, 139),
                primary: egui::Color32::from_rgb(99, 102, 241),
                primary_soft: egui::Color32::from_rgb(238, 242, 255),
                success: egui::Color32::from_rgb(16, 185, 129),
                success_soft: egui::Color32::from_rgb(220, 252, 231),
                warning: egui::Color32::from_rgb(217, 119, 6),
                warning_soft: egui::Color32::from_rgb(254, 243, 199),
                danger: egui::Color32::from_rgb(220, 38, 38),
                log_bg: egui::Color32::from_rgb(248, 250, 252),
                log_text: egui::Color32::from_rgb(51, 65, 85),
            },
            ThemeMode::Dark => Self {
                bg: egui::Color32::from_rgb(11, 15, 26),
                surface: egui::Color32::from_rgb(19, 24, 38),
                surface_alt: egui::Color32::from_rgb(27, 33, 51),
                border: egui::Color32::from_rgb(43, 53, 76),
                text: egui::Color32::from_rgb(232, 236, 245),
                text_muted: egui::Color32::from_rgb(143, 152, 173),
                primary: egui::Color32::from_rgb(129, 140, 248),
                primary_soft: egui::Color32::from_rgb(34, 41, 71),
                success: egui::Color32::from_rgb(52, 211, 153),
                success_soft: egui::Color32::from_rgb(13, 41, 36),
                warning: egui::Color32::from_rgb(251, 191, 36),
                warning_soft: egui::Color32::from_rgb(54, 36, 9),
                danger: egui::Color32::from_rgb(248, 113, 113),
                log_bg: egui::Color32::from_rgb(13, 18, 30),
                log_text: egui::Color32::from_rgb(190, 200, 220),
            },
        }
    }
}

fn apply_theme_visuals(ctx: &egui::Context, theme: &Theme) {
    let dark = theme.bg.r() < 80;
    ctx.all_styles_mut(|style| {
        style.visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        style.visuals.panel_fill = theme.bg;
        style.visuals.window_fill = theme.surface;
        style.visuals.extreme_bg_color = theme.surface;
        style.visuals.text_edit_bg_color = Some(theme.surface_alt);
        style.visuals.hyperlink_color = theme.primary;
        style.visuals.override_text_color = Some(theme.text);
        style.visuals.selection.bg_fill = theme.primary_soft;
        style.visuals.selection.stroke = egui::Stroke::new(1.0, theme.primary);
        style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, theme.border);
        style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, theme.text);
        style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.inactive.weak_bg_fill = theme.surface_alt;
        style.visuals.widgets.inactive.bg_fill = theme.surface_alt;
        style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, theme.border);
        style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, theme.text);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.hovered.weak_bg_fill = theme.primary_soft;
        style.visuals.widgets.hovered.bg_fill = theme.primary_soft;
        style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, theme.primary);
        style.visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, theme.text);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);
        style.visuals.widgets.active.bg_fill = theme.primary_soft;
        style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, theme.primary);
        style.visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, theme.text);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(8);
        style.visuals.window_corner_radius = egui::CornerRadius::same(10);
        style.visuals.popup_shadow = egui::Shadow {
            offset: [0, 6],
            blur: 20,
            spread: 0,
            color: egui::Color32::from_black_alpha(if dark { 120 } else { 28 }),
        };
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size = egui::vec2(36.0, 32.0);
        style.wrap_mode = Some(egui::TextWrapMode::Wrap);
        style.animation_time = 0.16;
    });
}

fn install_base_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(20.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(11.5, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(12.5, egui::FontFamily::Monospace),
        );
    });
}

// =====================================================================
// Reusable widgets
// =====================================================================

fn section_card(theme: &Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.surface)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(12)
        .inner_margin(egui::Margin::same(20))
        .shadow(egui::Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 0,
            color: egui::Color32::from_black_alpha(8),
        })
}

fn step_header(ui: &mut egui::Ui, theme: &Theme, number: usize, title: &str, subtitle: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 14.0, theme.primary_soft);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            number.to_string(),
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
            theme.primary,
        );
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(theme.text),
            );
            ui.label(
                egui::RichText::new(subtitle)
                    .size(12.0)
                    .color(theme.text_muted),
            );
        });
    });
}

fn draw_step_chip(
    ui: &mut egui::Ui,
    theme: &Theme,
    number: usize,
    label: &str,
    active: bool,
    done: bool,
) {
    let (fill, fg) = if active {
        (theme.primary, egui::Color32::WHITE)
    } else if done {
        (theme.success_soft, theme.success)
    } else {
        (theme.surface_alt, theme.text_muted)
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(100)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .stroke(egui::Stroke::new(1.0, theme.border))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let badge = if done {
                    "✓".to_string()
                } else {
                    number.to_string()
                };
                ui.label(egui::RichText::new(badge).size(11.5).strong().color(fg));
                ui.label(egui::RichText::new(label).size(11.5).color(fg));
            });
        });
}

fn bullet_line(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    let avail = ui.available_width();
    ui.horizontal_top(|ui| {
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("•")
                .size(13.0)
                .strong()
                .color(theme.primary),
        );
        ui.add_space(6.0);
        let text_width = (avail - 24.0).max(120.0);
        ui.allocate_ui_with_layout(
            egui::vec2(text_width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(text).size(12.5).color(theme.text)).wrap(),
                );
            },
        );
    });
    ui.add_space(2.0);
}

fn wrapped_label(ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32) {
    let avail = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(avail, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).size(size).color(color)).wrap());
        },
    );
}

fn primary_button(ui: &mut egui::Ui, theme: &Theme, text: &str, enabled: bool) -> egui::Response {
    let fill = if enabled {
        theme.primary
    } else {
        mix(theme.primary, theme.surface_alt, 0.5)
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(
            egui::RichText::new(text)
                .color(egui::Color32::WHITE)
                .strong()
                .size(13.5),
        )
        .fill(fill)
        .stroke(egui::Stroke::NONE)
        .corner_radius(10)
        .min_size(egui::vec2(140.0, 38.0)),
    )
}

fn danger_button(ui: &mut egui::Ui, theme: &Theme, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .color(egui::Color32::WHITE)
                .strong()
                .size(13.5),
        )
        .fill(theme.danger)
        .stroke(egui::Stroke::NONE)
        .corner_radius(10)
        .min_size(egui::vec2(110.0, 38.0)),
    )
}

fn pill_button(ui: &mut egui::Ui, theme: &Theme, text: &str, active: bool) -> egui::Response {
    let (fill, color, stroke) = if active {
        (
            theme.primary_soft,
            theme.primary,
            egui::Stroke::new(1.0, theme.primary),
        )
    } else {
        (
            theme.surface_alt,
            theme.text,
            egui::Stroke::new(1.0, theme.border),
        )
    };
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(12.5).color(color))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(100)
            .min_size(egui::vec2(0.0, 28.0)),
    )
}

fn tinted_pill(
    ui: &mut egui::Ui,
    _theme: &Theme,
    text: &str,
    fill: egui::Color32,
    color: egui::Color32,
) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(100)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.5).strong().color(color));
        });
}

fn scope_pill(ui: &mut egui::Ui, theme: &Theme, scope: &str) {
    let (fill, color) = match scope {
        "public" => (theme.success_soft, theme.success),
        "private" => (theme.primary_soft, theme.primary),
        "loopback" => (theme.surface_alt, theme.text_muted),
        _ => (theme.warning_soft, theme.warning),
    };
    tinted_pill(ui, theme, scope, fill, color);
}

fn check_dot(ui: &mut egui::Ui, theme: &Theme, selected: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 6, theme.primary);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "✓",
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
            egui::Color32::WHITE,
        );
    } else {
        painter.rect_filled(rect, 6, theme.surface);
        painter.rect_stroke(
            rect,
            6,
            egui::Stroke::new(1.5, theme.border),
            egui::StrokeKind::Inside,
        );
    }
}

fn labeled_field<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(11.5)
                .strong()
                .color(theme.text_muted),
        );
        ui.add_space(2.0);
        body(ui)
    })
    .inner
}

fn divider(ui: &mut egui::Ui, theme: &Theme) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme.border);
}

fn warning_callout(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    egui::Frame::new()
        .fill(theme.warning_soft)
        .stroke(egui::Stroke::new(1.0, theme.warning))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new("⚠").size(14.0).color(theme.warning));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(text).size(12.0).color(theme.text));
            });
        });
}

fn empty_state(ui: &mut egui::Ui, theme: &Theme, text: &str) -> egui::Response {
    egui::Frame::new()
        .fill(theme.surface_alt)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(8)
        .inner_margin(egui::Margin::same(20))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(text).size(12.5).color(theme.text_muted));
            });
        })
        .response
}

fn draw_connection_row(ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>, row: &ConnectionRow) {
    let active = row.closed_at.is_none();
    let elapsed_until = row.closed_at.unwrap_or_else(Instant::now);
    let age = elapsed_until.duration_since(row.opened_at);
    let status_label = if active {
        t.connections_active
    } else {
        t.connections_recent
    };
    let (status_fill, status_color) = if active {
        (theme.success_soft, theme.success)
    } else {
        (theme.surface_alt, theme.text_muted)
    };

    egui::Frame::new()
        .fill(theme.surface)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                tinted_pill(
                    ui,
                    theme,
                    row.protocol.label(),
                    theme.primary_soft,
                    theme.primary,
                );
                tinted_pill(ui, theme, status_label, status_fill, status_color);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{}: {}", t.connection_target, row.target))
                            .size(13.0)
                            .strong()
                            .monospace()
                            .color(theme.text),
                    );
                    ui.add_space(3.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}: {} ({})",
                                t.connection_adapter, row.egress_name, row.egress_ip
                            ))
                            .size(11.5)
                            .color(theme.text_muted),
                        );
                        ui.label(
                            egui::RichText::new(format!("{}: {}", t.connection_client, row.client))
                                .size(11.5)
                                .color(theme.text_muted),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{}: {}",
                                t.connection_age,
                                format_duration(age)
                            ))
                            .size(11.5)
                            .color(theme.text_muted),
                        );
                        if !active || row.up_bytes > 0 || row.down_bytes > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}: {} up / {} down",
                                    t.connection_bytes,
                                    format_bytes(row.up_bytes),
                                    format_bytes(row.down_bytes)
                                ))
                                .size(11.5)
                                .color(theme.text_muted),
                            );
                        }
                    });
                    if !active && !row.reason.is_empty() {
                        ui.label(
                            egui::RichText::new(&row.reason)
                                .size(11.0)
                                .color(theme.text_muted),
                        );
                    }
                });
            });
        });
}

fn table_header(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .size(11.0)
                .strong()
                .monospace()
                .color(theme.text),
        )
        .wrap_mode(egui::TextWrapMode::Extend),
    );
}

fn draw_connection_table_row(ui: &mut egui::Ui, theme: &Theme, t: &Texts<'_>, row: &ConnectionRow) {
    let active = row.closed_at.is_none();
    let elapsed_until = row.closed_at.unwrap_or_else(Instant::now);
    let age = format_duration(elapsed_until.duration_since(row.opened_at));
    let state = if active {
        t.connections_active
    } else {
        t.connections_recent
    };
    let state_color = if active {
        theme.success
    } else {
        theme.text_muted
    };

    table_cell(ui, theme, &row.id, theme.text_muted);
    table_cell(ui, theme, state, state_color);
    table_cell(ui, theme, row.protocol.label(), theme.primary);
    table_cell(ui, theme, &age, theme.text_muted);
    table_cell(ui, theme, &row.client, theme.text_muted);
    table_cell(ui, theme, &row.target, theme.text);
    table_cell(ui, theme, &row.egress_name, theme.text);
    table_cell(ui, theme, &row.egress_ip.to_string(), theme.text_muted);
    table_cell(ui, theme, &format_bytes(row.up_bytes), theme.text_muted);
    table_cell(ui, theme, &format_bytes(row.down_bytes), theme.text_muted);
    table_cell(ui, theme, &row.reason, theme.text_muted);
    ui.end_row();
}

fn table_cell(ui: &mut egui::Ui, _theme: &Theme, text: &str, color: egui::Color32) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .size(11.0)
                .monospace()
                .color(color),
        )
        .wrap_mode(egui::TextWrapMode::Extend),
    );
}

fn connection_matches_filter(row: &ConnectionRow, filter: ConnectionMonitorFilter) -> bool {
    match filter {
        ConnectionMonitorFilter::Active => row.closed_at.is_none(),
        ConnectionMonitorFilter::Recent => row.closed_at.is_some(),
        ConnectionMonitorFilter::All => true,
    }
}

fn draw_status_pill(
    ui: &mut egui::Ui,
    theme: &Theme,
    status: &AppStatus,
    t: &Texts<'_>,
    active: bool,
) {
    let (fill, color, label) = match status {
        AppStatus::Idle => (
            theme.surface_alt,
            theme.text_muted,
            t.status_idle.to_string(),
        ),
        AppStatus::ProxyRunning => (
            theme.success_soft,
            theme.success,
            t.status_proxy_running.to_string(),
        ),
        AppStatus::VpnRunning => (
            theme.success_soft,
            theme.success,
            t.status_vpn_running.to_string(),
        ),
        AppStatus::AdapterRefreshFailed => (
            theme.warning_soft,
            theme.warning,
            t.status_adapter_failed.to_string(),
        ),
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(100)
        .inner_margin(egui::Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
                if active {
                    ui.painter()
                        .circle_stroke(rect.center(), 6.0, egui::Stroke::new(1.0, color));
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(label).size(12.0).strong().color(color));
            });
        });
}

fn draw_pulse_indicator(ui: &mut egui::Ui, theme: &Theme, seconds: f32, running: bool) {
    let size = 44.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let color = if running {
        theme.success
    } else {
        theme.text_muted
    };
    if running {
        let pulse = (seconds * 2.0).sin() * 0.5 + 0.5;
        let radius = 14.0 + pulse * 6.0;
        painter.circle_filled(
            center,
            radius,
            egui::Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                (60.0 + pulse * 60.0) as u8,
            ),
        );
    }
    painter.circle_filled(center, 12.0, color);
    painter.circle_filled(center, 4.5, egui::Color32::WHITE);
}

fn draw_combine_diagram(
    ui: &mut egui::Ui,
    theme: &Theme,
    seconds: f32,
    size: egui::Vec2,
    input_count: usize,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 12, theme.surface_alt);
    painter.rect_stroke(
        rect,
        12,
        egui::Stroke::new(1.0, theme.border),
        egui::StrokeKind::Inside,
    );

    let count = input_count.clamp(2, 5);
    let center_y = rect.center().y;
    let left_x = rect.left() + 28.0;
    let merge = egui::pos2(rect.left() + rect.width() * 0.6, center_y);
    let out = egui::pos2(rect.right() - 28.0, center_y);

    let mut inputs = Vec::with_capacity(count);
    let span = (rect.height() - 60.0).max(40.0);
    for i in 0..count {
        let t = if count == 1 {
            0.5
        } else {
            i as f32 / (count - 1) as f32
        };
        let y = rect.top() + 30.0 + t * span;
        inputs.push(egui::pos2(left_x, y));
    }

    for (index, input) in inputs.iter().enumerate() {
        let control = egui::pos2(rect.left() + rect.width() * 0.38, input.y);
        let path = cubic_points(*input, control, merge, 32);
        painter.add(egui::Shape::line(
            path.clone(),
            egui::Stroke::new(2.0, mix(theme.primary, theme.surface, 0.55)),
        ));
        let progress = ((seconds * 0.5) + index as f32 * 0.18).fract();
        let dot = sample_polyline(&path, progress);
        painter.circle_filled(dot, 4.0, theme.primary);
    }

    painter.line_segment([merge, out], egui::Stroke::new(2.6, theme.primary));
    let output_progress = (seconds * 0.55).fract();
    let output_dot = egui::pos2(merge.x + (out.x - merge.x) * output_progress, merge.y);
    painter.circle_filled(output_dot, 4.5, theme.success);

    for input in &inputs {
        painter.circle_filled(*input, 8.0, theme.surface);
        painter.circle_stroke(*input, 8.0, egui::Stroke::new(1.8, theme.primary));
        painter.circle_filled(*input, 3.0, theme.primary);
    }
    painter.circle_filled(merge, 12.0, theme.primary);
    painter.circle_filled(merge, 5.0, theme.surface);
    painter.circle_filled(out, 10.0, theme.success_soft);
    painter.circle_stroke(out, 10.0, egui::Stroke::new(1.8, theme.success));
    painter.circle_filled(out, 4.0, theme.success);
}

fn draw_logo_mark(ui: &mut egui::Ui, theme: &Theme, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8, theme.primary_soft);
    let center = rect.center();
    for y_offset in [-9.0, 0.0, 9.0] {
        let start = egui::pos2(rect.left() + 8.0, center.y + y_offset);
        let end = egui::pos2(center.x + 3.0, center.y);
        painter.line_segment([start, end], egui::Stroke::new(1.8, theme.primary));
        painter.circle_filled(start, 2.6, theme.primary);
    }
    painter.circle_filled(egui::pos2(rect.right() - 9.0, center.y), 4.5, theme.success);
}

// =====================================================================
// Background process glue
// =====================================================================

struct AdapterRow {
    adapter: AdapterAddress,
    selected: bool,
    weight: u16,
}

struct ConnectionRow {
    id: String,
    protocol: proxy::ConnectionProtocol,
    client: String,
    target: String,
    egress_name: String,
    egress_ip: IpAddr,
    opened_at: Instant,
    closed_at: Option<Instant>,
    up_bytes: u64,
    down_bytes: u64,
    reason: String,
}

impl From<proxy::ConnectionOpened> for ConnectionRow {
    fn from(opened: proxy::ConnectionOpened) -> Self {
        Self {
            id: opened.id,
            protocol: opened.protocol,
            client: opened.client.to_string(),
            target: opened.target,
            egress_name: opened.egress_name,
            egress_ip: opened.egress_ip,
            opened_at: opened.opened_at,
            closed_at: None,
            up_bytes: 0,
            down_bytes: 0,
            reason: String::new(),
        }
    }
}

struct ManagedProxy {
    cancel: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl ManagedProxy {
    fn start(
        config: ProxyConfig,
        log: mpsc::Sender<String>,
        connection_log: mpsc::Sender<proxy::ConnectionEvent>,
    ) -> Result<Self> {
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();
        let thread = thread::Builder::new()
            .name("net-combiner-proxy".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = log.send(format!("failed to create Tokio runtime: {error}"));
                        return;
                    }
                };

                if let Err(error) = runtime.block_on(proxy::run_proxy(
                    config,
                    thread_cancel,
                    log.clone(),
                    Some(connection_log),
                )) {
                    let _ = log.send(format!("proxy failed: {error}"));
                }
            })?;

        Ok(Self {
            cancel,
            thread: Some(thread),
        })
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    fn join_finished(&mut self) {
        if self.is_finished() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ManagedProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

// =====================================================================
// Localization
// =====================================================================

struct Texts<'a> {
    tagline: &'a str,
    step_label_intro: &'a str,
    step_label_adapters: &'a str,
    step_label_mode: &'a str,
    step_label_settings: &'a str,
    step_label_run: &'a str,
    step1_title: &'a str,
    step1_subtitle: &'a str,
    intro_bullet1: &'a str,
    intro_bullet2: &'a str,
    intro_bullet3: &'a str,
    step2_title: &'a str,
    step2_subtitle: &'a str,
    adapters_help: &'a str,
    refresh: &'a str,
    refreshed_ago: &'a str,
    no_adapters: &'a str,
    hidden_adapters_n: &'a str,
    show_all: &'a str,
    hide_extra: &'a str,
    adapters_none_selected: &'a str,
    adapters_n_selected: &'a str,
    weight: &'a str,
    step3_title: &'a str,
    step3_subtitle: &'a str,
    mode_proxy_title: &'a str,
    mode_proxy_body: &'a str,
    mode_proxy_for: &'a str,
    mode_vpn_title: &'a str,
    mode_vpn_body: &'a str,
    mode_vpn_for: &'a str,
    step4_proxy_title: &'a str,
    step4_proxy_subtitle: &'a str,
    step4_vpn_title: &'a str,
    step4_vpn_subtitle: &'a str,
    show_advanced: &'a str,
    hide_advanced: &'a str,
    listen_address: &'a str,
    listen_help: &'a str,
    internal_listen_address: &'a str,
    internal_port: &'a str,
    internal_listen_help: &'a str,
    port: &'a str,
    udp_associate: &'a str,
    udp_associate_help: &'a str,
    configure_routes: &'a str,
    configure_routes_help: &'a str,
    enable_ipv6: &'a str,
    dns: &'a str,
    dns_help: &'a str,
    tun2proxy_path: &'a str,
    tun2proxy_hint: &'a str,
    bypass_cidrs: &'a str,
    bypass_help: &'a str,
    advanced_title: &'a str,
    step5_title: &'a str,
    step5_subtitle: &'a str,
    status_ready: &'a str,
    status_running: &'a str,
    run_help_no_adapter: &'a str,
    run_help_ready: &'a str,
    start_proxy: &'a str,
    start_vpn: &'a str,
    stop: &'a str,
    vpn_admin_warning: &'a str,
    connections_title: &'a str,
    connections_subtitle: &'a str,
    connections_empty: &'a str,
    connections_active: &'a str,
    connections_recent: &'a str,
    connections_all: &'a str,
    connection_target: &'a str,
    connection_adapter: &'a str,
    connection_client: &'a str,
    connection_age: &'a str,
    connection_bytes: &'a str,
    open_connection_monitor: &'a str,
    connection_monitor_title: &'a str,
    clear_recent_connections: &'a str,
    clear_all_connections: &'a str,
    column_id: &'a str,
    column_state: &'a str,
    column_proto: &'a str,
    column_age: &'a str,
    column_client: &'a str,
    column_target: &'a str,
    column_adapter: &'a str,
    column_egress_ip: &'a str,
    column_up: &'a str,
    column_down: &'a str,
    column_reason: &'a str,
    options_title: &'a str,
    tray_section_title: &'a str,
    close_to_tray: &'a str,
    close_to_tray_help: &'a str,
    tray_status: &'a str,
    tray_available: &'a str,
    tray_unavailable: &'a str,
    quick_options: &'a str,
    update_title: &'a str,
    update_repo: &'a str,
    update_check: &'a str,
    update_install: &'a str,
    language: &'a str,
    theme: &'a str,
    light: &'a str,
    dark: &'a str,
    quit_app: &'a str,
    logs_title: &'a str,
    log_file: &'a str,
    no_logs: &'a str,
    clear: &'a str,
    status_idle: &'a str,
    status_proxy_running: &'a str,
    status_vpn_running: &'a str,
    status_adapter_failed: &'a str,
}

impl<'a> Texts<'a> {
    fn new(language: Language) -> Self {
        match language {
            Language::Korean => Self {
                tagline: "여러 네트워크 어댑터를 하나의 출구로 묶어주는 도구",
                step_label_intro: "소개",
                step_label_adapters: "어댑터",
                step_label_mode: "모드",
                step_label_settings: "설정",
                step_label_run: "실행",
                step1_title: "net-combiner 가 뭔가요?",
                step1_subtitle: "여러 인터넷 회선을 하나의 가상 회선처럼 사용해요.",
                intro_bullet1: "Wi-Fi, 유선랜, 휴대폰 테더링 같은 여러 네트워크를 동시에 활용합니다.",
                intro_bullet2: "각 연결(트래픽 흐름)을 어댑터에 분산해 전체 처리량을 늘립니다.",
                intro_bullet3: "프록시 모드는 앱 설정만으로, VPN 모드는 시스템 전체 트래픽을 라우팅합니다.",
                step2_title: "어댑터 선택",
                step2_subtitle: "사용할 네트워크 어댑터를 골라주세요. 여러 개 선택할 수 있습니다.",
                adapters_help: "어댑터는 컴퓨터의 네트워크 연결구입니다. Wi-Fi, 유선, USB 테더링 등이 각각 어댑터입니다. 골라둔 어댑터들로만 트래픽이 나갑니다.",
                refresh: "새로고침",
                refreshed_ago: "갱신",
                no_adapters: "표시할 어댑터가 없어요. '모두 보기'를 눌러보세요.",
                hidden_adapters_n: "숨김",
                show_all: "모두 보기",
                hide_extra: "기본만 보기",
                adapters_none_selected: "어댑터를 1개 이상 선택하세요",
                adapters_n_selected: "개 선택됨",
                weight: "가중치",
                step3_title: "모드 선택",
                step3_subtitle: "어떻게 트래픽을 보낼지 결정합니다.",
                mode_proxy_title: "프록시 모드",
                mode_proxy_body: "127.0.0.1:1080 에 SOCKS5 프록시를 띄웁니다. 브라우저나 앱에 프록시 주소를 입력해서 사용합니다.",
                mode_proxy_for: "권장: 특정 앱만 분산하고 싶을 때, 권한이 없을 때",
                mode_vpn_title: "VPN 모드",
                mode_vpn_body: "TUN 인터페이스를 만들고 시스템의 모든 트래픽을 가로채 분산합니다. 별도 설정이 필요 없습니다.",
                mode_vpn_for: "권장: 컴퓨터 전체 트래픽을 묶고 싶을 때 (관리자 권한 필요)",
                step4_proxy_title: "프록시 설정",
                step4_proxy_subtitle: "브라우저나 앱에 입력할 SOCKS5 프록시 주소를 정합니다.",
                step4_vpn_title: "VPN 설정",
                step4_vpn_subtitle: "TUN 인터페이스 동작 방식과 우회 정책을 정합니다.",
                show_advanced: "고급 설정 보기",
                hide_advanced: "고급 설정 숨기기",
                listen_address: "수신 주소",
                listen_help: "127.0.0.1 은 내 컴퓨터에서만 접근할 수 있다는 뜻입니다. 다른 기기에서 쓰려면 0.0.0.0 으로 바꾸세요.",
                internal_listen_address: "내부 수신 주소",
                internal_port: "내부 포트",
                internal_listen_help: "tun2proxy 가 내부적으로 연결할 SOCKS5 주소입니다. 보통 그대로 두세요.",
                port: "포트",
                udp_associate: "SOCKS5 UDP 지원",
                udp_associate_help: "DNS 조회나 일부 게임에 필요합니다. 켜두는 걸 권장합니다.",
                configure_routes: "시스템 라우트 자동 설정",
                configure_routes_help: "끄면 직접 라우팅 테이블을 설정해야 합니다. 보통은 켜둡니다.",
                enable_ipv6: "IPv6 라우팅 사용",
                dns: "DNS 처리 방식",
                dns_help: "virtual: 가상 IP 로 매핑 / over-tcp: TCP DNS 사용 / direct: 시스템 그대로",
                tun2proxy_path: "tun2proxy 실행 파일 경로",
                tun2proxy_hint: "비워두면 앱 옆 자동 탐색",
                bypass_cidrs: "우회 CIDR (한 줄에 하나)",
                bypass_help: "여기 입력한 대역은 VPN 을 거치지 않고 원래 경로로 나갑니다.",
                advanced_title: "고급 옵션",
                step5_title: "실행",
                step5_subtitle: "준비됐어요. 시작 버튼을 누르세요.",
                status_ready: "대기 중",
                status_running: "실행 중",
                run_help_no_adapter: "어댑터를 먼저 선택하세요.",
                run_help_ready: "시작할 준비가 되었습니다",
                start_proxy: "프록시 시작",
                start_vpn: "VPN 시작",
                stop: "중지",
                vpn_admin_warning: "VPN 모드는 관리자(또는 root) 권한이 필요합니다. tun2proxy 가 같은 폴더에 있어야 합니다.",
                connections_title: "연결 상태",
                connections_subtitle: "어떤 대상 IP가 어떤 어댑터를 통해 나가는지 실시간으로 보여줍니다.",
                connections_empty: "아직 연결된 대상이 없습니다.",
                connections_active: "활성",
                connections_recent: "최근",
                connections_all: "전체",
                connection_target: "대상",
                connection_adapter: "어댑터",
                connection_client: "클라이언트",
                connection_age: "시간",
                connection_bytes: "전송량",
                open_connection_monitor: "연결 모니터 열기",
                connection_monitor_title: "연결 모니터",
                clear_recent_connections: "최근 항목 지우기",
                clear_all_connections: "모두 지우기",
                column_id: "ID",
                column_state: "상태",
                column_proto: "프로토콜",
                column_age: "시간",
                column_client: "클라이언트",
                column_target: "대상",
                column_adapter: "어댑터",
                column_egress_ip: "출구 IP",
                column_up: "업로드",
                column_down: "다운로드",
                column_reason: "사유",
                options_title: "옵션",
                tray_section_title: "트레이",
                close_to_tray: "창을 닫으면 트레이로 보내기",
                close_to_tray_help: "종료는 트레이 메뉴의 종료를 사용하세요. 프록시/VPN은 백그라운드에서 계속 동작합니다.",
                tray_status: "트레이 상태",
                tray_available: "사용 가능",
                tray_unavailable: "사용 불가",
                quick_options: "빠른 옵션",
                update_title: "자동 업데이트",
                update_repo: "GitHub 저장소",
                update_check: "업데이트 확인",
                update_install: "최신 버전 설치",
                language: "언어",
                theme: "테마",
                light: "라이트",
                dark: "다크",
                quit_app: "프로그램 종료",
                logs_title: "로그",
                log_file: "로그 파일",
                no_logs: "아직 이벤트가 없습니다.",
                clear: "지우기",
                status_idle: "대기",
                status_proxy_running: "프록시 실행 중",
                status_vpn_running: "VPN 실행 중",
                status_adapter_failed: "어댑터 새로고침 실패",
            },
            Language::English => Self {
                tagline: "Combine multiple network adapters into one virtual exit.",
                step_label_intro: "Intro",
                step_label_adapters: "Adapters",
                step_label_mode: "Mode",
                step_label_settings: "Settings",
                step_label_run: "Run",
                step1_title: "What is net-combiner?",
                step1_subtitle: "Use several internet connections as if they were one.",
                intro_bullet1: "Use Wi-Fi, Ethernet, phone tethering and more at the same time.",
                intro_bullet2: "Each connection (flow) is spread across adapters to increase throughput.",
                intro_bullet3: "Proxy mode works per-app; VPN mode routes the whole system through it.",
                step2_title: "Pick your adapters",
                step2_subtitle: "Select the network adapters you want to use. Pick as many as you like.",
                adapters_help: "An adapter is a network interface on your computer (Wi-Fi, Ethernet, USB tether). Only the ones you pick will be used to send traffic.",
                refresh: "Refresh",
                refreshed_ago: "updated",
                no_adapters: "No adapters to show. Try 'Show all'.",
                hidden_adapters_n: "hidden",
                show_all: "Show all",
                hide_extra: "Show defaults",
                adapters_none_selected: "Select at least one adapter",
                adapters_n_selected: "selected",
                weight: "Weight",
                step3_title: "Choose a mode",
                step3_subtitle: "How should the traffic be sent?",
                mode_proxy_title: "Proxy mode",
                mode_proxy_body: "Runs a SOCKS5 proxy at 127.0.0.1:1080. Point your browser or app at it.",
                mode_proxy_for: "Best for: per-app routing, no admin needed",
                mode_vpn_title: "VPN mode",
                mode_vpn_body: "Creates a TUN interface and routes all system traffic through it, so apps don't need to know.",
                mode_vpn_for: "Best for: routing the whole machine (admin/root required)",
                step4_proxy_title: "Proxy settings",
                step4_proxy_subtitle: "Address your browser or app will connect to.",
                step4_vpn_title: "VPN settings",
                step4_vpn_subtitle: "How the TUN interface routes and resolves traffic.",
                show_advanced: "Show advanced",
                hide_advanced: "Hide advanced",
                listen_address: "Listen address",
                listen_help: "127.0.0.1 means only this machine can use it. Change to 0.0.0.0 to share with other devices.",
                internal_listen_address: "Internal listen address",
                internal_port: "Internal port",
                internal_listen_help: "Where tun2proxy connects internally. Usually leave as default.",
                port: "Port",
                udp_associate: "Enable SOCKS5 UDP",
                udp_associate_help: "Needed for DNS and some games. Recommended to keep on.",
                configure_routes: "Auto-configure system routes",
                configure_routes_help: "Off means you must edit the routing table yourself. Usually leave on.",
                enable_ipv6: "Enable IPv6 routing",
                dns: "DNS handling",
                dns_help: "virtual: map to fake IPs / over-tcp: TCP DNS / direct: leave system DNS",
                tun2proxy_path: "tun2proxy binary path",
                tun2proxy_hint: "Empty = auto-detect next to the app",
                bypass_cidrs: "Bypass CIDRs (one per line)",
                bypass_help: "Traffic to these ranges skips the VPN.",
                advanced_title: "Advanced",
                step5_title: "Run",
                step5_subtitle: "All set. Press start when you're ready.",
                status_ready: "Ready",
                status_running: "Running",
                run_help_no_adapter: "Select at least one adapter first.",
                run_help_ready: "Ready to start",
                start_proxy: "Start proxy",
                start_vpn: "Start VPN",
                stop: "Stop",
                vpn_admin_warning: "VPN mode needs administrator/root rights. tun2proxy should be next to the app.",
                connections_title: "Connection activity",
                connections_subtitle: "Live view of which target IP leaves through which selected adapter.",
                connections_empty: "No active or recent connections yet.",
                connections_active: "active",
                connections_recent: "recent",
                connections_all: "all",
                connection_target: "Target",
                connection_adapter: "Adapter",
                connection_client: "Client",
                connection_age: "Age",
                connection_bytes: "Bytes",
                open_connection_monitor: "Open monitor",
                connection_monitor_title: "Connection monitor",
                clear_recent_connections: "Clear recent",
                clear_all_connections: "Clear all",
                column_id: "ID",
                column_state: "State",
                column_proto: "Proto",
                column_age: "Age",
                column_client: "Client",
                column_target: "Target",
                column_adapter: "Adapter",
                column_egress_ip: "Egress IP",
                column_up: "Up",
                column_down: "Down",
                column_reason: "Reason",
                options_title: "Options",
                tray_section_title: "Tray",
                close_to_tray: "Close to tray",
                close_to_tray_help: "Use Quit from the tray menu to fully exit. Proxy/VPN keeps running while hidden.",
                tray_status: "Tray status",
                tray_available: "available",
                tray_unavailable: "unavailable",
                quick_options: "Quick options",
                update_title: "Auto update",
                update_repo: "GitHub repo",
                update_check: "Check updates",
                update_install: "Install latest",
                language: "Language",
                theme: "Theme",
                light: "Light",
                dark: "Dark",
                quit_app: "Quit app",
                logs_title: "Logs",
                log_file: "Log file",
                no_logs: "No events yet.",
                clear: "Clear",
                status_idle: "Idle",
                status_proxy_running: "Proxy running",
                status_vpn_running: "VPN running",
                status_adapter_failed: "Adapter refresh failed",
            },
        }
    }
}

// =====================================================================
// Fonts / icon
// =====================================================================

fn install_cjk_font_fallbacks(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in system_font_candidates() {
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let font_name = format!("system-{name}");
        fonts.font_data.insert(
            font_name.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push(font_name.clone());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(font_name);
    }
    ctx.set_fonts(fonts);
}

fn system_font_candidates() -> Vec<&'static Path> {
    let mut paths = Vec::new();

    #[cfg(target_os = "windows")]
    {
        paths.extend(
            [
                r"C:\Windows\Fonts\malgun.ttf",
                r"C:\Windows\Fonts\meiryo.ttc",
                r"C:\Windows\Fonts\msgothic.ttc",
                r"C:\Windows\Fonts\msyh.ttc",
                r"C:\Windows\Fonts\msjh.ttc",
                r"C:\Windows\Fonts\seguiemj.ttf",
            ]
            .into_iter()
            .map(Path::new),
        );
    }

    #[cfg(target_os = "macos")]
    {
        paths.extend(
            [
                "/System/Library/Fonts/AppleSDGothicNeo.ttc",
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/Hiragino Sans GB.ttc",
                "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
                "/System/Library/Fonts/Apple Color Emoji.ttc",
            ]
            .into_iter()
            .map(Path::new),
        );
    }

    #[cfg(target_os = "linux")]
    {
        paths.extend(
            [
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJKkr-Regular.otf",
                "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
                "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            ]
            .into_iter()
            .map(Path::new),
        );
    }

    paths
}

fn load_icon_data() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/net-combiner-icon.png")).ok()
}

fn open_log_file() -> (PathBuf, Option<fs::File>, Option<String>) {
    let path = default_log_path();
    let result = (|| -> std::io::Result<fs::File> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        rotate_log_if_large(&path)?;
        ensure_utf8_bom(&path)?;
        fs::OpenOptions::new().create(true).append(true).open(&path)
    })();

    match result {
        Ok(file) => (path, Some(file), None),
        Err(error) => (
            path.clone(),
            None,
            Some(format!(
                "failed to open log file {}: {error}",
                path.display()
            )),
        ),
    }
}

fn rotate_log_if_large(path: &Path) -> std::io::Result<()> {
    const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
    if !path.exists() || path.metadata()?.len() <= MAX_LOG_BYTES {
        return Ok(());
    }
    let rotated = path.with_extension("log.1");
    if rotated.exists() {
        fs::remove_file(&rotated)?;
    }
    fs::rename(path, rotated)?;
    Ok(())
}

fn ensure_utf8_bom(path: &Path) -> std::io::Result<()> {
    const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    if !path.exists() {
        let mut file = fs::File::create(path)?;
        file.write_all(UTF8_BOM)?;
        return Ok(());
    }

    let bytes = fs::read(path)?;
    if bytes.starts_with(UTF8_BOM) {
        return Ok(());
    }

    let mut file = fs::File::create(path)?;
    file.write_all(UTF8_BOM)?;
    file.write_all(&bytes)?;
    Ok(())
}

fn default_log_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("logs").join("net-combiner.log");
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("logs")
        .join("net-combiner.log")
}

// =====================================================================
// Small utilities
// =====================================================================

#[cfg(windows)]
fn log_timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    let mut time = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    unsafe {
        GetLocalTime(&mut time);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

#[cfg(not(windows))]
fn log_timestamp() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "0.000".to_owned(),
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0_usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn tray_language_for(language: Language) -> TrayLanguage {
    match language {
        Language::English => TrayLanguage::English,
        Language::Korean => TrayLanguage::Korean,
    }
}

fn connection_monitor_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("net-combiner-connection-monitor")
}

fn cubic_points(
    start: egui::Pos2,
    control: egui::Pos2,
    end: egui::Pos2,
    steps: usize,
) -> Vec<egui::Pos2> {
    (0..=steps)
        .map(|step| {
            let t = step as f32 / steps as f32;
            let a = (1.0 - t).powi(2);
            let b = 2.0 * (1.0 - t) * t;
            let c = t.powi(2);
            egui::pos2(
                a * start.x + b * control.x + c * end.x,
                a * start.y + b * control.y + c * end.y,
            )
        })
        .collect()
}

fn sample_polyline(points: &[egui::Pos2], t: f32) -> egui::Pos2 {
    if points.is_empty() {
        return egui::Pos2::ZERO;
    }
    let index = ((points.len() - 1) as f32 * t).round() as usize;
    points[index.min(points.len() - 1)]
}

fn mix(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| -> u8 {
        (x as f32 * (1.0 - t) + y as f32 * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

fn default_selected(_adapter: &AdapterAddress) -> bool {
    false
}

fn visible_by_default(adapter: &AdapterAddress) -> bool {
    !adapter.is_loopback && !adapter.is_link_local
}

fn trim_middle(value: &str, max_chars: usize) -> String {
    let len = value.chars().count();
    if len <= max_chars || max_chars < 5 {
        return value.to_owned();
    }
    let left_count = (max_chars - 1) / 2;
    let right_count = max_chars - 1 - left_count;
    let left: String = value.chars().take(left_count).collect();
    let right: String = value
        .chars()
        .rev()
        .take(right_count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{left}...{right}")
}
