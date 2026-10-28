//! UI language: Simplified Chinese (default) or English.
//!
//! Both tables are `Strings` structs, so a key that exists in one language
//! must exist in the other — a missing translation is a compile error, not
//! a blank label. The choice is process-wide (one atomic: the GUI renders
//! on a single thread and the worker only reads it), toggled from the
//! top-right corner of the memory tool, and persisted in the Lua config
//! store as `gui.language` (`"zh"` / `"en"`).

use std::sync::atomic::{AtomicU8, Ordering};

use crate::config_store::{ConfigValue, SharedConfigStore};

/// Config-store key holding the selected language code.
pub const CONFIG_KEY: &str = "gui.language";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lang {
    Zh,
    En,
}

const ZH_TAG: u8 = 0;
const EN_TAG: u8 = 1;

static CURRENT: AtomicU8 = AtomicU8::new(ZH_TAG);

/// The active language.
pub fn lang() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == EN_TAG {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// The string table for the active language.
pub fn s() -> &'static Strings {
    match lang() {
        Lang::Zh => &ZH,
        Lang::En => &EN,
    }
}

/// The persisted code for `lang` (`"zh"` / `"en"`).
pub fn code(lang: Lang) -> &'static str {
    match lang {
        Lang::Zh => "zh",
        Lang::En => "en",
    }
}

/// Switches the language without touching the config store.
pub fn set_lang(lang: Lang) {
    CURRENT.store(
        match lang {
            Lang::Zh => ZH_TAG,
            Lang::En => EN_TAG,
        },
        Ordering::Relaxed,
    );
}

/// Switches the language and persists the choice; a store that cannot be
/// locked still gets the in-memory switch.
pub fn set_lang_persist(lang: Lang, config: &SharedConfigStore) {
    set_lang(lang);
    if let Ok(mut store) = config.lock() {
        store.set(CONFIG_KEY, ConfigValue::Str(code(lang).to_owned()));
        store.flush();
    }
}

/// Restores the persisted language, if one is recorded. Unknown values
/// leave the default (Chinese) in place.
pub fn restore(config: &SharedConfigStore) {
    let Ok(store) = config.lock() else {
        return;
    };
    if let Some(ConfigValue::Str(code)) = store.get(CONFIG_KEY) {
        match code.as_str() {
            "en" => set_lang(Lang::En),
            "zh" => set_lang(Lang::Zh),
            _ => {}
        }
    }
}

/// Fills the `{}` placeholders of a table string, in order. `format!`
/// requires a literal format string, and the two languages may order the
/// parts differently, so the holes are positional.
pub fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(template.len() + 16);
    let mut parts = template.split("{}");
    if let Some(head) = parts.next() {
        out.push_str(head);
    }
    for (arg, tail) in args.iter().zip(parts) {
        let _ = write!(out, "{arg}");
        out.push_str(tail);
    }
    out
}

/// Draws the 中文 / EN toggle. Add it inside a `right_to_left` layout to
/// pin the pair to the top-right corner; `EN` ends up rightmost.
pub fn toggle_widget(ui: &mut egui::Ui, config: &SharedConfigStore) {
    let current = lang();
    if ui.selectable_label(current == Lang::En, "EN").clicked() && current != Lang::En {
        set_lang_persist(Lang::En, config);
    }
    if ui.selectable_label(current == Lang::Zh, "中文").clicked() && current != Lang::Zh {
        set_lang_persist(Lang::Zh, config);
    }
}

