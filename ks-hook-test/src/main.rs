//! Minimal end-to-end harness for the kernel hook API.
//!
//! It loads the driver (or reuses a live one), then
//! 1. checks the SDK-side guard rails (bad addresses/ids are refused),
//! 2. runs the driver's own kernel self tests (byte patch, shellcode,
//!    driver-built trampoline) against a leaf function in the driver image,
//! 3. runs a user-mode detour test against a function in *this* process,
//! 4. shuts the driver down again.
//!
//! Nothing outside the driver's own `.text` and this harness's own `.text`
//! is patched, so running this on a development machine cannot disturb
//! foreign code. A `0` from a self test means the platform refused the
//! write (HVCI keeps text read-only); the rest of the run still passes.

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

/// User-mode hook target: several dependent register-only operations give a
/// prologue long enough for either jump form with no RIP-relative operand,
/// so the driver-built trampoline needs no rebasing.
#[inline(never)]
extern "system" fn user_hook_target(input: u64) -> u64 {
    let mut value = core::hint::black_box(input);
    value = value.rotate_left(13);
    value = value.wrapping_mul(0x1234_5678_9ABC_DEF1);
    value ^ 0x1111_1111
}

fn run() -> Result<(), String> {
    ks_sdk::set_log_sink(|line| println!("kdu: {line}"));
    println!("hook-test: starting driver");
    match ks_sdk::start() {
        Ok(()) => println!("hook-test: driver started"),
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
    // pid=0 requires kernel addresses.
    expect_err(
        "null kernel target",
        ks_sdk::hook_create(0, 0, 0xFFFF_F800_0000_1000),
    )?;
    expect_err(
        "user-mode target for kernel pid",
        ks_sdk::hook_create(0, 0x1000, 0x1000),
    )?;
    // A user pid requires user addresses.
    expect_err(
        "kernel target for user pid",
        ks_sdk::hook_create(0x1000, 0xFFFF_F800_0000_1000, 0xFFFF_F800_0000_2000),
    )?;
    expect_err("id 0 enable", ks_sdk::hook_enable(0, 0))?;
    expect_err("unknown id remove", ks_sdk::hook_remove(0, 0xDEAD_BEEF))?;
    expect_err(
        "user-mode shellcode target",
        ks_sdk::hook_shellcode(0x1000, 0xFFFF_F800_0000_1000, &[0xC3]),
    )?;
    expect_err(
        "empty shellcode",
        ks_sdk::hook_shellcode(0, 0xFFFF_F800_0000_1000, &[]),
    )?;
    let too_large = vec![0x90u8; ks_core::protocol::MAX_HOOK_SHELLCODE + 1];
    expect_err(
        "oversize shellcode",
        ks_sdk::hook_shellcode(0, 0xFFFF_F800_0000_1000, &too_large),
    )?;
    println!("hook-test: guard rails ok");

    println!("hook-test: driver self test (create/enable/verify/disable/remove)");
    let patched = ks_sdk::hook_self_test().map_err(|error| format!("self test: {error}"))?;
    match patched {
        1 => println!("hook-test: PASS - jump applied and original bytes restored"),
        0 => println!("hook-test: SKIPPED - the platform refused the write (HVCI)"),
        other => return Err(format!("unexpected self-test result {other}")),
    }

    println!("hook-test: shellcode self test (alloc/copy/hook/call/disable/remove)");
    let shellcode = ks_sdk::hook_shellcode_self_test()
        .map_err(|error| format!("shellcode self test: {error}"))?;
    match shellcode {
        1 => println!("hook-test: PASS - stub executed through the hook, original restored"),
        0 => println!("hook-test: SKIPPED - executable allocation refused (HVCI)"),
        other => return Err(format!("unexpected shellcode self-test result {other}")),
    }

    println!("hook-test: detour self test (driver-built trampoline)");
    let detour =
        ks_sdk::hook_detour_self_test().map_err(|error| format!("detour self test: {error}"))?;
    match detour {
        1 => println!("hook-test: PASS - detour called the original prologue via the trampoline"),
        0 => println!("hook-test: SKIPPED - executable allocation refused (HVCI)"),
        other => return Err(format!("unexpected detour self-test result {other}")),
    }

    // ---- User-mode end-to-end test ----
    // Hook a function in this process: create → enable → call → verify,
    // disable → call → verify, remove.
    println!("hook-test: user-mode detour test (this process)");
    let own_pid = std::process::id() as u64;
    let target_address = user_hook_target as *const () as u64;

    let probe = ks_sdk::hook_probe(own_pid, target_address)
        .map_err(|error| format!("user probe: {error}"))?;
    println!(
        "hook-test: probed user target: replaced_len={} rip_relative={} relative_branch={}",
        probe.replaced_len, probe.rip_relative, probe.relative_branch
    );
    if probe.relative_branch {
        println!("hook-test: SKIPPED - prologue has a relative branch");
    } else {
        let expected = user_hook_target(0x42);
        // call [rip-14]; add rax, 1; ret — 64-bit add so the full return
        // value is preserved (matches ShellcodeBuilder::add_rax_imm8).
        let code = [
            0xFFu8, 0x15, 0xF2, 0xFF, 0xFF, 0xFF, 0x48, 0x83, 0xC0, 0x01, 0xC3,
        ];
        let hook = ks_sdk::hook_detour(own_pid, target_address, &code)
            .map_err(|error| format!("user detour: {error}"))?;
        println!(
            "hook-test: user detour installed: id={} replaced_len={} code=0x{:X} tramp=0x{:X}",
            hook.id, hook.replaced_len, hook.code_address, hook.trampoline_address
        );
        ks_sdk::hook_enable(own_pid, hook.id).map_err(|error| format!("user enable: {error}"))?;
        let through = user_hook_target(0x42);
        ks_sdk::hook_disable(own_pid, hook.id).map_err(|error| format!("user disable: {error}"))?;
        let restored = user_hook_target(0x42);
        ks_sdk::hook_remove(own_pid, hook.id).map_err(|error| format!("user remove: {error}"))?;
        if through == expected.wrapping_add(1) && restored == expected {
            println!(
                "hook-test: PASS - user detour observed {through:#x} (original {expected:#x}+1)"
            );
        } else {
            return Err(format!(
                "user detour mismatch: through={through:#x} restored={restored:#x} expected={expected:#x}"
            ));
        }
    }

    println!("hook-test: shellcode builder");
    let code = {
        use ks_sdk::shellcode::{Reg, ShellcodeBuilder};
        ShellcodeBuilder::new()
            .push_regs(&[Reg::Rax, Reg::Rcx])
            .call_original()
            .pop_regs(&[Reg::Rax, Reg::Rcx])
            .ret()
            .build()
    };
    if code.len() != 2 + 6 + 2 + 1 {
        return Err(format!("unexpected builder length {}", code.len()));
    }
    println!("hook-test: builder produced {} bytes", code.len());

    println!("hook-test: stopping driver");
    if let Err(error) = ks_sdk::stop() {
        if ks_sdk::instance_claim_present().unwrap_or(false) {
            return Err(format!("stop: {error}"));
        }
    }

    // A crashed run leaves the registry publication behind while the ring
    // is gone. `start` must judge that residue and recover: forge it here
    // (no driver is live at this point) and verify a fresh instance comes
    // up with new names.
    println!("hook-test: stale registry recovery");
    forge_stale_publication()?;
    if !ks_sdk::instance_claim_present().unwrap_or(false) {
        return Err("forged Instance claim is not visible".into());
    }
    let forged = ks_sdk::published_object_names();
    ks_sdk::start().map_err(|error| format!("start over stale residue: {error}"))?;
    if ks_sdk::published_object_names() == forged {
        return Err("stale publication survived the restart".into());
    }
    ks_sdk::ping().map_err(|error| format!("ping after residue recovery: {error}"))?;
    println!("hook-test: PASS - stale publication cleaned, fresh instance answers");
    ks_sdk::stop().map_err(|error| format!("stop after recovery: {error}"))?;

    println!("hook-test: done");
    Ok(())
}

/// Writes a fake `HKLM\SOFTWARE\KernelScript` publication: an `Instance`
/// claim plus the three object-name values. Needs elevation (the harness
/// has it), and no live driver may exist at this point.
fn forge_stale_publication() -> Result<(), String> {
    const KEY: &str = r"HKLM\SOFTWARE\KernelScript";
    let values = [
        ("Instance", "REG_DWORD", "1"),
        (
            "SectionName",
            "REG_SZ",
            r"\BaseNamedObjects\KernelScriptSection-deadbeefdeadbeef",
        ),
        (
            "RequestEventName",
            "REG_SZ",
            r"\BaseNamedObjects\KernelScriptRequest-deadbeefdeadbeef",
        ),
        (
            "ResponseEventName",
            "REG_SZ",
            r"\BaseNamedObjects\KernelScriptResponse-deadbeefdeadbeef",
        ),
    ];
    for (name, kind, data) in values {
        let status = std::process::Command::new("reg")
            .args(["add", KEY, "/v", name, "/t", kind, "/d", data, "/f"])
            .status()
            .map_err(|error| format!("reg add: {error}"))?;
        if !status.success() {
            return Err(format!("reg add {name} failed"));
        }
    }
    Ok(())
}

fn expect_err<T>(what: &str, result: Result<T, ks_sdk::LinkError>) -> Result<(), String> {
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err(format!("{what} was accepted")),
    }
}
