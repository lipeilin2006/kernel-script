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
    check_lock_list(t, failures);
    check_alloc(t, failures);
    check_protect(t, failures);
    check_process_suspend(failures);

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

/// Lock-list export: the driver dumps every lock it is replaying, including
/// ones created by other clients (a Lua script, another tool). The UI relies
/// on this to show foreign locks.
fn check_lock_list(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "lock list reports driver locks", || {
        const LOCK_ID: u64 = 0xA1B2_C3D4;
        let address = t.slot(0xA00);
        // A lock that this client did not create would need a second
        // process; the wire format is the same either way, so verifying our
        // own lock here proves the export works for Lua-created locks too.
        ks_sdk::lock(LOCK_ID, t.pid, address, &[0x5A, 0x5A]).map_err(|e| format!("lock: {e}"))?;
        thread::sleep(LOCK_SETTLE);

        let list = ks_sdk::lock_list().map_err(|e| format!("lock_list: {e}"))?;
        let entry = list
            .iter()
            .find(|entry| entry.id == LOCK_ID)
            .ok_or_else(|| format!("lock {LOCK_ID:#x} missing from the list"))?;
        if entry.pid != t.pid {
            return Err(format!("lock pid {}, want {}", entry.pid, t.pid));
        }
        if entry.address != address {
            return Err(format!(
                "lock address {:#X}, want {address:#X}",
                entry.address
            ));
        }
        if entry.len != 2 {
            return Err(format!("lock len {}, want 2", entry.len));
        }

        ks_sdk::unlock(LOCK_ID).map_err(|e| format!("unlock: {e}"))?;
        thread::sleep(LOCK_SETTLE);
        let list = ks_sdk::lock_list().map_err(|e| format!("lock_list: {e}"))?;
        if list.iter().any(|entry| entry.id == LOCK_ID) {
            return Err("unlocked lock still present in the list".into());
        }
        Ok(())
    });
}

/// Target-process allocation: reserve/commit a page in this process,
/// write and read it back through the normal (non-MDL) paths, then free
/// it. The second check covers the guard rails the driver enforces before
/// it touches the address space.
fn check_alloc(t: &Target, failures: &mut Vec<(String, String)>) {
    check(failures, "alloc/write/read/free roundtrip", || {
        let size = 0x1000usize;
        let address = ks_sdk::alloc(t.pid, size, ks_core::protocol::ALLOC_PROTECT_READWRITE)
            .map_err(|e| format!("alloc: {e}"))?;
        if address == 0 {
            return Err("alloc returned a null base".into());
        }
        let pattern: Vec<u8> = (0..64u8)
            .map(|index| index.wrapping_mul(7).wrapping_add(3))
            .collect();
        let write = ks_sdk::write_bytes(t.pid, address, &pattern, false, false)
            .map_err(|e| format!("write: {e}"));
        let read = ks_sdk::read_bytes(t.pid, address, pattern.len(), false, false)
            .map_err(|e| format!("read: {e}"));
        let freed = ks_sdk::free(t.pid, address).map_err(|e| format!("free: {e}"));
        write?;
        let data = read?;
        freed?;
        if data != pattern {
            return Err(format!(
                "read back {:02X?}, want {:02X?}",
                &data[..data.len().min(16)],
                &pattern[..pattern.len().min(16)]
            ));
        }
        Ok(())
    });

    check(failures, "alloc rejects oversize and bad protect", || {
        if ks_sdk::alloc(
            t.pid,
            ks_core::protocol::MAX_ALLOC_SIZE + 1,
            ks_core::protocol::ALLOC_PROTECT_READWRITE,
        )
        .is_ok()
        {
            return Err("oversize allocation accepted".into());
        }
        if ks_sdk::alloc(t.pid, 0x1000, 0x80).is_ok() {
            return Err("unsupported protection accepted".into());
        }
        // Read-only allocations are refused: the plain write path could
        // never touch the page (only the MDL path could).
        if ks_sdk::alloc(t.pid, 0x1000, ks_core::protocol::PROT_READ).is_ok() {
            return Err("read-only allocation accepted".into());
        }
        // A second free must fail because the VAD is gone. `free` itself
        // checks no ownership (see the SDK guide): the first call released
        // the region, so the address no longer exists.
        let address = ks_sdk::alloc(t.pid, 0x1000, ks_core::protocol::ALLOC_PROTECT_READWRITE)
            .map_err(|e| format!("alloc for double free: {e}"))?;
        ks_sdk::free(t.pid, address).map_err(|e| format!("first free: {e}"))?;
        if ks_sdk::free(t.pid, address).is_ok() {
            return Err("double free succeeded".into());
        }
        Ok(())
    });

    // Kernel hooks are only exercised through their guard rails: creating a
    // real one would patch live kernel code on the development machine.
    check(
        failures,
        "kernel hook rejects bad addresses and ids",
        || {
            // pid=0 (kernel) requires kernel addresses.
            if ks_sdk::hook_create(0, 0, 0xFFFF_F800_0000_1000).is_ok() {
                return Err("null target accepted".into());
            }
            if ks_sdk::hook_create(0, 0x1000, 0x1000).is_ok() {
                return Err("user-mode target accepted for pid=0".into());
            }
            // A user pid requires user addresses.
            if ks_sdk::hook_create(0x1000, 0xFFFF_F800_0000_1000, 0xFFFF_F800_0000_2000).is_ok() {
                return Err("kernel target accepted for user pid".into());
            }
            if ks_sdk::hook_enable(0, 0).is_ok() {
                return Err("id 0 accepted".into());
            }
            if ks_sdk::hook_remove(0, 0xDEAD_BEEF).is_ok() {
                return Err("unknown id removed".into());
            }
            // Nothing was created, so the bulk calls must touch nothing.
            if ks_sdk::hook_disable_all().map_err(|e| e.to_string())? != 0 {
                return Err("disable_all touched an empty table".into());
            }
            Ok(())
        },
    );
}

