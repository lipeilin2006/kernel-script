//! `ks-test load` / `ks-test loadone`: the provider × victim load matrix.
//!
//! Each combination runs in a fresh child process because a ks-link
//! session binds to one driver generation and never reconnects.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::step_log::say;
use crate::CREATE_NO_WINDOW;

/// `ks-test loadone <provider> <victim>`: one pinned provider × victim
/// attempt in a FRESH process. The matrix parent spawns this per
/// combination because a ks-link session binds to one driver
/// generation and never reconnects — a second combo in the same
/// process would talk to a dead ring.
pub(crate) fn load_one_standalone(provider: u32, victim: u32) -> i32 {
    std::env::set_var("KS_SDK_MAP", "1");
    std::env::remove_var("KS_SDK_KDU_PRV");
    std::env::remove_var("KS_TEST_KDU_PRV");
    std::env::remove_var("KS_SDK_VICTIM");
    if let Err(error) = ks_sdk::start_with(Some(provider), Some(victim)) {
        eprintln!("loadone: start failed: {error}");
        return 1;
    }
    if let Err(error) = ks_sdk::ping() {
        eprintln!("loadone: loaded but ping failed: {error}");
        let _ = ks_sdk::stop();
        return 1;
    }
    println!("loadone: provider {provider} × victim {victim}: loaded, ring answers");
    let _ = ks_sdk::stop();
    0
}

/// `ks-test load`: sweep every retained provider × victim build through
/// `ks_sdk::start_with` on the manual-map path and report the matrix.
///
/// Each combination runs in a fresh `loadone` child process (one
/// ks-link session per driver generation) and gets a full [`ks_sdk::stop`].
/// Exit 0 only when every combination passed.
pub(crate) fn run_load_matrix() -> i32 {
    std::env::set_var("KS_SDK_MAP", "1");
    std::env::remove_var("KS_SDK_KDU_PRV");
    std::env::remove_var("KS_TEST_KDU_PRV");
    std::env::remove_var("KS_SDK_VICTIM");

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("load matrix: current exe: {error}");
            return 2;
        }
    };
    let providers = ks_sdk::provider_ids();
    let victims = ks_sdk::victim_builds();
    say(&format!(
        "load matrix: {} providers × {} victims, manual mapping only",
        providers.len(),
        victims.len()
    ));

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut results: Vec<String> = Vec::new();

    for provider in &providers {
        for victim in &victims {
            say(&format!("=== provider {provider} × victim {victim} ==="));
            let outcome = match load_cell(&exe, *provider, *victim) {
                Ok(()) => {
                    pass += 1;
                    "PASS".to_string()
                }
                Err(reason) => {
                    fail += 1;
                    format!("FAIL ({reason})")
                }
            };
            say(&format!("    {outcome}"));
            results.push(format!(
                "  provider {provider} × victim {victim}: {outcome}"
            ));
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    say("");
    say("--- load matrix results ---");
    for line in &results {
        say(line);
    }
    say(&format!(
        "load matrix: {pass} pass, {fail} fail (of {})",
        pass + fail
    ));
    if fail == 0 {
        0
    } else {
        1
    }
}

/// Spawns one `loadone` child and reports why it failed: the last line of
/// its output (what the harness has always shown) or the spawn error.
fn load_cell(exe: &Path, provider: u32, victim: u32) -> Result<(), String> {
    let output = Command::new(exe)
        .args(["loadone", &provider.to_string(), &victim.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let last = text.lines().last().unwrap_or("no output").to_string();
            Err(last)
        }
        Err(error) => Err(format!("spawn: {error}")),
    }
}
