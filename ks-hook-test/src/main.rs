//! Minimal end-to-end harness for the kernel hook API.
//!
//! It loads the driver (or reuses a live one), then
//! 1. checks the SDK-side guard rails (bad addresses/ids are refused),
//! 2. runs the driver's own self test: create → enable → verify jump →
//!    disable → verify restored bytes → remove against a leaf function in
//!    the driver image, and
//! 3. shuts the driver down again.
//!
//! Nothing outside the driver's own `.text` is patched, so running this on a
//! development machine cannot disturb foreign kernel code. A `0` from the
//! self test means the platform refused the write (HVCI keeps driver text
//! read-only); the rest of the run still passes.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hook test failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    ks_sdk::set_log_sink(|line| println!("kdu: {line}"));
    println!("hook-test: starting driver");
    match ks_sdk::start() {
        Ok(()) => println!("hook-test: driver started"),
        // A driver that is already live (or a second load that lost the
        // single-instance guard) is fine: reuse it.
        Err(error) if ks_sdk::instance_claim_present().unwrap_or(false) => {
            println!("hook-test: reusing the live driver ({error})");
        }
        Err(error) => return Err(format!("start: {error}")),
    }

    println!("hook-test: waiting for the registry publication");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while ks_sdk::published_object_names_strict().is_none() {
        if std::time::Instant::now() >= deadline {
            return Err("driver never published its object names".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    ks_sdk::ping().map_err(|error| format!("ping: {error}"))?;
    println!("hook-test: ring is answering");

    println!("hook-test: guard rails");
    expect_err("null target", ks_sdk::hook_create(0, 0xFFFF_F800_0000_1000))?;
    expect_err(
        "user-mode target",
        ks_sdk::hook_create(0x1000, 0xFFFF_F800_0000_1000),
    )?;
    expect_err(
        "user-mode trampoline",
        ks_sdk::hook_create(0xFFFF_F800_0000_1000, 0x1000),
    )?;
    expect_err("id 0 enable", ks_sdk::hook_enable(0))?;
    expect_err("unknown id remove", ks_sdk::hook_remove(0xDEAD_BEEF))?;
    println!("hook-test: guard rails ok");

    println!("hook-test: driver self test (create/enable/verify/disable/remove)");
    let patched = ks_sdk::hook_self_test().map_err(|error| format!("self test: {error}"))?;
    match patched {
        1 => println!("hook-test: PASS - jump applied and original bytes restored"),
        0 => println!("hook-test: SKIPPED - the platform refused the write (HVCI)"),
        other => return Err(format!("unexpected self-test result {other}")),
    }

    println!("hook-test: shellcode guard rails");
    expect_err(
        "user-mode shellcode target",
        ks_sdk::hook_shellcode(0x1000, &[0xC3]),
    )?;
    expect_err(
        "empty shellcode",
        ks_sdk::hook_shellcode(0xFFFF_F800_0000_1000, &[]),
    )?;
    let too_large = vec![0x90u8; ks_core::protocol::MAX_HOOK_SHELLCODE + 1];
    expect_err(
        "oversize shellcode",
        ks_sdk::hook_shellcode(0xFFFF_F800_0000_1000, &too_large),
    )?;
    println!("hook-test: shellcode guard rails ok");

    println!("hook-test: shellcode self test (alloc/copy/hook/call/disable/remove)");
    let shellcode = ks_sdk::hook_shellcode_self_test()
        .map_err(|error| format!("shellcode self test: {error}"))?;
    match shellcode {
        1 => println!("hook-test: PASS - stub executed through the hook, original restored"),
        0 => println!("hook-test: SKIPPED - executable allocation refused (HVCI)"),
        other => return Err(format!("unexpected shellcode self-test result {other}")),
    }

    println!("hook-test: stopping driver");
    if let Err(error) = ks_sdk::stop() {
        // A mapped image may already have self-torn-down; that is not a test
        // failure as long as the claim is gone.
        if ks_sdk::instance_claim_present().unwrap_or(false) {
            return Err(format!("stop: {error}"));
        }
    }
    println!("hook-test: done");
    Ok(())
}

fn expect_err<T>(what: &str, result: Result<T, ks_sdk::LinkError>) -> Result<(), String> {
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err(format!("{what} was accepted")),
    }
}