/// Page protection and region queries: probe the harness's own mappings,
/// walk an allocation through RW -> RX -> RWX -> free, exercise the CFG
/// modifier, and verify a stale span fails with the driver's own NTSTATUS
/// instead of being swallowed.
fn check_protect(t: &Target, failures: &mut Vec<(String, String)>) {
    use ks_core::protocol::{
        MAX_PROTECT_SIZE, MEM_COMMIT, MEM_IMAGE, MEM_PRIVATE, PROT_RW, PROT_RWX, PROT_RX,
    };

    check(failures, "query_region reports committed regions", || {
        let heap =
            ks_sdk::query_region(t.pid, t.buffer_addr).map_err(|e| format!("heap query: {e}"))?;
        if heap.state != MEM_COMMIT {
            return Err(format!("heap state {:#x}, want MEM_COMMIT", heap.state));
        }
        if heap.typ != MEM_PRIVATE {
            return Err(format!("heap type {:#x}, want MEM_PRIVATE", heap.typ));
        }
        if t.buffer_addr < heap.base || t.buffer_addr >= heap.base + heap.region_size {
            return Err(format!(
                "buffer {:#x} outside its region {:#x} + {:#x}",
                t.buffer_addr, heap.base, heap.region_size
            ));
        }
        // Walk the image from its base until an executable region: the
        // header page is read-only and `.text` starts its own RX region.
        let mut address = t.image_base;
        let mut found_exec = false;
        for _ in 0..16 {
            let region =
                ks_sdk::query_region(t.pid, address).map_err(|e| format!("image query: {e}"))?;
            if region.region_size == 0 {
                break;
            }
            if region.typ == MEM_IMAGE && (region.protect == PROT_RX || region.protect == PROT_RWX)
            {
                found_exec = true;
                break;
            }
            address = region.base + region.region_size;
        }
        if !found_exec {
            return Err("no executable MEM_IMAGE region above the image base".into());
        }
        Ok(())
    });

    check(failures, "alloc/protect/free roundtrip", || {
        let address = ks_sdk::alloc(t.pid, 0x1000, PROT_RW).map_err(|e| format!("alloc: {e}"))?;
        let probe = ks_sdk::query_region(t.pid, address).map_err(|e| format!("query: {e}"))?;
        if probe.state != MEM_COMMIT || probe.typ != MEM_PRIVATE || probe.protect != PROT_RW {
            return Err(format!(
                "fresh alloc reports state {:#x} type {:#x} protect {:#x}",
                probe.state, probe.typ, probe.protect
            ));
        }
        // RW -> RX: the old protection comes back, reads keep working, and
        // a one-byte request still reports the page the kernel covered.
        let (old, covered) = ks_sdk::protect_span(t.pid, address, 1, PROT_RX, false)
            .map_err(|e| format!("protect RX: {e}"))?;
        if covered != 0x1000 {
            return Err(format!(
                "protect_span covered {covered} bytes, want one page"
            ));
        }
        if old != PROT_RW {
            return Err(format!("old protect {old:#x}, want PROT_RW"));
        }
        let probe = ks_sdk::query_region(t.pid, address).map_err(|e| format!("query: {e}"))?;
        if probe.protect != PROT_RX {
            return Err(format!("region protect {:#x}, want PROT_RX", probe.protect));
        }
        let data = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read on RX page: {e}"))?;
        if data.len() != 4 {
            return Err(format!("read returned {} bytes", data.len()));
        }
        // RX -> RWX: writes now work, which is the point of the feature.
        let old = ks_sdk::protect(t.pid, address, 0x1000, PROT_RWX)
            .map_err(|e| format!("protect RWX: {e}"))?;
        if old != PROT_RX {
            return Err(format!("old protect {old:#x}, want PROT_RX"));
        }
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        ks_sdk::write_bytes(t.pid, address, &payload, false, false)
            .map_err(|e| format!("write on RWX page: {e}"))?;
        let readback = ks_sdk::read_bytes(t.pid, address, 4, false, false)
            .map_err(|e| format!("read back: {e}"))?;
        if readback != payload {
            return Err(format!("read back {readback:02X?}, want {payload:02X?}"));
        }
        // Back to plain read/write, then release.
        let old = ks_sdk::protect(t.pid, address, 0x1000, PROT_RW)
            .map_err(|e| format!("restore RW: {e}"))?;
        if old != PROT_RWX {
            return Err(format!("old protect {old:#x}, want PROT_RWX"));
        }
        ks_sdk::free(t.pid, address).map_err(|e| format!("free: {e}"))
    });

    check(
        failures,
        "protect rejects bad spans and stale addresses",
        || {
            // Client-side guard rails: rejected before any round trip.
            if ks_sdk::protect(t.pid, 0, 0x1000, PROT_RW).is_ok() {
                return Err("null address accepted".into());
            }
            if ks_sdk::protect(t.pid, t.buffer_addr, 0, PROT_RW).is_ok() {
                return Err("zero size accepted".into());
            }
            if ks_sdk::protect(t.pid, t.buffer_addr, 0x1000, 0x100).is_ok() {
                return Err("PAGE_GUARD accepted".into());
            }
            if ks_sdk::protect(t.pid, t.buffer_addr, 0x1000, 0x01).is_ok() {
                return Err("PAGE_NOACCESS accepted".into());
            }
            if ks_sdk::protect(t.pid, t.buffer_addr, MAX_PROTECT_SIZE + 1, PROT_RW).is_ok() {
                return Err("oversize span accepted".into());
            }
            if ks_sdk::protect(0, t.buffer_addr, 0x1000, PROT_RW).is_ok() {
                return Err("pid 0 accepted".into());
            }
            // PAGE_TARGETS_NO_UPDATE only pairs with an executable
            // protection; the client refuses the rest up front.
            if ks_sdk::protect_ex(t.pid, t.buffer_addr, 0x1000, PROT_RW, true).is_ok() {
                return Err("cfg_no_update accepted with PROT_RW".into());
            }
            // Driver-side: a span that no longer exists must come back as the
            // kernel's own failure status — never as success, and never
            // flattened into a generic client-side error.
            let address =
                ks_sdk::alloc(t.pid, 0x1000, PROT_RW).map_err(|e| format!("alloc: {e}"))?;
            ks_sdk::free(t.pid, address).map_err(|e| format!("free: {e}"))?;
            match ks_sdk::protect(t.pid, address, 0x1000, PROT_RW) {
                Err(ks_sdk::LinkError::NtStatus(status)) if status < 0 => {}
                Err(other) => {
                    return Err(format!(
                        "stale span failed as {other}, want a driver NTSTATUS"
                    ));
                }
                Ok(old) => return Err(format!("stale span accepted (old {old:#x})")),
            }
            // The CFG modifier travels the same path.
            let address =
                ks_sdk::alloc(t.pid, 0x1000, PROT_RW).map_err(|e| format!("alloc: {e}"))?;
            let old = ks_sdk::protect_ex(t.pid, address, 0x1000, PROT_RX, true)
                .map_err(|e| format!("protect_ex: {e}"))?;
            if old != PROT_RW {
                return Err(format!("cfg-modifier old protect {old:#x}, want PROT_RW"));
            }
            ks_sdk::free(t.pid, address).map_err(|e| format!("free: {e}"))
        },
    );
}

