//! ks-test: correctness harness and benchmark for the KernelScript ring.
//!
//! Default lifecycle: the dt-loader mapper (reached through ks-sdk) loads
//! the driver through `ks_sdk::start()` — the manual-map provider chain
//! runs first (shellcode V3, `DriverEntry` runs in place, nothing
//! registers with the SCM), and a chain that cannot run the driver
//! falls back to a normal service load of the signed image. Modes:
//! `full`/`minimal` correctness
//! suites, `benchmark` performance only, `load` the provider × victim
//! matrix, `shutdown` recovery. Readiness is the registry publication:
//! poll `HKLM\SOFTWARE\KernelScript` until the driver publishes its object
//! names, connect through `ks-sdk` (the `ks-link` re-export) and exercise
//! every operation against
//! this process's own memory: plain, RVA-resolved and MDL-remap
//! reads/writes, batch operations, pointer-chain walks, the driver-side
//! memory locks and concurrent ring access from several threads. The
//! read/write paths additionally run performance benchmarks (all sizes and
//! transports); pointer walks and friends only need to work.
//!
//! KDU never unloads a mapped image, so teardown inverts: the ring
//! `shutdown` request is what makes the driver release its ring objects,
//! registry claim and single-instance marker, and a `ks-test shutdown`
//! child process (link sessions never reconnect) verifies nothing live
//! remains and that the driver maps again cleanly. The legacy SCM lifecycle
//! (`ks-test sc [full]`) still exists for regression runs: `sc
//! create`/`sc start`, then after `shutdown` the checked `sc stop`/`sc
//! query`/`sc delete` sequence.
//!
//! Every correctness failure is collected and reported; benchmarks only run
//! when the whole suite passes. The process exits non-zero on failure, so
//! it can drive automated smoke runs. RVA/MDL-RVA checks target a static in
//! this image: RVA addressing is relative to the module base and the heap
//! typically sits below it, so heap addresses have no usable RVA. The
//! workspace profiles use `panic = "abort"`, so the harness reports
//! failures instead of unwinding — an aborted run would leak the loaded
//! driver instance.

use std::fs;
use std::io::{self, Write};
use std::sync::Arc;

mod bench;
mod checks;
mod elevate;
mod lifecycle;
mod load_matrix;
mod sc;
mod step_log;
mod target;

use bench::run_benchmarks;
use checks::run_checks;
use elevate::run_shutdown_child;
use lifecycle::{shutdown_standalone, wait_for_driver, EmbeddedDriver, LoadMode};
use load_matrix::{load_one_standalone, run_load_matrix};
use sc::{sc, sc_failure_code};
use step_log::{open_step_log, say};
use target::{check, read_is, Target};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Minimal mode: transport (ping/getbase) plus normal non-MDL reads
/// only. A bugcheck here implicates the ring/request path; passing
/// means the transport is sound and later suspects (mdl/write/batch)
/// can be reintroduced one at a time. `ks-test full` runs the suite.
fn run_minimal(target: &Target, failures: &mut Vec<(String, String)>) -> i32 {
    say("mode: minimal (normal reads only; `ks-test full` = full suite)");
    check(failures, "ping", || {
        ks_sdk::ping().map_err(|e| e.to_string())
    });
    check(failures, "get_process_base == image base", || {
        if target.driver_base == target.image_base {
            Ok(())
        } else {
            Err(format!(
                "driver reports 0x{:X}, user mode sees 0x{:X}",
                target.driver_base, target.image_base
            ))
        }
    });
    check(failures, "read_bytes(4) content", || {
        read_is(target, target.slot(0x400), &target.buffer[0x400..0x404])
    });
    check(failures, "read_bytes(64) content", || {
        read_is(target, target.slot(0x40), &target.buffer[0x40..0x80])
    });
    check(failures, "read_bytes(4096) content", || {
        read_is(target, target.slot(0), &target.buffer)
    });
    report(failures, "=== ALL CHECKS PASSED (minimal read-only) ===")
}