/// Every UI string in one language.
pub struct Strings {
    // Window chrome and the overlay fallback window.
    pub app_title: &'static str,
    pub overlay_title: &'static str,
    pub reopen_tool: &'static str,
    pub quit: &'static str,
    // Memory-tool chrome.
    pub log: &'static str,
    pub process: &'static str,
    pub pid: &'static str,
    pub filter_hint: &'static str,
    pub refresh: &'static str,
    pub close: &'static str,
    pub tab_hex: &'static str,
    pub tab_editor: &'static str,
    pub tab_modify: &'static str,
    // Hex tab.
    pub hex_start: &'static str,
    pub hex_no_base: &'static str,
    pub length: &'static str,
    /// `{}` is the byte count.
    pub bytes_fmt: &'static str,
    pub prev_page: &'static str,
    pub next_page: &'static str,
    /// `{}` is the 1-based page number.
    pub page_fmt: &'static str,
    pub reset_base: &'static str,
    pub hex_pick_process: &'static str,
    pub reading: &'static str,
    /// `{}` is the driver error.
    pub read_failed_fmt: &'static str,
    // Editor tab.
    pub script_dir: &'static str,
    pub reload: &'static str,
    pub reloaded: &'static str,
    pub new_file_hint: &'static str,
    pub new_file: &'static str,
    pub save: &'static str,
    pub unsaved: &'static str,
    /// `{}` = script count, second `{}` = directory.
    pub scanned_fmt: &'static str,
    /// `{}` is the saved file name.
    pub saved_fmt: &'static str,
    /// `{}` is the OS error.
    pub save_failed_fmt: &'static str,
    pub invalid_name: &'static str,
    pub file_exists: &'static str,
    /// `{}` is the new file name.
    pub created_fmt: &'static str,
    /// `{}` is the OS error.
    pub create_failed_fmt: &'static str,
    // Modify tab.
    /// `{}` is the lock-count suffix (or empty).
    pub lock_table_fmt: &'static str,
    pub unlock_all: &'static str,
    pub lock_hint: &'static str,
    pub no_locks: &'static str,
    pub lock_id: &'static str,
    pub address: &'static str,
    pub source: &'static str,
    pub this_window: &'static str,
    pub lua_other: &'static str,
    pub unlock: &'static str,
    pub add: &'static str,
    pub refresh_values: &'static str,
    pub write_changes: &'static str,
    pub unfreeze_all: &'static str,
    pub col_freeze: &'static str,
    pub col_type: &'static str,
    pub col_value: &'static str,
    pub no_process: &'static str,
    pub delete: &'static str,
    // Value types.
    pub type_i8: &'static str,
    pub type_i16: &'static str,
    pub type_i32: &'static str,
    pub type_i64: &'static str,
    pub type_u32: &'static str,
    pub type_f32: &'static str,
    pub type_f64: &'static str,
    // Transient statuses (set when an event arrives).
    pub status_initial: &'static str,
    pub written: &'static str,
    pub freezing: &'static str,
    pub unfrozen: &'static str,
    pub all_unfrozen: &'static str,
    pub unlocked: &'static str,
    /// `{}` is the process count.
    pub process_list_fmt: &'static str,
    /// `{}` is the driver error.
    pub base_failed_fmt: &'static str,
    /// `{}` is the lock count.
    pub entries_fmt: &'static str,
}

static ZH: Strings = Strings {
    app_title: "KernelScript - 内存工具",
    overlay_title: "内存工具",
    reopen_tool: "重新打开内存工具",
    quit: "退出程序",
    log: "日志",
    process: "进程",
    pid: "进程",
    filter_hint: "过滤",
    refresh: "刷新",
    close: "关闭",
    tab_hex: "内存查看 (Ctrl+1)",
    tab_editor: "Lua 脚本 (Ctrl+2)",
    tab_modify: "修改 (Ctrl+3)",
    hex_start: "起始",
    hex_no_base: "进程基址（未选择进程）",
    length: "长度",
    bytes_fmt: "{} 字节",
    prev_page: "◀ 上一页",
    next_page: "下一页 ▶",
    page_fmt: "第 {} 页",
    reset_base: "回到基址",
    hex_pick_process: "从左侧选择一个进程，十六进制视图会以它的模块基址为起点。",
    reading: "读取中...",
    read_failed_fmt: "读取失败: {}",
    script_dir: "脚本目录",
    reload: "重新加载",
    reloaded: "已从磁盘重新加载",
    new_file_hint: "新文件名",
    new_file: "新建",
    save: "保存 (Ctrl+S)",
    unsaved: "● 未保存",
    scanned_fmt: "{} 个脚本 · {}",
    saved_fmt: "已保存 {}（覆盖层会自动热重载）",
    save_failed_fmt: "保存失败: {}",
    invalid_name: "文件名无效",
    file_exists: "文件已存在",
    created_fmt: "已创建 {}",
    create_failed_fmt: "创建失败: {}",
    lock_table_fmt: "驱动锁表（含 Lua）{}",
    unlock_all: "全部解锁",
    lock_hint: "本窗口的锁也能在下面的地址列表里编辑",
    no_locks: "当前没有活动的锁",
    lock_id: "锁 ID",
    address: "地址",
    source: "来源",
    this_window: "本窗口",
    lua_other: "Lua/其他",
    unlock: "解锁",
    add: "添加",
    refresh_values: "刷新数值",
    write_changes: "写入修改",
    unfreeze_all: "解除全部冻结",
    col_freeze: "冻结",
    col_type: "类型",
    col_value: "数值",
    no_process: "未选择进程",
    delete: "删",
    type_i8: "1 字节",
    type_i16: "2 字节",
    type_i32: "4 字节",
    type_i64: "8 字节",
    type_u32: "4 字节 (无符号)",
    type_f32: "浮点",
    type_f64: "双精度",
    status_initial: "选择左侧进程，查看它的模块内存",
    written: "已写入",
    freezing: "冻结中",
    unfrozen: "已解冻",
    all_unfrozen: "已解除全部冻结",
    unlocked: "已解锁",
    process_list_fmt: "进程列表: {} 项",
    base_failed_fmt: "模块基址读取失败: {}",
    entries_fmt: "{} 项",
};

