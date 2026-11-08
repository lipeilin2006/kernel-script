//! Overlay window host: window creation, D3D11 presentation, pointer-region
//! passthrough and the 100 Hz display loop.
//!
//! This mirrors the ks-td2 overlay runtime: a GLFW NoApi window (events and
//! click-through only) presented through a D3D11 swap chain with DWM
//! blur-behind transparency. The whole overlay repaints unconditionally at
//! the display cadence; a slow Lua frame slows the whole loop instead of
//! dropping the panel or painting a stale command list.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui_overlay::egui_window_glfw_passthrough::{
    self as glfw_passthrough, GlfwBackend, GlfwConfig,
};
use egui_render_d3d11::D3D11Backend;

use super::{KernelScriptApp, FRAME_INTERVAL, UI_SCALE};

/// Transparent fullscreen overlay window + its D3D11 renderer.
pub(super) fn create_overlay_window() -> (GlfwBackend, D3D11Backend) {
    let monitor_size = Rc::new(Cell::new([1920u32, 1080u32]));
    let monitor_size_clone = monitor_size.clone();

    let mut overlay = GlfwBackend::new(GlfwConfig {
        // The GLFW transparent hint provides the DWM blur-behind surface
        // that makes a NoApi window alpha-composited; no GL context exists.
        transparent_window: Some(true),
        opengl_window: Some(false),
        window_title: "Kernel Script Overlay".into(),
        glfw_callback: Box::new(move |gtx| {
            (glfw_passthrough::GlfwConfig::default().glfw_callback)(gtx);
            gtx.window_hint(glfw_passthrough::glfw::WindowHint::ScaleToMonitor(true));
            // Hidden until the first overlay frame is painted (no white
            // flash while the driver probe and Lua start-up run).
            gtx.window_hint(glfw_passthrough::glfw::WindowHint::Visible(false));
            gtx.with_primary_monitor(|_, monitor| {
                if let Some(monitor) = monitor {
                    if let Some(video_mode) = monitor.get_video_mode() {
                        monitor_size_clone.set([video_mode.width, video_mode.height]);
                    }
                }
            });
        }),
        window_callback: Box::new(|window: &mut glfw_passthrough::glfw::Window| {
            window.set_floating(true);
            window.set_decorated(false);
            // Start click-through: the pointer-region check below switches
            // back to interactive when the cursor is over a panel.
            window.set_mouse_passthrough(true);
        }),
        ..Default::default()
    });

    let [width, height] = monitor_size.get();
    overlay.window.set_size(width as i32, height as i32);
    // Same single source of truth as ks-td2: the overlay is never taller
    // than the primary screen minus 10 physical pixels.
    egui_overlay::cap_overlay_height(&mut overlay);
    overlay.window.set_pos(0, 0);

    {
        let (scale, _) = overlay.window.get_content_scale();
        crate::lua_runtime::set_content_scale(scale);
        tracing::info!(content_scale = scale, "overlay content scale set");
    }

    // Glass recipe: GLFW's blur-behind plus ExtendFrameIntoClientArea. The
    // D3D11 backend repeats the blur-behind internally; that is harmless.
    unsafe {
        use glfw_passthrough::glfw::ffi::glfwGetWin32Window;
        let hwnd = glfwGetWin32Window(&mut overlay.window as *mut _ as *mut _);
        super::win32::enable_dwm_transparency(hwnd as *mut _);
    }

    let framebuffer = overlay.window.get_framebuffer_size();
    let mut gfx = D3D11Backend::new(
        overlay.window.get_win32_window(),
        [framebuffer.0.max(1) as u32, framebuffer.1.max(1) as u32],
    );
    gfx.set_clear_color([0.0, 0.0, 0.0, 0.0]);
    tracing::info!("overlay: d3d11 + DWM blur-behind backend");
    (overlay, gfx)
}

