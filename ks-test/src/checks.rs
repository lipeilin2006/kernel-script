//! The correctness suite: each domain is one step function so a bugcheck
//! names the culprit from the last `RUN` marker in the step log.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::step_log::say;
use crate::target::{check, read_is, u32_at, Target, RVA_TARGET};

/// Settle time around lock checks: the driver's merged worker rewrites one
/// lock entry per loop pass between requests, at full speed, so a fraction
/// of a second is always enough for sweeps to apply or stop.
const LOCK_SETTLE: Duration = Duration::from_millis(200);

pub(crate) fn run_checks(target: &Arc<Target>, failures: &mut Vec<(String, String)>) {
    let t: &Target = target;
    say("--- Correctness ---");

    check_discovery(t, failures);
    check_plain_rw(t, failures);
    check_rva_mdl(t, failures);
    check_batch(t, failures);
    check_chain(t, failures);
    check_locks(t, failures);

    check(failures, "concurrent ring access", || {
        run_concurrency_smoke(target)
    });
}

/// Transport and discovery: ping, process enumeration, module base, and
/// the randomized registry publication.
fn check_discovery(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "ping", || {
        ks_sdk::ping().map_err(|e| e.to_string())
    });

    check(failures, "get_pid(self)", || {
        let exe = std::env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))?
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or("current_exe has no file name")?;
        let pid = ks_sdk::find_pid(&exe).map_err(|e| format!("{exe}: {e}"))?;
        if pid == t.pid {
            Ok(())
        } else {
            Err(format!("found pid {pid}, want {}", t.pid))
        }
    });

    check(failures, "get_pid(negative)", || {
        if ks_sdk::find_pid("definitely-not-a-real-process.exe").is_err() {
            Ok(())
        } else {
            Err("enumeration found a process that cannot exist".into())
        }
    });

    check(failures, "get_process_base == image base", || {
        if t.driver_base == t.image_base {
            Ok(())
        } else {
            Err(format!(
                "driver reports 0x{:X}, user mode sees 0x{:X}",
                t.driver_base, t.image_base
            ))
        }
    });

    check(failures, "get_process_base(invalid pid) errors", || {
        // 0xFFFF_FFFE is virtually certain not to exist; the driver must
        // fail the lookup instead of reporting a base.
        match ks_sdk::get_process_base(0xFFFF_FFFE) {
            Ok(base) => Err(format!("nonexistent pid reported base 0x{base:X}")),
            Err(_) => Ok(()),
        }
    });

    check(failures, "find_pid locates this process", || {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let name = exe
            .file_name()
            .ok_or_else(|| "exe has no file name".to_string())?
            .to_string_lossy()
            .into_owned();
        let found = ks_sdk::find_pid(&name).map_err(|e| format!("find_pid({name}): {e}"))?;
        if found == t.pid {
            Ok(())
        } else {
            Err(format!("find_pid returned {found}, want {}", t.pid))
        }
    });

    check(
        failures,
        "registry publishes randomized object names",
        || {
            // Strict read: no fallback to the compiled-in names, so a driver
            // that never wrote the key fails this check.
            let names = ks_sdk::published_object_names_strict()
                .ok_or_else(|| "HKLM\\SOFTWARE\\KernelScript values missing".to_string())?;
            // Each published name must be the client-side default plus the
            // driver's `-<16 hex>` startup token: prefix intact, token well
            // formed, and the plain default (token-less) never published.
            let defaults = [
                ks_core::ring::SECTION_CLIENT_NAME,
                ks_core::ring::REQUEST_EVENT_CLIENT_NAME,
                ks_core::ring::RESPONSE_EVENT_CLIENT_NAME,
            ];
            let prefixes = [
                "Global\\KernelScriptSection-",
                "Global\\KernelScriptRequest-",
                "Global\\KernelScriptResponse-",
            ];
            for (index, name) in names.iter().enumerate() {
                let prefix = prefixes[index];
                let Some(token) = name.strip_prefix(prefix) else {
                    return Err(format!(
                        "registry value {index} is {name:?}, want {prefix}<16 hex>"
                    ));
                };
                if token.len() != 16 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!(
                        "registry value {index} token {token:?} is not 16 hex chars"
                    ));
                }
                if name.as_str() == defaults[index] {
                    return Err(format!(
                        "registry value {index} published the fixed default"
                    ));
                }
            }
            Ok(())
        },
    );
}

