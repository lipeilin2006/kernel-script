//! Step logs: every harness milestone is flushed to every file that opened
//! successfully so a bugcheck (which takes the console with it) still
//! leaves the exact crashing step on disk. Includes fixed absolute paths so
//! the log is findable no matter where the executable was copied.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static STEP_LOG: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prints to the console and appends+flushes to the step logs.
pub(crate) fn say(line: &str) {
    println!("{line}");
    if let Ok(mut files) = STEP_LOG.lock() {
        for file in files.iter_mut() {
            let _ = writeln!(file, "[{:>6}] {line}", now_secs());
            let _ = file.flush();
            let _ = file.sync_all();
        }
    }
}

pub(crate) fn open_step_log() {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ks-test-run.log"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ks-test-run.log"));
    }
    candidates.push(PathBuf::from(r"D:\kernel-script\ks-test-run.log"));
    candidates.push(PathBuf::from(r"C:\ks-test-run.log"));

    let mut files = Vec::new();
    let mut opened = Vec::new();
    for path in &candidates {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            opened.push(format!("{}", path.display()));
            files.push(file);
        }
    }
    *STEP_LOG.lock().expect("step log poisoned") = files;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    say(&format!("harness start exe={exe} logs={opened:?}"));
}