/// Presents one egui pass. Kept byte-for-byte in step with ks-td2's contract:
/// `RawInput` arrives in native logical units, but egui's zoom makes its
/// points space `logical / zoom_factor`; divide input and let the renderer
/// divide by the same zoom.
fn render_pass(
    ctx: &egui::Context,
    gfx: &mut D3D11Backend,
    backend: &mut GlfwBackend,
    ui: impl FnMut(&mut egui::Ui),
) {
    if backend.resized_event_pending {
        let latest = backend.window.get_framebuffer_size();
        gfx.resize_framebuffer([latest.0.max(1) as u32, latest.1.max(1) as u32]);
        backend.resized_event_pending = false;
    }
    let mut input = backend.take_raw_input();
    let zoom_in = ctx.zoom_factor();
    if (zoom_in - 1.0).abs() > 1e-4 {
        let inverse = 1.0 / zoom_in;
        if let Some(rect) = input.screen_rect.as_mut() {
            rect.min = egui::pos2(rect.min.x * inverse, rect.min.y * inverse);
            rect.max = egui::pos2(rect.max.x * inverse, rect.max.y * inverse);
        }
        for event in &mut input.events {
            match event {
                egui::Event::PointerMoved(position) => {
                    *position = egui::pos2(position.x * inverse, position.y * inverse);
                }
                egui::Event::PointerButton { pos, .. } => {
                    *pos = egui::pos2(pos.x * inverse, pos.y * inverse);
                }
                egui::Event::MouseWheel { delta, .. } => {
                    *delta = egui::vec2(delta.x * inverse, delta.y * inverse);
                }
                _ => {}
            }
        }
    }

    let framebuffer = backend.window.get_framebuffer_size();
    gfx.prepare_frame([framebuffer.0.max(1) as u32, framebuffer.1.max(1) as u32]);
    let egui::FullOutput {
        platform_output,
        textures_delta,
        shapes,
        pixels_per_point,
        viewport_output,
        ..
    } = ctx.run_ui(input, ui);
    let _repaint_after = viewport_output
        .into_values()
        .map(|output| output.repaint_delay)
        .min()
        .unwrap_or(Duration::from_millis(10));
    let meshes = ctx.tessellate(shapes, pixels_per_point);
    // Renderer sees the screen size in egui points at the current ppp:
    // fb / zoom = logical * native / (zoom * native) = logical / zoom.
    let zoom_out = (pixels_per_point / backend.scale.max(1e-6)).max(1e-6);
    gfx.render_egui(
        meshes,
        textures_delta,
        [
            backend.window_size_logical[0] / zoom_out,
            backend.window_size_logical[1] / zoom_out,
        ],
    );
    gfx.present();
    for command in &platform_output.commands {
        if let egui::OutputCommand::CopyText(text) = command {
            backend.window.set_clipboard_string(text);
        }
    }
    backend.set_cursor(platform_output.cursor_icon);
}

/// Region-based click-through state.
#[derive(Default)]
struct RegionState {
    /// Union of the egui windows painted last frame (egui points).
    rect: Option<egui::Rect>,
    /// Whether the cursor was inside that union (None = not judged yet).
    inside: Option<bool>,
    /// Consecutive frames with the same inside/outside verdict.
    stable: u32,
}

impl RegionState {
    /// Panel-union rect in physical screen pixels.
    ///
    /// egui points -> framebuffer pixels via `fb / viewport_rect` (the
    /// viewport rect is the zoom-adjusted `RawInput.screen_rect`), then add
    /// the overlay window's physical origin.
    fn screen_rect(
        overlay: &GlfwBackend,
        ctx: &egui::Context,
        rect: egui::Rect,
    ) -> Option<(egui::Pos2, egui::Pos2)> {
        if rect.is_negative() {
            return None;
        }
        let framebuffer = overlay.window.get_framebuffer_size();
        let viewport = ctx.viewport_rect();
        if framebuffer.0 == 0
            || framebuffer.1 == 0
            || viewport.width() <= 0.0
            || viewport.height() <= 0.0
        {
            return None;
        }
        let kx = framebuffer.0 as f32 / viewport.width();
        let ky = framebuffer.1 as f32 / viewport.height();
        let (ox, oy) = overlay.window.get_pos();
        Some((
            egui::pos2(ox as f32 + rect.min.x * kx, oy as f32 + rect.min.y * ky),
            egui::pos2(ox as f32 + rect.max.x * kx, oy as f32 + rect.max.y * ky),
        ))
    }
}

fn any_mouse_button_down() -> bool {
    [0x01u16, 0x02, 0x04, 0x05, 0x06].iter().any(|&vk| unsafe {
        windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) as u16 & 0x8000
            != 0
    })
}