/// Plain (non-MDL, non-RVA) reads and writes against the heap buffer.
fn check_plain_rw(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "write_i32/read_i32 roundtrip", || {
        let address = t.slot(0x400);
        ks_sdk::write_bytes(t.pid, address, &0x1234_5678i32.to_le_bytes(), false, false)
            .map_err(|e| format!("write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read: {e}"))?;
        if u32_at(&data, 0) == 0x1234_5678 {
            Ok(())
        } else {
            Err(format!("read back 0x{:X}", u32_at(&data, 0)))
        }
    });

    check(failures, "read_bytes(64) content", || {
        read_is(t, t.slot(0x40), &t.buffer[0x40..0x80])
    });

    check(failures, "write_bytes/read_bytes roundtrip (2 KiB)", || {
        // 2 KiB at 0x800: exactly to the end of the buffer, clear of the
        // bookkeeping fields at the front.
        let address = t.slot(0x800);
        let pattern: Vec<u8> = (0..=255u8).cycle().take(2048).collect();
        ks_sdk::write_bytes(t.pid, address, &pattern, false, false)
            .map_err(|e| format!("write: {e}"))?;
        read_is(t, address, &pattern)
    });

    check(failures, "read size > 4096 rejected", || {
        // The protocol caps single reads/writes at 4096 bytes; 4097 must
        // fail client-side validation before anything reaches the driver.
        match ks_sdk::read_bytes(t.pid, t.slot(0), 4097, false, false) {
            Ok(_) => Err("4097-byte read was accepted".into()),
            Err(_) => Ok(()),
        }
    });
}