static EN: Strings = Strings {
    app_title: "KernelScript - Memory Tool",
    overlay_title: "Memory Tool",
    reopen_tool: "Reopen Memory Tool",
    quit: "Quit",
    log: "Log",
    process: "Process",
    pid: "PID",
    filter_hint: "filter",
    refresh: "Refresh",
    close: "Close",
    tab_hex: "Memory (Ctrl+1)",
    tab_editor: "Lua Scripts (Ctrl+2)",
    tab_modify: "Modify (Ctrl+3)",
    hex_start: "Base",
    hex_no_base: "process base (no process selected)",
    length: "Length",
    bytes_fmt: "{} B",
    prev_page: "◀ Prev",
    next_page: "Next ▶",
    page_fmt: "Page {}",
    reset_base: "Reset",
    hex_pick_process: "Pick a process on the left; the hex view starts at its module base.",
    reading: "Reading...",
    read_failed_fmt: "read failed: {}",
    script_dir: "Script dir",
    reload: "Reload",
    reloaded: "Reloaded from disk",
    new_file_hint: "new file name",
    new_file: "New",
    save: "Save (Ctrl+S)",
    unsaved: "● unsaved",
    scanned_fmt: "{} scripts · {}",
    saved_fmt: "Saved {} (the overlay hot-reloads it)",
    save_failed_fmt: "save failed: {}",
    invalid_name: "invalid file name",
    file_exists: "file already exists",
    created_fmt: "created {}",
    create_failed_fmt: "create failed: {}",
    lock_table_fmt: "Driver lock table (incl. Lua){}",
    unlock_all: "Unlock all",
    lock_hint: "Locks from this window stay editable in the list below",
    no_locks: "No active locks",
    lock_id: "Lock ID",
    address: "Address",
    source: "Source",
    this_window: "this window",
    lua_other: "Lua/other",
    unlock: "Unlock",
    add: "Add",
    refresh_values: "Refresh",
    write_changes: "Write",
    unfreeze_all: "Unfreeze all",
    col_freeze: "Freeze",
    col_type: "Type",
    col_value: "Value",
    no_process: "no process selected",
    delete: "Del",
    type_i8: "1 byte",
    type_i16: "2 bytes",
    type_i32: "4 bytes",
    type_i64: "8 bytes",
    type_u32: "4 bytes (unsigned)",
    type_f32: "float",
    type_f64: "double",
    status_initial: "Pick a process on the left to browse its module memory",
    written: "written",
    freezing: "freezing",
    unfrozen: "unfrozen",
    all_unfrozen: "all unfrozen",
    unlocked: "unlocked",
    process_list_fmt: "processes: {}",
    base_failed_fmt: "module base failed: {}",
    entries_fmt: "{} entries",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_switches_tables() {
        set_lang(Lang::Zh);
        assert_eq!(s().refresh, "刷新");
        set_lang(Lang::En);
        assert_eq!(s().refresh, "Refresh");
        set_lang(Lang::Zh);
        assert_eq!(s().refresh, "刷新");
    }

    #[test]
    fn english_table_has_no_cjk() {
        let en = &EN;
        let all = [
            en.app_title,
            en.overlay_title,
            en.reopen_tool,
            en.quit,
            en.log,
            en.process,
            en.pid,
            en.filter_hint,
            en.refresh,
            en.close,
            en.tab_hex,
            en.tab_editor,
            en.tab_modify,
            en.hex_start,
            en.hex_no_base,
            en.length,
            en.bytes_fmt,
            en.prev_page,
            en.next_page,
            en.page_fmt,
            en.reset_base,
            en.hex_pick_process,
            en.reading,
            en.read_failed_fmt,
            en.script_dir,
            en.reload,
            en.reloaded,
            en.new_file_hint,
            en.new_file,
            en.save,
            en.unsaved,
            en.scanned_fmt,
            en.saved_fmt,
            en.save_failed_fmt,
            en.invalid_name,
            en.file_exists,
            en.created_fmt,
            en.create_failed_fmt,
            en.lock_table_fmt,
            en.unlock_all,
            en.lock_hint,
            en.no_locks,
            en.lock_id,
            en.address,
            en.source,
            en.this_window,
            en.lua_other,
            en.unlock,
            en.add,
            en.refresh_values,
            en.write_changes,
            en.unfreeze_all,
            en.col_freeze,
            en.col_type,
            en.col_value,
            en.no_process,
            en.delete,
            en.type_i8,
            en.type_i16,
            en.type_i32,
            en.type_i64,
            en.type_u32,
            en.type_f32,
            en.type_f64,
            en.status_initial,
            en.written,
            en.freezing,
            en.unfrozen,
            en.all_unfrozen,
            en.unlocked,
            en.process_list_fmt,
            en.base_failed_fmt,
            en.entries_fmt,
        ];
        for text in all {
            assert!(
                !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{text:?} still contains CJK"
            );
        }
    }

    #[test]
    fn codes_round_trip() {
        assert_eq!(code(Lang::Zh), "zh");
        assert_eq!(code(Lang::En), "en");
    }
}