/// Panel rect = interactive, outside = click-through.
///
/// Rules (ported from ks-td2, where the old "any hover means click capture"
/// approach kept stealing game clicks):
/// - keep the current state while any mouse button is held (dragging a
///   slider out of the panel must not flip to passthrough mid-drag);
/// - entering the panel becomes interactive immediately;
/// - leaving it needs three stable frames (~30 ms) to absorb boundary
///   jitter;
/// - the check polls `GetCursorPos`, not egui hover: a click-through window
///   does not receive mouse-move messages, so hover alone could never bring
///   the panel back.
fn update_pointer_region(
    overlay: &mut GlfwBackend,
    ctx: &egui::Context,
    region: &mut RegionState,
    ui_visible: bool,
    ui_rect: Option<egui::Rect>,
) {
    let want_passthrough = if !ui_visible {
        region.rect = None;
        region.inside = None;
        region.stable = 0;
        true
    } else {
        region.rect = ui_rect;
        match ui_rect.and_then(|rect| RegionState::screen_rect(overlay, ctx, rect)) {
            // No window painted (all scripts idle) or no geometry yet: let
            // the game receive the mouse.
            None => true,
            Some((min, max)) => {
                let (cursor_x, cursor_y) = crate::window_util::cursor_pos();
                let inside = (cursor_x as f32) >= min.x
                    && (cursor_x as f32) <= max.x
                    && (cursor_y as f32) >= min.y
                    && (cursor_y as f32) <= max.y;
                if region.inside != Some(inside) {
                    region.inside = Some(inside);
                    region.stable = 0;
                } else {
                    region.stable = region.stable.saturating_add(1);
                }
                if any_mouse_button_down() {
                    overlay.is_passthrough()
                } else if inside {
                    false
                } else if region.stable >= 3 {
                    true
                } else {
                    overlay.is_passthrough()
                }
            }
        }
    };
    if overlay.is_passthrough() != want_passthrough {
        tracing::info!(
            want_passthrough,
            ui_visible,
            region = ?region.inside,
            stable = region.stable,
            focused = overlay.focused,
            "passthrough-toggle"
        );
    }
    overlay.set_passthrough(want_passthrough);
}

pub(super) struct DisplayLoop {
    pub(super) overlay: GlfwBackend,
    pub(super) gfx: D3D11Backend,
    pub(super) app: KernelScriptApp,
}

impl DisplayLoop {
    pub(super) fn run(mut self) {
        tracing::info!("entering overlay display loop");
        let Self { overlay, gfx, app } = &mut self;
        let ctx = egui::Context::default();
        // Ctrl +/-/0 must never rescale the overlay while the user is
        // aiming in game; the whole-UI scale is fixed at UI_SCALE.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let ui_rect_handle = app.ui_rect_handle();

        let mut next_display = Instant::now();
        let mut windows_shown = false;
        let mut region = RegionState::default();

        loop {
            // Single thread: input pump + logic + presentation, paced at
            // 100 Hz. Early event-pump wakeups process input but skip the
            // frame, exactly like ks-td2.
            let wait = next_display
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            overlay.glfw.wait_events_timeout(wait);
            overlay.tick();
            crate::lua_runtime::set_content_scale(overlay.scale);
            app.poll_input();

            if overlay.window.should_close() || app.quit_requested {
                break;
            }
            if Instant::now() < next_display {
                continue;
            }
            next_display += FRAME_INTERVAL;
            let now = Instant::now();
            if next_display < now {
                next_display = now;
            }

            app.driver().poll();
            app.ensure_chrome(&ctx);
            app.sample_fps();
            let fps = app.fps_ema;
            let ui_visible = app.ui_visible();
            let (origin_x, origin_y) = overlay.window.get_pos();
            let origin = (origin_x as f32, origin_y as f32);

            render_pass(&ctx, gfx, overlay, |ui| {
                let ctx = ui.ctx();
                if ui_visible {
                    app.control_window(ctx, &ui_rect_handle);
                }
                app.run_lua_frame(ctx, now, ui_visible, origin, fps);
            });

            // Apply zoom after the pass: egui consumes it at the next
            // `run_ui`, whose ratio hack then scales the previous (valid)
            // screen_rect instead of the never-set default.
            ctx.set_zoom_factor(UI_SCALE);

            update_pointer_region(overlay, &ctx, &mut region, ui_visible, ui_rect_handle.get());

            // Reveal only after the first overlay frame (no white flash).
            if !windows_shown {
                overlay.window.show();
                windows_shown = true;
            }
        }
        tracing::info!("overlay display loop exited");
    }
}
