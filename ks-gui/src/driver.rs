//! Silent driver lifecycle: the startup probe reuses a live driver or
//! starts the embedded one through `ks_sdk::start(None)` (in-process KDU map)
//! on a background worker, [`DriverControl::poll`] chains that start once
//! the probe reports a stopped driver, and [`finish_on_exit`] shuts a live
//! driver down again after the render loop has returned. The module also
//! owns the gate that keeps `sync_ipc` round trips away from the
//! process-wide session while such a job can close or replace it. The GUI
//! renders this as one small `KernelScript` window: the current phase
//! (`probing...`, `starting...`, `running`, or the start error in red)
//! above a scrollable startup log — the lifecycle narrative plus the
//! `trying provider <id>` lines the ks_sdk log sink receives while
//! `ks_sdk::start` walks its provider chain — and the window's only
//! control, the `Stop` button, which just requests the overlay to close;
//! the shutdown itself runs in [`finish_on_exit`].
//!
//! Threading rules:
//! - The GUI thread owns [`DriverControl`]; a worker only sends an
//!   [`Outcome`] (job kind + owned plain data) over the channel and never
//!   touches Lua or egui.
//! - The startup log buffer is an `Arc<Mutex<VecDeque<String>>>` shared
//!   with the ks_sdk log sink: the sink appends owned strings while the
//!   map runs on the worker thread, this thread appends the lifecycle
//!   narrative, and the window reads it for display. Every critical
//!   section only pushes or reads owned strings.
//! - A job sets [`LIFECYCLE_BUSY`] on the GUI thread *before* it is
//!   spawned. The GUI Lua thread is the only round-trip source and checks
//!   that flag at every `sync_ipc` entry, so once it is set no round trip
//!   can start — and none is already in flight, because the flag flip and
//!   the spawn happen on the same thread outside Lua. That is the
//!   precondition `ks_sdk::close_session` documents: the session may be
//!   freed only while nothing waits on it.
//! - The worker clears the flag after its job returns and *before* it
//!   sends the outcome: the session is settled at that point (a start
//!   left the fresh load behind, a stop closed the session itself), and
//!   clearing first guarantees a follow-up job spawned from that outcome
//!   closes the gate again before the old worker could clear it away.
//! - [`finish_on_exit`] runs on the main thread after the loop returned —
//!   Lua and rendering are gone — so it waits out any in-flight job and
//!   then performs the shutdown synchronously, never on the render thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Cap for the startup log the `KernelScript` window shows; the oldest
/// line is dropped beyond it.
const MAX_LOG_LINES: usize = 200;

/// Set while a lifecycle job owns the session; `sync_ipc` refuses every
/// call while it is true.
static LIFECYCLE_BUSY: AtomicBool = AtomicBool::new(false);

/// Whether `sync_ipc` must refuse round trips right now.
pub fn lifecycle_busy() -> bool {
    LIFECYCLE_BUSY.load(Ordering::Acquire)
}

/// What [`DriverControl::ui`] displays for the current lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Probing,
    Starting,
    Running,
    Failed(String),
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Phase::Probing => "probing...",
            Phase::Starting => "starting...",
            Phase::Running => "running",
            Phase::Failed(_) => "error",
        }
    }
}

/// One lifecycle job; the worker runs exactly one at a time (the probe
/// chain in [`DriverControl::poll`] never spawns a second while one is in
/// flight, because outcomes only arrive after the job returned).
#[derive(Debug)]
enum Job {
    Probe,
    Start,
}

/// Worker result back to the GUI thread: job kind plus owned plain data.
enum Outcome {
    Probe(bool),
    Start(Result<(), String>),
}

pub struct DriverControl {
    /// The state the `KernelScript` window shows; updated by [`Self::poll`]
    /// when a job outcome arrives, never by the worker itself.
    phase: Phase,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
    /// Startup log lines shared with the ks_sdk log sink: the sink
    /// appends `trying provider <id>` lines from the worker thread
    /// inside `ks_sdk::start`, the GUI thread appends the lifecycle
    /// narrative, and [`Self::ui`] reads the buffer for display.
    logs: Arc<Mutex<VecDeque<String>>>,
}

/// Appends one line to the shared startup log, dropping the oldest line
/// beyond [`MAX_LOG_LINES`]. The critical section only copies an owned
/// string.
fn push_line(logs: &Mutex<VecDeque<String>>, line: &str) {
    let mut guard = logs.lock().unwrap_or_else(|error| error.into_inner());
    if guard.len() >= MAX_LOG_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
}

