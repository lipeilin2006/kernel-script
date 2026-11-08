//! 本地 vendor（源：crates.io egui_overlay 0.9.0）。
//!
//! 本地修改（相对上游）：
//! - 新增 [`OVERLAY_MAX_HEIGHT_MARGIN`] + [`cap_overlay_height`]：覆盖层窗口
//!   最大高度 = 主屏高度 - 10（物理像素），[`start`] 内置调用；ks-td2 的
//!   创建 / sync_overlay_pos 两条路径也调用同一函数（单一事实来源）。
//! - 2026-10-13 全 DX11 迁移：上游 `start` / [`EguiOverlay`] / `OverlayApp`
//!   及 three_d/wgpu 后端选择整体删除（ks-td2 只用 GlfwBackend +
//!   cap_overlay_height，渲染由 egui_render_d3d11 直连）。窗口层
//!   egui_window_glfw_passthrough 仍经此再导出。
pub use egui_window_glfw_passthrough;
use egui_window_glfw_passthrough::GlfwBackend;

/// 覆盖层窗口最大高度边距（物理像素）：窗口高度不得超过主屏高度 - 10。
pub const OVERLAY_MAX_HEIGHT_MARGIN: i32 = 10;

/// 将覆盖层窗口高度限制在 主屏高度 - [`OVERLAY_MAX_HEIGHT_MARGIN`] 以内。
///
/// 每次调用都实时查询主屏 video mode（分辨率切换后仍正确）；查询失败时
/// 不做修改。返回是否实际收窄了高度。
pub fn cap_overlay_height(backend: &mut GlfwBackend) -> bool {
    let mut screen_h = 0i32;
    backend.glfw.with_primary_monitor(|_, monitor| {
        if let Some(mode) = monitor.and_then(|m| m.get_video_mode()) {
            screen_h = mode.height as i32;
        }
    });
    if screen_h <= 0 {
        return false;
    }
    let max_h = (screen_h - OVERLAY_MAX_HEIGHT_MARGIN).max(1);
    let (w, h) = backend.window.get_size();
    if h > max_h {
        backend.window.set_size(w, max_h);
        return true;
    }
    false
}