/// Single-instance guard: a second driver load while this one is live
/// must fail inside DriverEntry (the marker object is openable ->
/// STATUS_OBJECT_NAME_COLLISION), and the live instance must keep
/// answering afterwards. KDU mode re-runs the mapper (the second image
/// never reaches a service); the legacy path uses a duplicate service.
fn check_second_instance(embedded: &EmbeddedDriver, failures: &mut Vec<(String, String)>) {
    check(failures, "second driver instance rejected", || {
        match &embedded.mode {
            LoadMode::Kdu => {
                match ks_sdk::start() {
                    Ok(()) => return Err("second instance mapped (guard missing)".into()),
                    // STATUS_OBJECT_NAME_COLLISION, exactly what the
                    // marker probe returns.
                    Err(ks_sdk::Error::DriverEntry(0xC000_0035)) => {}
                    Err(ks_sdk::Error::DriverEntry(status)) => {
                        return Err(format!(
                            "second map failed for the wrong reason (0x{status:08X}, \
                             expected 0xC0000035)"
                        ));
                    }
                    Err(error) => return Err(format!("second map failed: {error}")),
                }
                ks_sdk::ping().map_err(|e| format!("ping after rejected load: {e}"))
            }
            LoadMode::Sc { service } => {
                let duplicate_service = format!("{service}dup");
                // A separate copy at a distinct path: same bytes,
                // independent load, so the guard (not loader path
                // aliasing) is what rejects it.
                let duplicate_image = embedded.root.join("ks-driver-dup.sys");
                fs::copy(embedded.root.join("ks-driver.sys"), &duplicate_image)
                    .map_err(|e| format!("copy image: {e}"))?;
                let image_arg = duplicate_image.to_string_lossy().into_owned();
                if let Err(error) = sc(&[
                    "create",
                    &duplicate_service,
                    "type=",
                    "kernel",
                    "start=",
                    "demand",
                    "binPath=",
                    &image_arg,
                ]) {
                    let _ = fs::remove_file(&duplicate_image);
                    return Err(format!("sc create: {error}"));
                }
                let started = sc(&["start", &duplicate_service]);
                let _ = sc(&["delete", &duplicate_service]);
                let _ = fs::remove_file(&duplicate_image);
                let error = match started {
                    Err(error) => error,
                    Ok(()) => return Err("second instance started (guard missing)".into()),
                };
                match sc_failure_code(&error) {
                    Some(183) => {}
                    other => {
                        return Err(format!(
                            "second start failed for the wrong reason (code {other:?}): \
                             {error}"
                        ));
                    }
                }
                ks_sdk::ping().map_err(|e| format!("ping after rejected load: {e}"))
            }
        }
    });
}

/// Sends the destructive ring `shutdown` request and checks that the
/// worker stopped answering. Runs only while no earlier check failed, so
/// a broken driver is never asked to shut down as well; returns whether
/// the shutdown request itself succeeded (KDU teardown verifies the claim
/// only then).
fn check_shutdown(failures: &mut Vec<(String, String)>) -> bool {
    if !failures.is_empty() {
        return false;
    }
    say("");
    check(failures, "shutdown command stops the worker", || {
        ks_sdk::shutdown().map_err(|e| e.to_string())
    });
    let shutdown_ok = failures.is_empty();
    check(
        failures,
        "requests time out after shutdown",
        || match ks_sdk::ping() {
            Err(_) => Ok(()),
            Ok(()) => Err("ping succeeded after shutdown".into()),
        },
    );
    shutdown_ok
}

/// Mode-specific teardown. Both modes run even when earlier checks
/// failed, so no instance is ever left behind.
fn run_teardown(
    embedded: &mut EmbeddedDriver,
    shutdown_ok: bool,
    failures: &mut Vec<(String, String)>,
) {
    match embedded.mode.clone() {
        // Legacy SCM: after shutdown the worker is gone, so `sc stop` must
        // complete the unload (service reports STOPPED) and `sc delete`
        // must remove the service. With a live worker this exercises the
        // normal unload path instead.
        LoadMode::Sc { .. } => {
            let (stop_outcome, delete_outcome) = embedded.teardown();
            check(failures, "sc stop: service reports STOPPED", || {
                stop_outcome
            });
            check(failures, "sc delete: service removed", || delete_outcome);
            // DriverUnload runs inside `sc stop`, so an unloaded driver has
            // already erased its registry publication.
            check(
                failures,
                "unload removed the published object names",
                || match ks_sdk::published_object_names_strict() {
                    None => Ok(()),
                    Some(_) => Err("object names still published".into()),
                },
            );
        }
        // KDU: no service exists. The invariants are that shutdown
        // released the single-instance claim and the registry publication,
        // that the driver maps again afterwards (the marker and ring are
        // really gone, no reboot needed), and that nothing live remains
        // behind.
        LoadMode::Kdu => {
            if shutdown_ok {
                check(
                    failures,
                    "shutdown released the single-instance claim",
                    || match ks_sdk::instance_claim_present() {
                        Ok(false) => Ok(()),
                        Ok(true) => Err("Instance claim still present".into()),
                        Err(error) => Err(error.to_string()),
                    },
                );
                check(
                    failures,
                    "shutdown removed the published object names",
                    || match ks_sdk::published_object_names_strict() {
                        None => Ok(()),
                        Some(_) => Err("object names still published".into()),
                    },
                );
            }
            if failures.is_empty() {
                check(failures, "driver re-maps after shutdown", || {
                    ks_sdk::start().map_err(|e| format!("ks_sdk::start: {e}"))?;
                    // This process's session still points at the dead ring,
                    // so the fresh instance is stopped through a child.
                    run_shutdown_child()
                });
            }
            check(failures, "no live driver instance remains", || {
                run_shutdown_child()
            });
            embedded.finished = true;
        }
    }
}