impl DriverControl {
    /// Creates the controller and spawns the startup probe: a driver
    /// instance already live (leftover from a previous run) is reused, a
    /// stopped one is started silently on the worker. Also installs the
    /// ks_sdk log sink that feeds the window's startup log.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        // The sink runs on the worker thread inside `ks_sdk::start` and
        // only appends owned strings to the shared buffer.
        let sink_logs = Arc::clone(&logs);
        ks_sdk::set_log_sink(move |line| push_line(&sink_logs, line));
        let mut control = Self {
            phase: Phase::Probing,
            tx,
            rx,
            logs,
        };
        control.spawn(Job::Probe);
        control
    }

    /// Appends one lifecycle line to the startup log (GUI thread).
    fn log_line(&self, line: impl AsRef<str>) {
        push_line(&self.logs, line.as_ref());
    }

    /// Applies every finished worker job. Runs once per frame before any
    /// Lua runs, so the auto-start chained from the probe outcome closes
    /// the gate again before this frame's Lua could round trip.
    pub fn poll(&mut self) {
        while let Ok(outcome) = self.rx.try_recv() {
            match outcome {
                Outcome::Probe(live) => {
                    tracing::info!(live, "driver lifecycle: probe finished");
                    if live {
                        self.log_line("reusing the live driver");
                        self.phase = Phase::Running;
                    } else {
                        self.spawn(Job::Start);
                    }
                }
                Outcome::Start(Ok(())) => {
                    tracing::info!("driver lifecycle: started");
                    self.log_line("driver started");
                    self.phase = Phase::Running;
                }
                Outcome::Start(Err(error)) => {
                    tracing::warn!(%error, "driver lifecycle: start failed");
                    // A start that lost the single-instance guard found a
                    // live driver (someone else's load answers); anything
                    // else really is the failure the error describes.
                    self.phase = match ks_sdk::instance_claim_present() {
                        Ok(true) => {
                            self.log_line("start lost the single-instance guard: reusing it");
                            Phase::Running
                        }
                        _ => {
                            self.log_line(format!("start failed: {error}"));
                            Phase::Failed(error)
                        }
                    };
                }
            }
        }
    }

    /// Draws the `KernelScript` window — the current driver phase, then
    /// the scrollable startup log, then the `Stop` button — and returns
    /// true when `Stop` was clicked, i.e. when the caller should close
    /// the overlay window. Pure display: no lifecycle job is spawned
    /// from here.
    pub fn ui(&self, ctx: &egui::Context) -> bool {
        let mut stop_clicked = false;
        egui::Window::new("KernelScript")
            .default_pos(egui::pos2(16.0, 16.0))
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                match &self.phase {
                    Phase::Failed(error) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("driver: {error}"));
                    }
                    phase => {
                        ui.label(format!("driver: {}", phase.label()));
                    }
                }
                {
                    let logs = self.logs.lock().unwrap_or_else(|error| error.into_inner());
                    if !logs.is_empty() {
                        egui::ScrollArea::vertical()
                            .max_height(140.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for line in logs.iter() {
                                    ui.monospace(line);
                                }
                            });
                    }
                }
                if ui.button("Stop").clicked() {
                    stop_clicked = true;
                }
            });
        stop_clicked
    }

    /// Marks the gate closed, publishes the matching phase, and runs the
    /// job on a detached thread. Must only be called on the GUI thread
    /// while no job is in flight.
    fn spawn(&mut self, job: Job) {
        LIFECYCLE_BUSY.store(true, Ordering::Release);
        self.phase = match job {
            Job::Probe => Phase::Probing,
            Job::Start => Phase::Starting,
        };
        self.log_line(match &job {
            Job::Probe => "probing for a live driver...",
            Job::Start => "starting the driver...",
        });
        tracing::info!(?job, "driver lifecycle: job started");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let outcome = match job {
                Job::Probe => Outcome::Probe(probe()),
                Job::Start => Outcome::Start(start()),
            };
            LIFECYCLE_BUSY.store(false, Ordering::Release);
            let _ = tx.send(outcome);
        });
    }
}

impl Default for DriverControl {
    fn default() -> Self {
        Self::new()
    }
}

/// Mirrors the launch behaviour once the render loop has returned: waits
/// out an in-flight lifecycle job (the KDU map can take seconds), then
/// silently shuts a live driver down and drops the process-wide session.
/// Lua and rendering are gone at this point, so the round trip runs on
/// the main thread without violating the render-thread rule.
pub fn finish_on_exit() {
    let deadline = Instant::now() + Duration::from_secs(60);
    while lifecycle_busy() {
        if Instant::now() >= deadline {
            tracing::warn!("driver lifecycle: exit gave up waiting for the in-flight job");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if probe() {
        if let Err(error) = stop() {
            tracing::warn!(%error, "driver lifecycle: stop on exit failed");
        }
    }
    ks_sdk::close_session();
    tracing::info!("driver lifecycle: exit cleanup finished");
}

/// True when the published names exist *and* the ring answers — a live
/// leftover instance from a previous run. Stale names cannot answer: the
/// objects behind them are gone, so the session open fails immediately.
fn probe() -> bool {
    ks_sdk::published_object_names_strict().is_some() && ks_sdk::ping().is_ok()
}

fn start() -> Result<(), String> {
    // Any session left from before belongs to an older load (or to a
    // probe that timed out); drop it so the first Lua round trip after
    // this opens against the new load's randomized names.
    ks_sdk::close_session();
    ks_sdk::start().map_err(|error| error.to_string())
}

fn stop() -> Result<(), String> {
    let result = ks_sdk::stop().map_err(|error| error.to_string());
    // The driver's objects are gone either way; the next round trip must
    // not wait on a session that can never answer again.
    ks_sdk::close_session();
    result
}
