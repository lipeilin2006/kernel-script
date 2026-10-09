//! The `config` module: typed scalar entries through the shared config
//! store (`config.json`, debounced flush).

use std::sync::Arc;

use mlua::{Lua, Value};

use crate::config_store::{ConfigValue, SharedConfigStore};

pub fn register_config_api(lua: &Lua, config: SharedConfigStore) -> mlua::Result<()> {
    let module = lua.create_table()?;

    // config.set(key, value) -> bool
    // Accepts boolean, integer, number or string values. Returns false for
    // unsupported types or oversized keys/values; the entry is only queued in
    // memory and written to config.json by the debounced flush.
    {
        let config = Arc::clone(&config);
        module.set(
            "set",
            lua.create_function(move |_, (key, value): (String, Value)| {
                let Some(entry) = config_value_from_lua(&value) else {
                    return Ok(false);
                };
                Ok(config.lock().unwrap().set(&key, entry))
            })?,
        )?;
    }

    // config.get(key, default?) -> value
    // Returns the stored entry, or `default` (nil when omitted) when missing.
    {
        let config = Arc::clone(&config);
        module.set(
            "get",
            lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
                let stored = config
                    .lock()
                    .unwrap()
                    .get(&key)
                    .map(|entry| config_value_into_lua(lua, entry));
                Ok(stored.unwrap_or_else(|| default.unwrap_or(Value::Nil)))
            })?,
        )?;
    }

    // config.remove(key) -> bool
    {
        let config = Arc::clone(&config);
        module.set(
            "remove",
            lua.create_function(move |_, key: String| Ok(config.lock().unwrap().remove(&key)))?,
        )?;
    }

    // config.save() -> bool
    // Forces an immediate write of the pending changes to config.json.
    {
        let config = Arc::clone(&config);
        module.set(
            "save",
            lua.create_function(move |_, ()| {
                let mut store = config.lock().unwrap();
                store.flush();
                Ok(!store.is_dirty())
            })?,
        )?;
    }

    lua.globals().set("config", module)
}

/// Converts a Lua value into a storable config entry. Only JSON-mappable
/// scalars are accepted; tables, functions and userdata are rejected.
fn config_value_from_lua(value: &Value) -> Option<ConfigValue> {
    match value {
        Value::Boolean(flag) => Some(ConfigValue::Bool(*flag)),
        Value::Integer(int) => Some(ConfigValue::Int(*int)),
        Value::Number(number) => Some(ConfigValue::Float(*number)),
        Value::String(text) => text
            .to_str()
            .ok()
            .map(|text| ConfigValue::Str(text.to_owned())),
        _ => None,
    }
}

fn config_value_into_lua(lua: &Lua, value: ConfigValue) -> Value {
    match value {
        ConfigValue::Bool(flag) => Value::Boolean(flag),
        ConfigValue::Int(int) => Value::Integer(int),
        // Non-finite floats cannot be represented in JSON; surface nil instead
        // of letting the flush fail on serialization.
        ConfigValue::Float(float) if float.is_finite() => Value::Number(float),
        ConfigValue::Float(_) => Value::Nil,
        ConfigValue::Str(text) => match lua.create_string(text.as_bytes()) {
            Ok(string) => Value::String(string),
            Err(_) => Value::Nil,
        },
    }
}