/// RVA-resolved and MDL-remap paths against the image static.
fn check_rva_mdl(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "read_rva(0) == MZ", || {
        let data =
            ks_sdk::read_bytes(t.pid, 0, 2, true, false).map_err(|e| format!("read: {e}"))?;
        if data.as_slice() == b"MZ" {
            Ok(())
        } else {
            Err(format!("image header reads {data:02X?}"))
        }
    });

    check(failures, "write_rva/read_rva roundtrip (image)", || {
        let value = 0xABCD_1234u32;
        ks_sdk::write_bytes(t.pid, t.rva_target_rva, &value.to_le_bytes(), true, false)
            .map_err(|e| format!("write_rva: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, false)
            .map_err(|e| format!("read_rva: {e}"))?;
        if u32_at(&data, 0) != value {
            return Err(format!("rva read back 0x{:X}", u32_at(&data, 0)));
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen == value {
            Ok(())
        } else {
            Err(format!("user view is 0x{seen:X}, want 0x{value:X}"))
        }
    });

    check(
        failures,
        "write_mdl_rva(0) identity roundtrip (image header)",
        || {
            // address == 0 with the RVA flag is legal (RVA 0 = module base).
            // The DOS header page is read-only, so the identity write goes
            // through the MDL-remap path, which must bypass page protection.
            let data = ks_sdk::read_bytes(t.pid, 0, 2, true, false)
                .map_err(|e| format!("read_rva: {e}"))?;
            if data.as_slice() != b"MZ" {
                return Err(format!("header reads {data:02X?}"));
            }
            ks_sdk::write_bytes(t.pid, 0, &data, true, true)
                .map_err(|e| format!("write_mdl_rva(0): {e}"))?;
            let again = ks_sdk::read_bytes(t.pid, 0, 2, true, false)
                .map_err(|e| format!("re-read: {e}"))?;
            if again == data {
                Ok(())
            } else {
                Err(format!("header now reads {again:02X?}"))
            }
        },
    );

    check(failures, "read_mdl roundtrip", || {
        // Seed through the plain path, then read the same bytes back
        // through the MDL-remap path.
        let address = t.slot(0x40);
        let pattern: Vec<u8> = (0..=255u8).cycle().take(512).collect();
        ks_sdk::write_bytes(t.pid, address, &pattern, false, false)
            .map_err(|e| format!("seed write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, pattern.len(), false, true)
            .map_err(|e| format!("mdl read: {e}"))?;
        if data.as_slice() == pattern.as_slice() {
            Ok(())
        } else {
            Err("mdl read content mismatch".into())
        }
    });

    check(failures, "write_mdl/read_mdl roundtrip", || {
        let address = t.slot(0x40);
        let pattern = vec![0x5Au8; 256];
        ks_sdk::write_bytes(t.pid, address, &pattern, false, true)
            .map_err(|e| format!("mdl write: {e}"))?;
        let data = ks_sdk::read_bytes(t.pid, address, pattern.len(), false, false)
            .map_err(|e| format!("plain read: {e}"))?;
        if data.as_slice() == pattern.as_slice() {
            Ok(())
        } else {
            Err("plain read after mdl write mismatch".into())
        }
    });

    check(
        failures,
        "write_mdl_rva/read_mdl_rva roundtrip (image)",
        || {
            let value = 0x7777_7777u32;
            ks_sdk::write_bytes(t.pid, t.rva_target_rva, &value.to_le_bytes(), true, true)
                .map_err(|e| format!("write_mdl_rva: {e}"))?;
            let data = ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, true)
                .map_err(|e| format!("read_mdl_rva: {e}"))?;
            if u32_at(&data, 0) != value {
                return Err(format!("mdl_rva read back 0x{:X}", u32_at(&data, 0)));
            }
            let seen = RVA_TARGET.load(Ordering::SeqCst);
            if seen == value {
                Ok(())
            } else {
                Err(format!("user view is 0x{seen:X}, want 0x{value:X}"))
            }
        },
    );
}

/// Batch read/write: contents, per-entry statuses and the client-side
/// entry-count rejection.
fn check_batch(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "batch_read content", || {
        let addresses: Vec<u64> = (0..8).map(|i| t.slot(0x800 + i * 4)).collect();
        let seeds: Vec<u32> = (0..8u32).map(|i| 0x1000_0000 + i).collect();
        for (index, seed) in seeds.iter().enumerate() {
            ks_sdk::write_bytes(t.pid, addresses[index], &seed.to_le_bytes(), false, false)
                .map_err(|e| format!("seed write {index}: {e}"))?;
        }
        let data =
            ks_sdk::batch_read(t.pid, 4, &addresses).map_err(|e| format!("batch_read: {e}"))?;
        if data.len() != addresses.len() * 4 {
            return Err(format!("returned {} bytes", data.len()));
        }
        for (index, seed) in seeds.iter().enumerate() {
            if u32_at(&data, index * 4) != *seed {
                return Err(format!("slot {index} mismatch"));
            }
        }
        Ok(())
    });

    check(failures, "batch_write statuses + readback", || {
        let addresses: Vec<u64> = (0..8).map(|i| t.slot(0x880 + i * 4)).collect();
        let entries: Vec<(u64, Vec<u8>)> = (0..8u32)
            .map(|i| {
                (
                    addresses[i as usize],
                    (0x2000_0000u32 + i).to_le_bytes().to_vec(),
                )
            })
            .collect();
        let statuses =
            ks_sdk::batch_write(t.pid, &entries).map_err(|e| format!("batch_write: {e}"))?;
        if statuses.len() != entries.len() {
            return Err(format!(
                "{} statuses for {} entries",
                statuses.len(),
                entries.len()
            ));
        }
        if let Some(bad) = statuses.iter().position(|&status| status != 0) {
            return Err(format!("entry {bad} status {:#x}", statuses[bad]));
        }
        for (index, entry) in entries.iter().enumerate() {
            let data = ks_sdk::read_bytes(t.pid, entry.0, 4, false, false)
                .map_err(|e| format!("readback {index}: {e}"))?;
            if u32_at(&data, 0) != 0x2000_0000 + index as u32 {
                return Err(format!("slot {index} mismatch after batch write"));
            }
        }
        Ok(())
    });

    check(failures, "batch_write rejects 65 entries", || {
        let entries: Vec<(u64, Vec<u8>)> = (0..65u64)
            .map(|i| (t.slot(0xC00 + i * 4), vec![0u8; 4]))
            .collect();
        match ks_sdk::batch_write(t.pid, &entries) {
            Err(_) => Ok(()),
            Ok(_) => Err("65-entry batch was accepted".into()),
        }
    });
}

/// Pointer-chain walks: a real two-hop chain and the offset-count cap.
fn check_chain(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "traverse_pointer_chain", || {
        let hop1 = t.buffer_addr + 0x300;
        let hop2 = t.buffer_addr + 0x200;
        ks_sdk::write_bytes(t.pid, t.slot(0x100), &hop1.to_le_bytes(), false, false)
            .map_err(|e| format!("seed hop1: {e}"))?;
        ks_sdk::write_bytes(t.pid, t.slot(0x300), &hop2.to_le_bytes(), false, false)
            .map_err(|e| format!("seed hop2: {e}"))?;
        let final_address = ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &[0x100, 0x0])
            .map_err(|e| format!("traverse: {e}"))?;
        if final_address == t.slot(0x200) {
            Ok(())
        } else {
            Err(format!(
                "walked to 0x{final_address:X}, want 0x{:X}",
                t.slot(0x200)
            ))
        }
    });

    check(failures, "traverse chain limit enforced", || {
        // 33 offsets exceed MAX_CHAIN_OFFSETS; the protocol must reject the
        // request before it reaches the driver.
        let offsets: Vec<u64> = (0..33).map(|i| 8 * i as u64).collect();
        match ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &offsets) {
            Err(_) => Ok(()),
            Ok(_) => Err("33-offset chain was accepted".into()),
        }
    });
}

