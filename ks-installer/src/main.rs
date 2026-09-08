#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::FontFamily;

const DRIVER_SERVICE: &str = "ks-driver";
const BACKEND_SERVICE: &str = "KsService";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BUTTON_SIZE: egui::Vec2 = egui::Vec2::new(104.0, 26.0);
const STATUS_REFRESH: Duration = Duration::from_secs(3);

const STATE_RUNNING: &str = "RUNNING";
const STATE_STOPPED: &str = "STOPPED";
const STATE_UNKNOWN: &str = "UNKNOWN";

struct ScJob {
    args: Vec<String>,
    best_effort: bool,
}

fn sc_job(args: &[&str]) -> ScJob {
    ScJob {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        best_effort: false,
    }
}

fn sc_job_best_effort(args: &[&str]) -> ScJob {
    ScJob {
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        best_effort: true,
    }
}

type SharedState = Arc<Mutex<String>>;

struct InstallerApp {
    base_dir: PathBuf,
    elevated: bool,
    relaunch_started: bool,
    busy: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<String>>>,
    events: Receiver<String>,
    statuses: Receiver<(String, String)>,
    status_tx: std::sync::mpsc::Sender<(String, String)>,
    driver_state: SharedState,
    backend_state: SharedState,
    last_refresh: Instant,
}

impl InstallerApp {
    fn new() -> Self {
        let base_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let (_log_tx, events) = channel();
        let (status_tx, statuses) = channel();
        let mut app = Self {
            elevated: is_elevated(),
            base_dir,
            relaunch_started: false,
            busy: Arc::new(AtomicBool::new(false)),
            log: Arc::new(Mutex::new(Vec::new())),
            events,
            statuses,
            status_tx,
            driver_state: Arc::new(Mutex::new(STATE_UNKNOWN.to_owned())),
            backend_state: Arc::new(Mutex::new(STATE_UNKNOWN.to_owned())),
            last_refresh: Instant::now() - STATUS_REFRESH,
        };
        app.push(format!("Directory: {}", app.base_dir.display()));
        app.push(if app.elevated {
            "Running as administrator."
        } else {
            "Not running as administrator; service operations will fail."
        });
        app
    }

    fn push(&mut self, line: impl Into<String>) {
        if let Ok(mut log) = self.log.lock() {
            log.push(line.into());
        }
    }

    fn state_handle(&self, name: &str) -> SharedState {
        if name == DRIVER_SERVICE {
            Arc::clone(&self.driver_state)
        } else {
            Arc::clone(&self.backend_state)
        }
    }

    fn run_jobs(&mut self, ctx: &egui::Context, jobs: Vec<ScJob>) {
        if self.busy.swap(true, Ordering::SeqCst) {
            self.push("Another operation is in progress, please wait.");
            return;
        }
        let log = Arc::clone(&self.log);
        let busy = Arc::clone(&self.busy);
        let status_tx = self.status_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            for job in jobs {
                if let Ok(mut guard) = log.lock() {
                    guard.push(format!("> sc.exe {}", job.args.join(" ")));
                }
                match execute_sc(&job.args) {
                    Ok(text) => {
                        if let Ok(mut guard) = log.lock() {
                            guard.push(text);
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = log.lock() {
                            guard.push(if job.best_effort {
                                format!("[skipped] {error}")
                            } else {
                                format!("[error] {error}")
                            });
                        }
                    }
                }
            }
            for name in [DRIVER_SERVICE, BACKEND_SERVICE] {
                let state = query_service_state(name);
                let _ = status_tx.send((name.to_owned(), state));
            }
            busy.store(false, Ordering::SeqCst);
            repaint.request_repaint();
        });
    }

    fn refresh_statuses(&mut self) {
        if self.busy.load(Ordering::SeqCst) {
            return;
        }
        self.last_refresh = Instant::now();
        for name in [DRIVER_SERVICE, BACKEND_SERVICE] {
            let state_handle = self.state_handle(name);
            let status_tx = self.status_tx.clone();
            let name = name.to_owned();
            std::thread::spawn(move || {
                let state = query_service_state(&name);
                let _ = status_tx.send((name, state));
                drop(state_handle);
            });
        }
    }

    fn install_driver(&mut self, ctx: &egui::Context) {
        let path = self.base_dir.join("ks-driver.sys");
        if !path.is_file() {
            self.push(format!("[error] driver file not found: {}", path.display()));
            return;
        }
        self.run_jobs(
            ctx,
            vec![sc_job(&[
                "create",
                DRIVER_SERVICE,
                "type=",
                "kernel",
                "start=",
                "demand",
                "binPath=",
                &path.to_string_lossy(),
            ])],
        );
    }

    fn install_service(&mut self, ctx: &egui::Context) {
        let path = self.base_dir.join("ks-service.exe");
        if !path.is_file() {
            self.push(format!(
                "[error] service file not found: {}",
                path.display()
            ));
            return;
        }
        let quoted = format!("\"{}\"", path.display());
        self.run_jobs(
            ctx,
            vec![
                sc_job(&[
                    "create",
                    BACKEND_SERVICE,
                    "type=",
                    "own",
                    "start=",
                    "demand",
                    "obj=",
                    "LocalSystem",
                    "binPath=",
                    &quoted,
                ]),
                sc_job(&["sidtype", BACKEND_SERVICE, "unrestricted"]),
            ],
        );
    }

    fn uninstall(&mut self, ctx: &egui::Context, name: &str) {
        self.run_jobs(
            ctx,
            vec![
                sc_job_best_effort(&["stop", name]),
                sc_job(&["delete", name]),
            ],
        );
    }

    fn query(&mut self, ctx: &egui::Context, name: &str) {
        self.run_jobs(ctx, vec![sc_job(&["query", name])]);
    }

    fn relaunch_elevated(&mut self) {
        if self.relaunch_started {
            return;
        }
        self.relaunch_started = true;
        let Some(self_path) = std::env::current_exe()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
        else {
            self.push("[error] cannot determine installer path.");
            return;
        };
        let spawn = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Process -FilePath '{self_path}' -Verb RunAs"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        match spawn {
            Ok(_) => {
                self.push("Administrator privileges requested; confirm the UAC prompt.");
                std::process::exit(0);
            }
            Err(error) => {
                self.relaunch_started = false;
                self.push(format!("[error] elevated relaunch failed: {error}"));
            }
        }
    }

    fn status_badge(&self, name: &str) -> (String, egui::Color32) {
        let state = self
            .state_handle(name)
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| STATE_UNKNOWN.to_owned());
        let color = if state.contains(STATE_RUNNING) {
            egui::Color32::from_rgb(90, 200, 90)
        } else if state.contains("PENDING") {
            egui::Color32::from_rgb(230, 180, 60)
        } else if state.contains(STATE_STOPPED) {
            egui::Color32::from_rgb(160, 160, 160)
        } else {
            egui::Color32::from_rgb(220, 90, 90)
        };
        (state, color)
    }

    fn service_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, name: &str, title: &str) {
        let enabled = !self.busy.load(Ordering::SeqCst);
        let (state, color) = self.status_badge(name);
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.colored_label(color, state);
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if button(ui, enabled, "Install").clicked() {
                    if name == DRIVER_SERVICE {
                        self.install_driver(ctx);
                    } else {
                        self.install_service(ctx);
                    }
                }
                if button(ui, enabled, "Start").clicked() {
                    self.run_jobs(ctx, vec![sc_job(&["start", name])]);
                }
                if button(ui, enabled, "Stop").clicked() {
                    self.run_jobs(ctx, vec![sc_job_best_effort(&["stop", name])]);
                }
                if button(ui, enabled, "Uninstall").clicked() {
                    self.uninstall(ctx, name);
                }
                if button(ui, enabled, "Status").clicked() {
                    self.query(ctx, name);
                }
            });
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Kernel Script Installer");
        ui.add_space(2.0);
        ui.label(format!("Directory: {}", self.base_dir.display()));
        ui.separator();

        if !self.elevated {
            ui.horizontal(|ui| {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Not running as administrator; service operations will fail.",
                );
                if ui.button("Restart as administrator").clicked() {
                    self.relaunch_elevated();
                }
            });
            ui.separator();
        }

        self.service_row(
            ui,
            ctx,
            DRIVER_SERVICE,
            &format!("Kernel driver ({DRIVER_SERVICE})"),
        );
        self.service_row(
            ui,
            ctx,
            BACKEND_SERVICE,
            &format!("Backend service ({BACKEND_SERVICE})"),
        );

        ui.separator();
        ui.label("Output:");
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let text = self
                    .log
                    .lock()
                    .map(|log| log.join("\n"))
                    .unwrap_or_default();
                ui.monospace(text);
            });
    }
}