/// Suspend/resume: spawns a heartbeat child, freezes it, verifies the
/// heartbeat stops growing, resumes it, verifies it grows again. Also checks
/// the guard rails (System pid, unknown pid).
fn check_process_suspend(failures: &mut Vec<(String, String)>) {
    check(failures, "suspend rejects System and unknown pids", || {
        if ks_sdk::suspend(0).is_ok() {
            return Err("pid 0 (Idle) accepted".into());
        }
        if ks_sdk::suspend(4).is_ok() {
            return Err("pid 4 (System) accepted".into());
        }
        // A never-suspended pid is a no-op, not an error.
        if ks_sdk::suspend(0xDEAD_BEEF).is_ok() {
            return Err("unknown pid suspended".into());
        }
        Ok(())
    });

    check(
        failures,
        "suspend freezes a child process, resume continues it",
        || {
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            let beat =
                std::env::temp_dir().join(format!("ks-test-heartbeat-{}.bin", std::process::id()));
            let _ = std::fs::remove_file(&beat);
            let mut child = std::process::Command::new(&exe)
                .arg("heartbeat")
                .arg(&beat)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn heartbeat: {e}"))?;

            // Give the child time to start appending.
            std::thread::sleep(Duration::from_millis(300));
            let pid = child.id() as u64;
            let size_before = std::fs::metadata(&beat).map(|m| m.len()).unwrap_or(0);
            if size_before == 0 {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&beat);
                return Err("heartbeat child produced no output".into());
            }

            // Suspend and confirm the heartbeat freezes.
            ks_sdk::suspend(pid).map_err(|e| format!("suspend: {e}"))?;
            std::thread::sleep(Duration::from_millis(350));
            let size_frozen = std::fs::metadata(&beat).map(|m| m.len()).unwrap_or(0);
            // Resume and confirm the heartbeat continues.
            ks_sdk::resume(pid).map_err(|e| format!("resume: {e}"))?;
            std::thread::sleep(Duration::from_millis(350));
            let size_after = std::fs::metadata(&beat).map(|m| m.len()).unwrap_or(0);

            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&beat);

            if size_frozen > size_before {
                return Err(format!(
                    "heartbeat grew while suspended: {size_before} -> {size_frozen}"
                ));
            }
            if size_after <= size_frozen {
                return Err(format!(
                    "heartbeat did not resume: {size_frozen} -> {size_after}"
                ));
            }
            Ok(())
        },
    );
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