/// Driver-side memory locks: rewrite wins over user-mode writes, unlocks
/// stop the rewrite, and the table quota rejects the 65th entry.
fn check_locks(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "lock lifecycle (driver rewrite + unlock)", || {
        let locked = 0x1111_1111u32;
        let foreign = 0x2222_2222u32;
        let freed = 0x3333_3333u32;
        let address = t.slot(0x700);
        let rva_locked = 0x5555_5555u32;

        ks_sdk::lock(1, t.pid, address, &locked.to_le_bytes()).map_err(|e| format!("lock: {e}"))?;
        // The RVA lock exercises lock_rva/unlock_rva on the image static.
        ks_sdk::lock_rva(2, t.pid, t.rva_target_rva, &rva_locked.to_le_bytes())
            .map_err(|e| format!("lock_rva: {e}"))?;
        thread::sleep(LOCK_SETTLE);

        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read locked: {e}"))?;
        if u32_at(&data, 0) != locked {
            return Err(format!("locked slot reads 0x{:X}", u32_at(&data, 0)));
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != rva_locked {
            return Err(format!("rva locked static reads 0x{seen:X}"));
        }

        // A plain user-mode write must lose against the driver rewrite.
        ks_sdk::write_bytes(t.pid, address, &foreign.to_le_bytes(), false, false)
            .map_err(|e| format!("foreign write: {e}"))?;
        RVA_TARGET.store(0x6666_6666, Ordering::SeqCst);
        thread::sleep(LOCK_SETTLE);
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read after foreign write: {e}"))?;
        if u32_at(&data, 0) != locked {
            return Err("driver rewrite did not restore the locked value".into());
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != rva_locked {
            return Err(format!(
                "rva driver rewrite did not restore 0x{rva_locked:X}"
            ));
        }

        ks_sdk::unlock(1).map_err(|e| format!("unlock: {e}"))?;
        ks_sdk::unlock_rva(2).map_err(|e| format!("unlock_rva: {e}"))?;
        // Sweep-clear for this pid; harmless when the table is already
        // empty.
        ks_sdk::unlock_all(t.pid).map_err(|e| format!("unlock_all: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        ks_sdk::write_bytes(t.pid, address, &freed.to_le_bytes(), false, false)
            .map_err(|e| format!("write after unlock: {e}"))?;
        RVA_TARGET.store(0x8888_8888, Ordering::SeqCst);
        thread::sleep(LOCK_SETTLE);
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read after unlock: {e}"))?;
        if u32_at(&data, 0) != freed {
            return Err("driver rewrite kept running after unlock".into());
        }
        let seen = RVA_TARGET.load(Ordering::SeqCst);
        if seen != 0x8888_8888 {
            return Err("rva driver rewrite kept running after unlock_rva".into());
        }
        Ok(())
    });

    check(failures, "lock table limit enforced", || {
        const SLOTS: u64 = ks_sdk::MAX_MEMORY_LOCKS as u64;
        // Fill every driver-side lock slot with a distinct 4-byte entry.
        for index in 0..SLOTS {
            ks_sdk::lock(
                index + 1,
                t.pid,
                t.slot(0x800 + index * 4),
                &0xA5A5_5A5Au32.to_le_bytes(),
            )
            .map_err(|e| format!("lock {}/{}: {e}", index + 1, SLOTS))?;
        }
        // A 65th distinct id must be rejected by the driver's quota; the
        // client maps STATUS_QUOTA_EXCEEDED back to TooManyEntries.
        match ks_sdk::lock(SLOTS + 1, t.pid, t.slot(0xF00), b"full") {
            Err(ks_sdk::LinkError::TooManyEntries { limit })
                if limit == ks_sdk::MAX_MEMORY_LOCKS => {}
            Err(error) => return Err(format!("65th lock failed as: {error}")),
            Ok(()) => return Err("65th lock was accepted".into()),
        }
        thread::sleep(LOCK_SETTLE);
        // All 64 entries are held: a foreign write must lose to the sweep.
        ks_sdk::write_bytes(t.pid, t.slot(0x800), &0u32.to_le_bytes(), false, false)
            .map_err(|e| format!("foreign write: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        let held = ks_sdk::read_bytes(t.pid, t.slot(0x800), 4, false, false)
            .map_err(|e| format!("read after foreign write: {e}"))?;
        if u32_at(&held, 0) != 0xA5A5_5A5A {
            return Err(format!("slot 0 not held: reads 0x{:X}", u32_at(&held, 0)));
        }
        // unlock_all clears the whole table for this pid: afterwards a
        // write must stick with no rewrite racing it.
        ks_sdk::unlock_all(t.pid).map_err(|e| format!("unlock_all: {e}"))?;
        ks_sdk::write_bytes(
            t.pid,
            t.slot(0x800),
            &0xDEAD_BEEFu32.to_le_bytes(),
            false,
            false,
        )
        .map_err(|e| format!("write after unlock_all: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        let cleared = ks_sdk::read_bytes(t.pid, t.slot(0x800), 4, false, false)
            .map_err(|e| format!("read after unlock_all: {e}"))?;
        if u32_at(&cleared, 0) != 0xDEAD_BEEF {
            return Err("a lock kept rewriting after unlock_all".into());
        }
        Ok(())
    });
}

/// Several threads hammer the single-slot ring through the client mutex:
/// readers keep verifying the magic word while writers churn dedicated
/// slots. Fails if any operation errors, a reader observes corruption, or
/// a writer slot ends up holding another writer's value.
fn run_concurrency_smoke(target: &Arc<Target>) -> Result<(), String> {
    const READS_PER_READER: usize = 150;
    const WRITES_PER_WRITER: usize = 100;
    const MAGIC: u32 = 0x1234_5678;

    let mut handles = Vec::new();

    for reader in 0..2usize {
        let target = Arc::clone(target);
        handles.push(thread::spawn(move || -> Result<(), String> {
            for _ in 0..READS_PER_READER {
                let data = ks_sdk::read_bytes(target.pid, target.slot(0x0), 4, false, false)
                    .map_err(|e| format!("reader {reader}: {e}"))?;
                if u32_at(&data, 0) != MAGIC {
                    return Err(format!("reader {reader}: magic slot corrupted"));
                }
            }
            Ok(())
        }));
    }

    for writer in 0..2usize {
        let target = Arc::clone(target);
        handles.push(thread::spawn(move || -> Result<(), String> {
            let address = target.slot(0x600 + writer as u64 * 4);
            for iteration in 0..WRITES_PER_WRITER as u32 {
                let value = ((writer as u32 + 1) << 16) | iteration;
                ks_sdk::write_bytes(target.pid, address, &value.to_le_bytes(), false, false)
                    .map_err(|e| format!("writer {writer}: {e}"))?;
            }
            Ok(())
        }));
    }

    // The main thread joins the contention with its own reads.
    for _ in 0..50 {
        ks_sdk::read_bytes(target.pid, target.slot(0x40), 64, false, false)
            .map_err(|e| format!("main thread read: {e}"))?;
    }

    let mut error = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => error = Some(err),
            Err(_) => error = Some("worker thread panicked".into()),
        }
    }
    if let Some(error) = error {
        return Err(error);
    }

    for writer in 0..2usize {
        let address = target.slot(0x600 + writer as u64 * 4);
        let data = ks_sdk::read_bytes(target.pid, address, 4, false, false)
            .map_err(|e| format!("final read slot {writer}: {e}"))?;
        let value = u32_at(&data, 0);
        if (value >> 16) as usize != writer + 1 {
            return Err(format!("writer slot {writer} holds 0x{value:X}"));
        }
    }
    Ok(())
}