fn button(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).size(13.0)).min_size(BUTTON_SIZE),
    )
}

impl eframe::App for InstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(line) = self.events.try_recv() {
            self.push(line);
        }
        while let Ok((name, state)) = self.statuses.try_recv() {
            if let Ok(mut guard) = self.state_handle(&name).lock() {
                *guard = state;
            }
        }
        if self.last_refresh.elapsed() >= STATUS_REFRESH {
            self.refresh_statuses();
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui(ui, ctx);
        });
        ctx.request_repaint_after(Duration::from_millis(150));
    }
}

fn query_service_state(name: &str) -> String {
    let Ok(output) = Command::new("sc.exe")
        .args(["query", name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return STATE_UNKNOWN.to_owned();
    };
    if !output.status.success() {
        // A deleted service reports failure; treat it as stopped.
        return STATE_STOPPED.to_owned();
    }
    let text = decode_output(&output.stdout);
    for line in text.lines() {
        if line.contains("STATE") {
            if let Some(state) = line.split_whitespace().next_back() {
                return state.to_owned();
            }
        }
    }
    STATE_UNKNOWN.to_owned()
}

fn execute_sc(args: &[String]) -> Result<String, String> {
    let output = Command::new("sc.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to run sc.exe: {error}"))?;
    let mut text = decode_output(&output.stdout);
    let stderr = decode_output(&output.stderr);
    if !stderr.trim().is_empty() {
        text.push_str(&stderr);
    }
    let text = text.trim().to_owned();
    if output.status.success() {
        Ok(if text.is_empty() {
            "(success, no output)".to_owned()
        } else {
            text
        })
    } else {
        Err(format!(
            "{} (exit code {:?})",
            if text.is_empty() {
                "command failed".to_owned()
            } else {
                text
            },
            output.status.code()
        ))
    }
}

fn decode_output(bytes: &[u8]) -> String {
    // sc.exe outputs in the system ANSI code page (GBK on zh-CN Windows).
    let (decoded, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        decoded.into_owned()
    }
}

fn is_elevated() -> bool {
    Command::new("net.exe")
        .args(["session"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn install_chinese_font(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    let Some(path) = candidates.iter().find(|path| Path::new(path).is_file()) else {
        return;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "ks-installer-cjk".to_owned(),
        egui::FontData::from_owned(bytes),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "ks-installer-cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn main() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kernel Script Installer")
            .with_inner_size([760.0, 600.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Kernel Script Installer",
        options,
        Box::new(|creation_context| {
            install_chinese_font(&creation_context.egui_ctx);
            Box::new(InstallerApp::new())
        }),
    )
    .ok();
}
