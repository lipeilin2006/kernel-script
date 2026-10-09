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
