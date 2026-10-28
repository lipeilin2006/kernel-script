//! The `memory` module: synchronous driver round trips (and the window
//! geometry helper) exposed to Lua.

use std::sync::atomic::Ordering;

use mlua::Lua;

use super::types::Address;
use super::CONTENT_SCALE;

pub fn register_memory_api(lua: &Lua) -> mlua::Result<()> {
    let module = lua.create_table()?;

    module.set(
        "get_pid",
        lua.create_function(|_, name: String| -> mlua::Result<u64> {
            crate::sync_ipc::get_pid(&name).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "get_process_base",
        lua.create_function(|_, pid: u64| -> mlua::Result<u64> {
            crate::sync_ipc::get_process_base(pid).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "read_i32",
        lua.create_function(|_, (pid, address): (u64, Address)| -> mlua::Result<i32> {
            crate::sync_ipc::read_i32(pid, address.get()).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "read_bytes",
        lua.create_function(
            |_, (pid, address, size): (u64, Address, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid read size"));
                }
                crate::sync_ipc::read_bytes(pid, address.get(), size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_i32",
        lua.create_function(
            |_, (pid, address, value): (u64, Address, i32)| -> mlua::Result<()> {
                crate::sync_ipc::write_i32(pid, address.get(), value).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_bytes",
        lua.create_function(
            |_, (pid, address, data): (u64, Address, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid write size"));
                }
                crate::sync_ipc::write_bytes(pid, address.get(), &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_rva",
        lua.create_function(
            |_, (pid, relative_address, size): (u64, u64, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid RVA read size"));
                }
                crate::sync_ipc::read_rva(pid, relative_address, size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_rva",
        lua.create_function(
            |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid RVA write size"));
                }
                crate::sync_ipc::write_rva(pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_mdl",
        lua.create_function(
            |_, (pid, address, size): (u64, Address, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL read size"));
                }
                crate::sync_ipc::read_mdl(pid, address.get(), size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_mdl",
        lua.create_function(
            |_, (pid, address, data): (u64, Address, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL write size"));
                }
                crate::sync_ipc::write_mdl(pid, address.get(), &data).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "read_mdl_rva",
        lua.create_function(
            |_, (pid, relative_address, size): (u64, u64, u64)| -> mlua::Result<Vec<u8>> {
                let size = usize::try_from(size)
                    .map_err(|_| mlua::Error::runtime("size does not fit usize"))?;
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL RVA read size"));
                }
                crate::sync_ipc::read_mdl_rva(pid, relative_address, size as u64)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "write_mdl_rva",
        lua.create_function(
            |_, (pid, relative_address, data): (u64, u64, Vec<u8>)| -> mlua::Result<()> {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid MDL RVA write size"));
                }
                crate::sync_ipc::write_mdl_rva(pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "batch_read",
        lua.create_function(
            |_, (pid, size, addresses_table): (u64, u32, mlua::Table)| -> mlua::Result<Vec<u8>> {
                if size == 0 || size > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE as u32 {
                    return Err(mlua::Error::runtime("invalid batch entry size"));
                }
                let count = addresses_table.len()? as usize;
                if count > ks_core::protocol::MAX_BATCH_ENTRIES {
                    return Err(mlua::Error::runtime("too many batch entries"));
                }
                let mut addresses = Vec::with_capacity(count);
                for i in 1..=count {
                    let addr: u64 = addresses_table.get(i)?;
                    addresses.push(addr);
                }
                crate::sync_ipc::batch_read(pid, size, &addresses).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    // memory.batch_write(pid, writes) -> {bool, ...}
    // `writes` is an array of {address, data} tables; `data` is a byte
    // table. All entries are applied in a single ring round trip and a
    // single kernel transition; the returned table holds one success flag
    // per entry, in input order.
    module.set(
        "batch_write",
        lua.create_function(
            |lua, (pid, writes_table): (u64, mlua::Table)| -> mlua::Result<mlua::Table> {
                let count = writes_table.len()? as usize;
                if count == 0 || count > ks_core::protocol::MAX_BATCH_WRITE_ENTRIES {
                    return Err(mlua::Error::runtime("batch_write expects 1-64 entries"));
                }
                let mut entries = Vec::with_capacity(count);
                for i in 1..=count {
                    let item: mlua::Table = writes_table.get(i)?;
                    let address: Address = item.get("address")?;
                    let data: Vec<u8> = item.get("data")?;
                    if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                        return Err(mlua::Error::runtime(format!(
                            "invalid batch_write data size at entry {i}"
                        )));
                    }
                    entries.push((address.get(), data));
                }
                let flags =
                    crate::sync_ipc::batch_write(pid, &entries).map_err(mlua::Error::runtime)?;
                let result = lua.create_table()?;
                for (i, ok) in flags.iter().enumerate() {
                    result.set(i + 1, *ok)?;
                }
                Ok(result)
            },
        )?,
    )?;

    module.set(
        "batch_offset",
        lua.create_function(|lua, sizes: mlua::Table| -> mlua::Result<mlua::Table> {
            let count = sizes.len()? as usize;
            let offsets = lua.create_table()?;
            let mut acc = 0u64;
            for i in 1..=count {
                offsets.set(i, acc)?;
                let size: u64 = sizes.get(i)?;
                acc += size;
            }
            offsets.set("total", acc)?;
            Ok(offsets)
        })?,
    )?;

    module.set(
        "traverse_pointer_chain",
        lua.create_function(
            |_, (pid, base, offsets_table): (u64, u64, mlua::Table)| -> mlua::Result<u64> {
                let count = offsets_table.len()? as usize;
                if count > 32 {
                    return Err(mlua::Error::runtime("pointer chain: max 32 offsets"));
                }
                let mut offsets = Vec::with_capacity(count);
                for i in 1..=count {
                    let offset: u64 = offsets_table.get(i)?;
                    offsets.push(offset);
                }
                crate::sync_ipc::traverse_pointer_chain(pid, base, &offsets)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    // memory.alloc(pid, size [, protect]) -> address
    // Reserves, commits and zeroes `size` bytes in the target and returns
    // the base address. `protect` defaults to PAGE_READWRITE (0x04); the
    // only executable option is PAGE_EXECUTE_READWRITE (0x40). The driver
    // rounds the size up to whole pages; `memory.free` releases it.
    module.set(
        "alloc",
        lua.create_function(
            |_, (pid, size, protect): (u64, u64, Option<u32>)| -> mlua::Result<u64> {
                let protect = protect.unwrap_or(ks_core::protocol::ALLOC_PROTECT_READWRITE);
                if !ks_core::protocol::valid_alloc_protect(protect) {
                    return Err(mlua::Error::runtime("invalid allocation protection"));
                }
                if size == 0 || size > ks_core::protocol::MAX_ALLOC_SIZE as u64 {
                    return Err(mlua::Error::runtime("invalid allocation size"));
                }
                crate::sync_ipc::alloc(pid, size, protect).map_err(mlua::Error::runtime)
            },
        )?,
    )?;

    module.set(
        "free",
        lua.create_function(|_, (pid, address): (u64, Address)| -> mlua::Result<()> {
            crate::sync_ipc::free(pid, address.get()).map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "lock",
        lua.create_function(
            |_, (id, pid, address, data): (u64, u64, Address, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid lock size"));
                }
                crate::sync_ipc::lock(id, pid, address.get(), &data).map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    module.set(
        "unlock",
        lua.create_function(|_, id: u64| {
            crate::sync_ipc::unlock(id).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "unlock_all",
        lua.create_function(|_, pid: u64| {
            crate::sync_ipc::unlock_all(pid).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "lock_rva",
        lua.create_function(
            |_, (id, pid, relative_address, data): (u64, u64, u64, Vec<u8>)| {
                if data.is_empty() || data.len() > ks_core::protocol::MAX_DRIVER_TRANSFER_SIZE {
                    return Err(mlua::Error::runtime("invalid lock size"));
                }
                crate::sync_ipc::lock_rva(id, pid, relative_address, &data)
                    .map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    module.set(
        "unlock_rva",
        lua.create_function(|_, id: u64| {
            crate::sync_ipc::unlock_rva(id).map_err(mlua::Error::runtime)
        })?,
    )?;
    // memory.lock_list() -> { {id, pid, address, len}, ... }
    // Every lock the driver is replaying, including locks created by other
    // clients (a Rust tool, another script).
    module.set(
        "lock_list",
        lua.create_function(|lua, ()| -> mlua::Result<mlua::Table> {
            let entries = crate::sync_ipc::lock_list().map_err(mlua::Error::runtime)?;
            let list = lua.create_table()?;
            for (i, entry) in entries.iter().enumerate() {
                let item = lua.create_table()?;
                item.set("id", entry.id)?;
                item.set("pid", entry.pid)?;
                item.set("address", entry.address)?;
                item.set("len", entry.len)?;
                list.set(i + 1, item)?;
            }
            Ok(list)
        })?,
    )?;
    // memory.protect(pid, address, size, protect [, cfg_no_update]) -> old
    // Changes the page protection of a mapped span; `protect` is one of
    // 0x02/0x04/0x20/0x40. With `cfg_no_update` the region's existing CFG
    // bitmap entries stay as they are. Returns the first page's previous
    // protection.
    module.set(
        "protect",
        lua.create_function(
            |_,
             (pid, address, size, protect, cfg_no_update): (
                u64,
                Address,
                u64,
                u32,
                Option<bool>,
            )| {
                let address = address.get();
                let result = match cfg_no_update {
                    None => crate::sync_ipc::protect(pid, address, size as usize, protect),
                    Some(flag) => {
                        crate::sync_ipc::protect_ex(pid, address, size as usize, protect, flag)
                    }
                };
                result.map_err(mlua::Error::runtime)
            },
        )?,
    )?;
    // memory.protect_span(pid, address, size, protect [, cfg_no_update])
    //   -> old_protect, bytes_protected
    // Same as `protect` but also returns the page-rounded length the
    // kernel covered (`protect(pid, addr, 1, …)` changes a whole page).
    module.set(
        "protect_span",
        lua.create_function(
            |_,
             (pid, address, size, protect, cfg_no_update): (
                u64,
                Address,
                u64,
                u32,
                Option<bool>,
            )| {
                let (old, len) = crate::sync_ipc::protect_span(
                    pid,
                    address.get(),
                    size as usize,
                    protect,
                    cfg_no_update.unwrap_or(false),
                )
                .map_err(mlua::Error::runtime)?;
                Ok((old, len as u64))
            },
        )?,
    )?;
    // memory.query_region(pid, address)
    //   -> { base, allocation_base, region_size, state, protect,
    //        allocation_protect, typ }
    // The probe step before `protect`: the span must sit inside one
    // region, and `state`/`typ` say whether the pages may change at all
    // (the numeric fields use the MEM_* / PROT_* constants).
    module.set(
        "query_region",
        lua.create_function(
            |lua, (pid, address): (u64, Address)| -> mlua::Result<mlua::Table> {
                let info = crate::sync_ipc::query_region(pid, address.get())
                    .map_err(mlua::Error::runtime)?;
                let report = lua.create_table()?;
                report.set("base", info.base)?;
                report.set("allocation_base", info.allocation_base)?;
                report.set("region_size", info.region_size)?;
                report.set("state", info.state)?;
                report.set("protect", info.protect)?;
                report.set("allocation_protect", info.allocation_protect)?;
                report.set("typ", info.typ)?;
                Ok(report)
            },
        )?,
    )?;
    module.set(
        "suspend",
        lua.create_function(|_, pid: u64| {
            crate::sync_ipc::suspend(pid).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "resume",
        lua.create_function(|_, pid: u64| {
            crate::sync_ipc::resume(pid).map_err(mlua::Error::runtime)
        })?,
    )?;

    // Hook API, mirroring the SDK surface. `pid = 0` selects the kernel
    // half, anything else that process's user half; addresses accept a
    // number or a `0x` string. Address fields in the returned tables are
    // hex strings so kernel addresses survive Lua's f64 numbers exactly.
    //
    // memory.hook_probe(pid, target)
    //   -> { replaced_len, absolute_len, original, rip_relative,
    //        relative_branch }
    module.set(
        "hook_probe",
        lua.create_function(
            |lua, (pid, target): (u64, Address)| -> mlua::Result<mlua::Table> {
                let probe =
                    crate::sync_ipc::hook_probe(pid, target.get()).map_err(mlua::Error::runtime)?;
                let report = lua.create_table()?;
                report.set("replaced_len", probe.replaced_len)?;
                report.set("absolute_len", probe.absolute_len)?;
                report.set("original", probe.original)?;
                report.set("rip_relative", probe.rip_relative)?;
                report.set("relative_branch", probe.relative_branch)?;
                Ok(report)
            },
        )?,
    )?;
    // memory.hook_create(pid, target, trampoline)
    //   -> { id, replaced_len, original }
    module.set(
        "hook_create",
        lua.create_function(
            |lua,
             (pid, target, trampoline): (u64, Address, Address)|
             -> mlua::Result<mlua::Table> {
                let hook = crate::sync_ipc::hook_create(pid, target.get(), trampoline.get())
                    .map_err(mlua::Error::runtime)?;
                let entry = lua.create_table()?;
                entry.set("id", hook.id)?;
                entry.set("replaced_len", hook.replaced_len)?;
                entry.set("original", hook.original)?;
                Ok(entry)
            },
        )?,
    )?;
    // memory.hook_shellcode(pid, target, code)
    //   -> { id, replaced_len, original, code_address }
    module.set(
        "hook_shellcode",
        lua.create_function(
            |lua, (pid, target, code): (u64, Address, Vec<u8>)| -> mlua::Result<mlua::Table> {
                let hook = crate::sync_ipc::hook_shellcode(pid, target.get(), &code)
                    .map_err(mlua::Error::runtime)?;
                let entry = lua.create_table()?;
                entry.set("id", hook.id)?;
                entry.set("replaced_len", hook.replaced_len)?;
                entry.set("original", hook.original)?;
                entry.set("code_address", format!("0x{:X}", hook.code_address))?;
                Ok(entry)
            },
        )?,
    )?;
    // memory.hook_detour(pid, target, code)
    //   -> { id, replaced_len, original, code_address, trampoline_address }
    module.set(
        "hook_detour",
        lua.create_function(
            |lua, (pid, target, code): (u64, Address, Vec<u8>)| -> mlua::Result<mlua::Table> {
                let hook = crate::sync_ipc::hook_detour(pid, target.get(), &code)
                    .map_err(mlua::Error::runtime)?;
                let entry = lua.create_table()?;
                entry.set("id", hook.id)?;
                entry.set("replaced_len", hook.replaced_len)?;
                entry.set("original", hook.original)?;
                entry.set("code_address", format!("0x{:X}", hook.code_address))?;
                entry.set(
                    "trampoline_address",
                    format!("0x{:X}", hook.trampoline_address),
                )?;
                Ok(entry)
            },
        )?,
    )?;
    module.set(
        "hook_enable",
        lua.create_function(|_, (pid, id): (u64, u64)| {
            crate::sync_ipc::hook_enable(pid, id).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "hook_disable",
        lua.create_function(|_, (pid, id): (u64, u64)| {
            crate::sync_ipc::hook_disable(pid, id).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "hook_remove",
        lua.create_function(|_, (pid, id): (u64, u64)| {
            crate::sync_ipc::hook_remove(pid, id).map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "hook_enable_all",
        lua.create_function(|_, ()| {
            crate::sync_ipc::hook_enable_all().map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "hook_disable_all",
        lua.create_function(|_, ()| {
            crate::sync_ipc::hook_disable_all().map_err(mlua::Error::runtime)
        })?,
    )?;
    module.set(
        "hook_remove_all",
        lua.create_function(|_, ()| {
            crate::sync_ipc::hook_remove_all().map_err(mlua::Error::runtime)
        })?,
    )?;

    module.set(
        "get_window_rect",
        lua.create_function(|lua, pid: u64| -> mlua::Result<Option<mlua::Table>> {
            let pid32 = u32::try_from(pid).map_err(|_| mlua::Error::runtime("PID too large"))?;
            let rects = crate::window_util::get_window_rects_by_pid(pid32);
            if rects.is_empty() {
                return Ok(None);
            }
            let scale = CONTENT_SCALE.load(Ordering::Relaxed) as f32 / 100.0;
            let list = lua.create_table()?;
            for (i, r) in rects.into_iter().enumerate() {
                let t = lua.create_table()?;
                t.set("x", r.x as f32 / scale)?;
                t.set("y", r.y as f32 / scale)?;
                t.set("width", r.width as f32 / scale)?;
                t.set("height", r.height as f32 / scale)?;
                list.set(i + 1, t)?;
            }
            Ok(Some(list))
        })?,
    )?;

    lua.globals().set("memory", module)
}

#[cfg(test)]
mod tests {
    use super::register_memory_api;

    /// The whole Lua-facing surface must exist on the `memory` table; the
    /// functions themselves need a live driver and are exercised by
    /// `ks-test`/`ks-hook-test` instead.
    #[test]
    fn registers_driver_round_trip_surface() {
        let lua = mlua::Lua::new();
        register_memory_api(&lua).unwrap();
        let memory: mlua::Table = lua.globals().get("memory").unwrap();
        for name in [
            "get_pid",
            "get_process_base",
            "read_i32",
            "read_bytes",
            "write_i32",
            "write_bytes",
            "read_rva",
            "write_rva",
            "read_mdl",
            "write_mdl",
            "read_mdl_rva",
            "write_mdl_rva",
            "batch_read",
            "batch_write",
            "batch_offset",
            "traverse_pointer_chain",
            "alloc",
            "free",
            "lock",
            "unlock",
            "unlock_all",
            "lock_rva",
            "unlock_rva",
            "lock_list",
            "protect",
            "protect_span",
            "query_region",
            "suspend",
            "resume",
            "hook_probe",
            "hook_create",
            "hook_shellcode",
            "hook_detour",
            "hook_enable",
            "hook_disable",
            "hook_remove",
            "hook_enable_all",
            "hook_disable_all",
            "hook_remove_all",
            "get_window_rect",
        ] {
            let value: mlua::Value = memory.get(name).expect(name);
            assert!(value.is_function(), "{name} must be a function");
        }
    }
}