/// Prints the collected failures (or the all-passed marker) and maps them
/// to the process exit code.
fn report(failures: &[(String, String)], all_passed: &str) -> i32 {
    say("");
    if failures.is_empty() {
        say(all_passed);
    } else {
        say(&format!("=== {} CHECK(S) FAILED ===", failures.len()));
        for (name, error) in failures {
            say(&format!("  {name}: {error}"));
        }
    }
    if failures.is_empty() {
        0
    } else {
        1
    }
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let full = args.iter().any(|a| a == "full");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    // Default load path is manual mapping with a service-load fallback;
    // `ks-test sc ...` selects the legacy SCM lifecycle for regression
    // runs, `benchmark` runs the performance suite only, and `load`
    // sweeps every provider × victim combination through start_with.
    let legacy_sc = args.iter().any(|a| a == "sc");

    if args.iter().any(|a| a == "load") {
        return run_load_matrix();
    }

    // Held (not dropped) until the end of `run`, so the driver stays loaded
    // while the harness executes. Full mode ends with the checked teardown
    // (per mode: `sc stop`/`sc query`/`sc delete`, or the instance-claim
    // checks); other paths fall back to Drop.
    let mut embedded = match EmbeddedDriver::start(legacy_sc) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start embedded driver: {error}");
            return 2;
        }
    };
    match &embedded.mode {
        LoadMode::Kdu => say("driver load mode: sdk (manual mapping first, service-load fallback)"),
        LoadMode::Sc { service } => {
            say(&format!("driver load mode: sc (legacy service {service})"));
        }
    }

    say("ks-test: self-process correctness + benchmark harness");
    if let Err(error) = wait_for_driver() {
        eprintln!("{error}");
        return 2;
    }

    let target = match Target::setup() {
        Ok(target) => Arc::new(target),
        Err(error) => {
            say(&format!("target setup failed: {error}"));
            return 2;
        }
    };
    say(&format!(
        "target pid: {}  image base: 0x{:X}  buffer: 0x{:X}  iters: {iters}",
        target.pid, target.image_base, target.buffer_addr
    ));
    say("");

    let mut failures = Vec::new();

    if args.iter().any(|a| a == "benchmark") {
        say("mode: benchmark (performance suite only; `ks-test full` adds correctness)");
        say("");
        run_benchmarks(&target, iters);
        return 0;
    }

    if !full {
        return run_minimal(&target, &mut failures);
    }

    check_second_instance(&embedded, &mut failures);

    run_checks(&target, &mut failures);

    if failures.is_empty() {
        say("");
        run_benchmarks(&target, iters);
    } else {
        say("");
        say("benchmarks skipped: correctness failures present");
    }

    // The shutdown command is destructive (it stops the worker), so it runs
    // last, after the benchmarks. In KDU mode it is also what releases the
    // single-instance claim, because no unload path exists.
    let shutdown_ok = check_shutdown(&mut failures);

    // Mode-specific teardown. Both run even when earlier checks failed, so
    // no instance is ever left behind.
    say("");
    run_teardown(&mut embedded, shutdown_ok, &mut failures);

    // The instance is already torn down and verified by the mode-specific
    // block above; `embedded` drops here and only removes the temp
    // directory before main decides about the pause prompt.
    report(&failures, "=== ALL CHECKS PASSED ===")
}

fn main() {
    open_step_log();
    // Route the KDU mapper's step-log lines into the step log with the
    // historic `kdu: ` prefix.
    ks_sdk::kdu::set_log_sink(|line| say(&format!("kdu: {line}")));
    // `ks-test shutdown` is the standalone recovery/cleanup channel (see
    // `shutdown_standalone`): it must never pause for input, so it exits
    // before run()'s interactive paths.
    if std::env::args().nth(1).as_deref() == Some("shutdown") {
        std::process::exit(shutdown_standalone());
    }
    // `ks-test loadone <provider> <victim>`: one pinned matrix cell,
    // driven by `run_load_matrix` (fresh process per driver generation).
    if std::env::args().nth(1).as_deref() == Some("loadone") {
        let parsed = (|| {
            let provider: u32 = std::env::args().nth(2)?.parse().ok()?;
            let victim: u32 = std::env::args().nth(3)?.parse().ok()?;
            Some((provider, victim))
        })();
        match parsed {
            Some((provider, victim)) => std::process::exit(load_one_standalone(provider, victim)),
            None => {
                eprintln!("usage: ks-test loadone <provider-id> <victim-build>");
                std::process::exit(2);
            }
        }
    }
    let failures = run();
    if failures != 0 {
        println!("Press Enter to exit.");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }
    std::process::exit(failures);
}
