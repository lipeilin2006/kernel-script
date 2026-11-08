//! 本地 vendor：egui 的 D3D11 渲染后端（`ks-gui` 透明覆盖层 / `ks-inspector`
//! 普通不透明窗口共用同一套 egui 管线）。
//!
//! 背景（2026-10-11）：旧 GPU / 旧显卡驱动上 three_d（OpenGL 3.3）路径的
//! 覆盖层黑屏。本后端把呈现链换成：
//!   GLFW 窗口（ClientApi=NoApi）→ DXGI CreateSwapChainForHwnd（DISCARD）
//!   → DwmEnableBlurBehindWindow 空区域逐像素 alpha 透明
//! 绕开 GL 上下文；窗口/输入/点击穿透仍走 egui_window_glfw_passthrough 的
//! NoApi 分支（WS_EX_LAYERED|WS_EX_TRANSPARENT + LWA_ALPHA 255）。
//!
//! 历史教训（rev0-rev3，勿回退）：SwapChainForComposition + DirectComposition
//! DesktopWindowTarget 与 WS_EX_LAYERED 组合在部分机器上整窗黑屏（本机正常、
//! 用户机黑屏，机器差异），弃用；参考 imgui_dx11-master 的成熟配方 = HWND
//! 普通交换链 + ImGui_ImplWin32_EnableAlphaCompositing（Win8+ 分支 =
//! DwmEnableBlurBehindWindow 空区域）+ LAYERED 穿透。
//!
//! 绘制语义逐条对齐 egui_render_glow（见 ks-td2 vendor，移植参照）：
//! - 顶点：egui 逻辑点、左上原点；屏幕变换在 VS 里做（u_screen_size）。
//! - 混合：预乘 alpha —— RGB ONE / INV_SRC_ALPHA，A INV_DEST_ALPHA / ONE
//!   （glow blend_func_separate 同款）。
//! - clip_rect（逻辑点）× scale → 物理像素 scissor（D3D 顶左原点，无 Y 翻转）。
//! - 纹理：RGBA8 直通 = glow 的 sRGB 纹理 + sRGB 输出着色器的净效果；
//!   字体纹理（Managed 0）固定 linear+clamp，其余按 TexturesDelta 的
//!   minification 选 linear/point；free 在绘制之后。
//! - Primitive::Callback 不支持（跳过 + debug 日志；ks-td2 不使用）。
//!
//! 顶点格式与 egui epaint::Vertex 布局一致（repr(C)，20 字节）：
//! pos [f32;2]@0、uv [f32;2]@8、color [u8;4]@16（UNORM 输入，预乘语义保持）。

// Vendored backend: the public API takes a raw `HWND` exactly like the
// upstream glow/three_d backends, and the remaining lints are pure style in
// ported code that must stay byte-comparable with its ks-td2 source.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::field_reassign_with_default)]

mod shaders;

use std::collections::HashMap;
use std::ffi::c_void;

use egui::epaint::{ImageDelta, Primitive, Vertex};
use egui::{ClippedPrimitive, ImageData, TextureId, TexturesDelta};
use windows::core::{s, Interface, Result as WinResult};
use windows::Win32::Foundation::{FALSE, HWND, RECT, TRUE};
use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11BlendState, ID3D11Buffer, ID3D11DepthStencilState, ID3D11Device,
    ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_INDEX_BUFFER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_DEST_ALPHA, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BOX, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_ALWAYS,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CULL_NONE, D3D11_DEPTH_STENCIL_DESC, D3D11_DEPTH_WRITE_MASK_ALL, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_INPUT_ELEMENT_DESC,
    D3D11_INPUT_PER_VERTEX_DATA, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
    D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dwm::{
    DwmEnableBlurBehindWindow, DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIDevice, IDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HRGN};

const INITIAL_BUFFER_BYTES: usize = 64 * 1024;

/// 诊断：读取 HWND 的 GWL_EXSTYLE（穿透路径期望
/// WS_EX_LAYERED|WS_EX_TRANSPARENT=0xC0038；参考 imgui_dx11 同款）。
pub fn debug_hwnd_exstyle(hwnd: *mut c_void) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_EXSTYLE};
    unsafe { GetWindowLongW(HWND(hwnd), GWL_EXSTYLE) as i32 }
}

/// 诊断：屏幕点 (x,y) 处经系统命中测试落到的顶层窗口（穿透生效时
/// 应返回下层窗口而非覆盖层自身）。
pub fn debug_window_from_point(x: i32, y: i32) -> isize {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
    unsafe { WindowFromPoint(POINT { x, y }).0 as isize }
}

/// 诊断：移动系统光标（配合命中测试用）。
pub fn debug_set_cursor_pos(x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

/// 诊断：窗口屏幕矩形 [left, top, right, bottom]。
pub fn debug_window_rect(hwnd: *mut c_void) -> [i32; 4] {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(HWND(hwnd), &mut r);
    }
    [r.left, r.top, r.right, r.bottom]
}

/// egui 的 D3D11 后端：接口形状与 glow/ThreeD 后端对齐
/// （`new` / `resize_framebuffer` / `prepare_frame` / `render_egui` /
/// `present` / `set_clear_color`），供 ks-td2 的 `EguiGfx` trait 统一驱动。
pub struct D3D11Backend {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: windows::Win32::Graphics::Dxgi::IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    backbuffer: Option<ID3D11Texture2D>,
    staging: Option<ID3D11Texture2D>,
    blend: ID3D11BlendState,
    rasterizer: ID3D11RasterizerState,
    depth_stencil: ID3D11DepthStencilState,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    cbuffer: ID3D11Buffer,
    vbo: ID3D11Buffer,
    ibo: ID3D11Buffer,
    vbo_capacity: usize,
    ibo_capacity: usize,
    sampler_linear: ID3D11SamplerState,
    sampler_point: ID3D11SamplerState,
    textures: HashMap<u64, GpuTexture>,
    clear_color: [f32; 4],
    framebuffer_size: [u32; 2],
    logical_screen_size: [f32; 2],
    frames: u64,
    resizes: u64,
}

struct GpuTexture {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    size: [u32; 2],
    /// 0 = linear（字体纹理固定），1 = point。
    sampler: u8,
}

impl D3D11Backend {
    /// `hwnd` = the window handle. This is the transparent-overlay
    /// constructor used by `ks-gui`: it installs the DWM blur-behind region
    /// that turns the swap chain's per-pixel alpha into desktop transparency.
    /// Ordinary windows must use [`Self::new_opaque`] instead.
    pub fn new(hwnd: *mut c_void, framebuffer_size: [u32; 2]) -> Self {
        unsafe {
            Self::new_inner(hwnd, framebuffer_size, true)
                .expect("failed to create d3d11 overlay backend")
        }
    }

    /// Same egui pipeline for an ordinary opaque window (`ks-inspector`):
    /// the swap chain is not composited per-pixel, so no DWM blur-behind
    /// region is installed. The window keeps its normal opaque background.
    pub fn new_opaque(hwnd: *mut c_void, framebuffer_size: [u32; 2]) -> Self {
        unsafe {
            Self::new_inner(hwnd, framebuffer_size, false).expect("failed to create d3d11 backend")
        }
    }

    unsafe fn new_inner(
        hwnd: *mut c_void,
        framebuffer_size: [u32; 2],
        overlay_transparent: bool,
    ) -> WinResult<Self> {
        // ── 设备：FL11_0 → 10_1 → 10_0 回退（TD2 最低配置即 DX11 硬件）──
        let levels = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
        ];
        let mut device_opt: Option<ID3D11Device> = None;
        let mut context_opt: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device_opt),
            Some(&mut level),
            Some(&mut context_opt),
        )?;
        let device = device_opt.expect("d3d11 device");
        let context = context_opt.expect("d3d11 context");

        // ── 适配器与特性级别日志（旧机器排障关键信息）──
        let dxgi_device: IDXGIDevice = device.cast()?;
        if let Ok(adapter) = dxgi_device.GetAdapter() {
            if let Ok(adapter1) = adapter.cast::<IDXGIAdapter1>() {
                if let Ok(desc) = adapter1.GetDesc1() {
                    let name = String::from_utf16_lossy(&desc.Description);
                    let name = name.trim_end_matches('\0');
                    tracing::info!(
                        adapter = %name,
                        feature_level = ?level.0,
                        "d3d11 overlay: device created"
                    );
                }
            }
        }

        // ── 普通 HWND 交换链（DISCARD）；透明覆盖层额外挂 DWM BlurBehind
        // 空区域（imgui_dx11 配方，= ImGui_ImplWin32_EnableAlphaCompositing
        // Win8+），普通窗口不挂，保持完全不透明。──
        // 不用 SwapChainForComposition/DComp：与 WS_EX_LAYERED 组合在部分
        // 机器上整窗黑屏（rev3 实机复现）。
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let fb_w = framebuffer_size[0].max(1);
        let fb_h = framebuffer_size[1].max(1);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: fb_w,
            Height: fb_h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: 0,
        };
        let swapchain = factory.CreateSwapChainForHwnd(&device, HWND(hwnd), &desc, None, None)?;
        if overlay_transparent {
            // Per-pixel transparency for the overlay: an empty-region
            // BlurBehind (imgui_dx11 Win8+ recipe). An ordinary window skips
            // this entirely and composites as an opaque surface.
            let region: HRGN = CreateRectRgn(0, 0, -1, -1);
            let bb = DWM_BLURBEHIND {
                dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                fEnable: TRUE,
                hRgnBlur: region,
                fTransitionOnMaximized: FALSE,
            };
            DwmEnableBlurBehindWindow(HWND(hwnd), &bb)?;
            let _ = DeleteObject(region.into());
        }

        let backend = Self {
            device: device.clone(),
            context,
            rtv: None,
            blend: Self::create_blend_state(&device)?,
            rasterizer: Self::create_rasterizer_state(&device)?,
            depth_stencil: Self::create_depth_stencil_state(&device)?,
            vs: {
                let mut o: Option<ID3D11VertexShader> = None;
                device.CreateVertexShader(shaders::EGUI_VS, None, Some(&mut o))?;
                o.expect("vs")
            },
            ps: {
                let mut o: Option<ID3D11PixelShader> = None;
                device.CreatePixelShader(shaders::EGUI_PS, None, Some(&mut o))?;
                o.expect("ps")
            },
            layout: Self::create_input_layout(&device)?,
            cbuffer: Self::create_dynamic_buffer(&device, 16, D3D11_BIND_CONSTANT_BUFFER.0 as u32)?,
            vbo: Self::create_dynamic_buffer(
                &device,
                INITIAL_BUFFER_BYTES,
                D3D11_BIND_VERTEX_BUFFER.0 as u32,
            )?,
            ibo: Self::create_dynamic_buffer(
                &device,
                INITIAL_BUFFER_BYTES,
                D3D11_BIND_INDEX_BUFFER.0 as u32,
            )?,
            vbo_capacity: INITIAL_BUFFER_BYTES,
            ibo_capacity: INITIAL_BUFFER_BYTES,
            sampler_linear: Self::create_sampler(&device, D3D11_FILTER_MIN_MAG_MIP_LINEAR)?,
            sampler_point: Self::create_sampler(&device, D3D11_FILTER_MIN_MAG_MIP_POINT)?,
            textures: HashMap::new(),
            clear_color: [0.0; 4],
            framebuffer_size,
            logical_screen_size: [fb_w as f32, fb_h as f32],
            frames: 0,
            resizes: 0,
            backbuffer: None,
            staging: None,
            swapchain,
        };
        // borrow-checker 兜底：上面按字段初始化后补 RTV。
        let mut backend = backend;
        backend.recreate_rtv()?;
        Ok(backend)
    }

    // ── 公共接口（与 glow/ThreeD 后端同名同义）──

    /// 最小化等 0 尺寸：放掉 RTV 停绘，等下一次非零 resize 恢复。
    pub fn resize_framebuffer(&mut self, fb_size: [u32; 2]) {
        if fb_size == self.framebuffer_size {
            return;
        }
        self.framebuffer_size = fb_size;
        self.resizes += 1;
        // 对齐 imgui_dx11 / D3D11-Overlay-ImGui 的 CleanupRenderTarget →
        // ResizeBuffers → CreateRenderTarget：ResizeBuffers 前必须释放
        // **全部** 交换链缓冲引用。rtv 之外 backbuffer（GetBuffer 长期
        // 持有，供 debug_readback）与 staging 也须放掉，否则 ResizeBuffers
        // 返回 DXGI_ERROR_INVALID_CALL → 旧路径提前 return、rtv 永远
        // None → 之后所有帧跳过绘制（实机：覆盖层一随游戏窗口变尺寸就
        // 渲染卡死，Insert/穿透仍在跑但画面冻结）。
        self.rtv = None;
        self.backbuffer = None;
        self.staging = None;
        if fb_size[0] == 0 || fb_size[1] == 0 {
            return;
        }
        unsafe {
            if let Err(e) = self.swapchain.ResizeBuffers(
                0,
                fb_size[0],
                fb_size[1],
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            ) {
                tracing::warn!("d3d11 ResizeBuffers failed: {e:?}");
                // 失败也要重建 RTV 绑回旧缓冲，避免整层永久停绘。
                if let Err(e2) = self.recreate_rtv() {
                    tracing::warn!("d3d11 recreate rtv after failed resize: {e2:?}");
                }
                return;
            }
        }
        if let Err(e) = unsafe { self.recreate_rtv() } {
            tracing::warn!("d3d11 recreate rtv failed: {e:?}");
        }
    }

    /// 清屏（等价 glow 的 clear_color + clear：GL 路径设置后由 prepare_frame
    /// 使用；本后端在 prepare_frame 里用该颜色清合成交换链）。
    pub fn set_clear_color(&mut self, rgba: [f32; 4]) {
        self.clear_color = rgba;
    }

    /// 等价 glow 的 prepare_frame：SCISSOR 关、整目标清屏、全视口。
    /// 入参为最新 framebuffer 尺寸（glow 忽略闭包，这里同样不依赖）。
    pub fn prepare_frame(&mut self, _latest_framebuffer_size: [u32; 2]) {
        let Some(rtv) = self.rtv.clone() else {
            return; // 0x0（最小化）期间无 RTV
        };
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.context.ClearRenderTargetView(&rtv, &self.clear_color);
            if self.framebuffer_size[0] > 0 && self.framebuffer_size[1] > 0 {
                let vp = D3D11_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: self.framebuffer_size[0] as f32,
                    Height: self.framebuffer_size[1] as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                self.context.RSSetViewports(Some(&[vp]));
            }
        }
    }

    pub fn render_egui(
        &mut self,
        meshes: Vec<ClippedPrimitive>,
        mut textures_delta: TexturesDelta,
        logical_screen_size: [f32; 2],
    ) {
        self.logical_screen_size = logical_screen_size;
        self.frames += 1;

        // 纹理增量：先 set（绘制前），free 留到绘制后（glow 同序）。
        for (texture_id, delta) in textures_delta
            .set
            .iter()
            .flat_map(|(&id, deltas)| deltas.iter().map(move |d| (id, d)))
        {
            unsafe { self.apply_texture_delta(texture_id, delta) };
        }

        if self.rtv.is_some() {
            let drawn: usize = meshes
                .iter()
                .map(|c| match &c.primitive {
                    Primitive::Mesh(m) => m.indices.len(),
                    _ => 0,
                })
                .sum();
            if self.frames == 1 || self.frames % 120 == 0 {
                tracing::info!(
                    frame = self.frames,
                    meshes = meshes.len(),
                    drawn_indices = drawn,
                    textures = self.textures.len(),
                    screen = ?logical_screen_size,
                    fb = ?self.framebuffer_size,
                    resizes = self.resizes,
                    "d3d11 render"
                );
            }
            let scale = if logical_screen_size[0] > 0.0 {
                self.framebuffer_size[0] as f32 / logical_screen_size[0]
            } else {
                1.0
            };
            unsafe { self.bind_pipeline() };
            let cb = [logical_screen_size[0], logical_screen_size[1], 0.0, 0.0];
            // 常量缓冲是 DYNAMIC 用量：UpdateSubresource 对 DYNAMIC 资源无效
            // （cbuffer 保持 0 → screen_size=(0,0) → 顶点 NaN → 全部网格不可见）。
            // 必须走 Map(WRITE_DISCARD)。
            unsafe {
                self.upload(&self.cbuffer.clone(), bytemuck::cast_slice(&cb));
            }

            for clipped in &meshes {
                let Some(scissor) =
                    scissor_from_clip_rect(&clipped.clip_rect, scale, self.framebuffer_size)
                else {
                    continue;
                };
                match &clipped.primitive {
                    Primitive::Mesh(mesh) => {
                        if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                            continue;
                        }
                        let key = match mesh.texture_id {
                            TextureId::Managed(k) => k,
                            TextureId::User(_) => {
                                tracing::debug!("d3d11: user textures unsupported; skipping mesh");
                                continue;
                            }
                        };
                        let Some(tex) = self.textures.get(&key) else {
                            tracing::debug!(
                                key,
                                "d3d11: mesh references missing texture; skipping"
                            );
                            continue;
                        };
                        let srv = tex.srv.clone();
                        let sampler = if tex.sampler == 1 {
                            self.sampler_point.clone()
                        } else {
                            self.sampler_linear.clone()
                        };
                        let rect = RECT {
                            left: scissor[0] as i32,
                            top: scissor[1] as i32,
                            right: (scissor[0] + scissor[2]) as i32,
                            bottom: (scissor[1] + scissor[3]) as i32,
                        };
                        let vbytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
                        let ibytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
                        unsafe { self.ensure_capacity(vbytes.len(), ibytes.len()) };
                        unsafe {
                            self.context.RSSetScissorRects(Some(&[rect]));
                            self.context.PSSetShaderResources(0, Some(&[Some(srv)]));
                            self.context.PSSetSamplers(0, Some(&[Some(sampler)]));
                            self.upload(&self.vbo.clone(), vbytes);
                            self.upload(&self.ibo.clone(), ibytes);
                            let bufs = [Some(self.vbo.clone())];
                            let strides = [std::mem::size_of::<Vertex>() as u32];
                            let offsets = [0u32];
                            self.context.IASetVertexBuffers(
                                0,
                                1,
                                Some(bufs.as_ptr()),
                                Some(strides.as_ptr()),
                                Some(offsets.as_ptr()),
                            );
                            self.context
                                .IASetIndexBuffer(Some(&self.ibo), DXGI_FORMAT_R32_UINT, 0);
                            self.context.DrawIndexed(mesh.indices.len() as u32, 0, 0);
                        }
                    }
                    Primitive::Callback(_) => {
                        tracing::debug!("d3d11: paint callbacks unsupported; skipping");
                    }
                }
            }
        }

        for tid in textures_delta.free.iter().copied() {
            match tid {
                TextureId::Managed(k) => {
                    self.textures.remove(&k);
                }
                TextureId::User(_) => {}
            }
        }
        // epaint::TexturesDelta 的 Drop 会 debug_assert 空表；已全部应用
        // （set 绘制前、free 绘制后），显式 clear（egui_glow 同款处理）。
        textures_delta.clear();
    }

    /// 呈现（d3d11 路径；GL 路径由 GlfwBackend::window.swap_buffers）。
    pub fn present(&mut self) {
        let hr = unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            tracing::warn!("d3d11 present failed: {hr:?}");
        }
    }

    /// 诊断：把交换链当前后缓冲回读为 BGRA 像素（冒烟/排障用；
    /// PrintWindow/BitBlt 截 DWM 玻璃窗口不可靠，GPU 回读为权威）。
    pub fn debug_readback_pixel(&mut self, x: u32, y: u32) -> Option<[u8; 4]> {
        let back = self.backbuffer.clone()?;
        let w = self.framebuffer_size[0];
        let h = self.framebuffer_size[1];
        if x >= w || y >= h {
            return None;
        }
        unsafe {
            if self.staging.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut t: Option<ID3D11Texture2D> = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .ok()?;
                self.staging = t;
            }
            let staging = self.staging.clone()?;
            self.context.CopyResource(&staging, &back);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .is_err()
            {
                return None;
            }
            let base = (y as usize) * (mapped.RowPitch as usize) + (x as usize) * 4;
            let p = mapped.pData as *const u8;
            let out = [
                *p.add(base),
                *p.add(base + 1),
                *p.add(base + 2),
                *p.add(base + 3),
            ];
            self.context.Unmap(&staging, 0);
            Some(out)
        }
    }

    // ── 内部 ──

    unsafe fn recreate_rtv(&mut self) -> WinResult<()> {
        self.rtv = None;
        self.backbuffer = None;
        self.staging = None;
        if self.framebuffer_size[0] == 0 || self.framebuffer_size[1] == 0 {
            return Ok(());
        }
        let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        self.device
            .CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        self.rtv = rtv;
        self.backbuffer = Some(back);
        Ok(())
    }

    unsafe fn bind_pipeline(&self) {
        self.context.IASetInputLayout(&self.layout);
        self.context
            .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        self.context.VSSetShader(&self.vs, None);
        self.context.PSSetShader(&self.ps, None);
        self.context
            .VSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
        self.context.OMSetBlendState(&self.blend, None, 0xffffffff);
        self.context.RSSetState(&self.rasterizer);
        self.context.OMSetDepthStencilState(&self.depth_stencil, 0);
        self.context.RSSetScissorRects(Some(&[RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }]));
    }

    unsafe fn apply_texture_delta(&mut self, texture_id: TextureId, delta: &ImageDelta) {
        let (pixels, size): (Vec<u8>, [usize; 2]) = match &delta.image {
            ImageData::Color(c) => (c.pixels.iter().flat_map(|p| p.to_array()).collect(), c.size),
        };
        let key = match texture_id {
            TextureId::Managed(k) => k,
            TextureId::User(_) => {
                tracing::debug!("d3d11: user texture delta ignored");
                return;
            }
        };
        let w = size[0] as u32;
        let h = size[1] as u32;
        if !self.textures.contains_key(&key) {
            let sampler = if key == 0 {
                0 // 字体图集：linear + clamp（glow font_sampler 同义）
            } else {
                match delta.options.minification {
                    egui::TextureFilter::Nearest => 1,
                    egui::TextureFilter::Linear => 0,
                }
            };
            match self.create_texture(w, h, &pixels, sampler) {
                Ok(tex) => {
                    self.textures.insert(key, tex);
                }
                Err(e) => {
                    tracing::warn!(key, "d3d11: texture create failed: {e:?}");
                    return;
                }
            }
            if delta.pos.is_some() {
                tracing::debug!(key, "d3d11: partial delta on new texture ignored");
            }
            return;
        }
        let sampler = if key == 0 {
            0 // 字体图集：linear + clamp（glow font_sampler 同义）
        } else {
            match delta.options.minification {
                egui::TextureFilter::Nearest => 1,
                egui::TextureFilter::Linear => 0,
            }
        };
        // 全量 delta 且尺寸变化 = 字体图集扩容（epaint 从 32 高起步、按需
        // 翻倍高度后发 ImageDelta::full）。D3D11 UpdateSubresource 不能改
        // 纹理尺寸——对旧尺寸纹理写入新尺寸数据 = 设备错误/图集整块损坏
        // （实机：切设置 tab 加载新中文字形 → 全 UI 文字乱 + 卡死）。
        // glow 对应路径是 tex_image_2d（GL 允许原地改尺寸）；此处销毁重建。
        if delta.pos.is_none() && self.textures[&key].size != [w, h] {
            let old = self.textures.remove(&key).expect("checked contains_key");
            let (old_w, old_h) = (old.size[0], old.size[1]);
            drop(old);
            tracing::info!(
                key,
                old_w,
                old_h,
                new_w = w,
                new_h = h,
                "d3d11: texture full-delta resize; recreating"
            );
            match self.create_texture(w, h, &pixels, sampler) {
                Ok(tex) => {
                    self.textures.insert(key, tex);
                }
                Err(e) => {
                    tracing::warn!(key, "d3d11: texture recreate failed: {e:?}");
                }
            }
            return;
        }
        let tex = self.textures[&key].tex.clone();
        // SrcRowPitch 必须是本块每行字节数（w*4）；传 0 是非法值，
        // 字体图集增量（新增 glyph）会损坏 → 全部 mesh 采样黑图。
        let row_pitch = w * 4;
        unsafe {
            if let Some(pos) = delta.pos {
                let rect = D3D11_BOX {
                    left: pos[0] as u32,
                    top: pos[1] as u32,
                    right: pos[0] as u32 + w,
                    bottom: pos[1] as u32 + h,
                    front: 0,
                    back: 1,
                };
                self.context.UpdateSubresource(
                    &tex,
                    0,
                    Some(&rect as *const D3D11_BOX),
                    pixels.as_ptr() as *const c_void,
                    row_pitch,
                    0,
                );
            } else {
                self.context.UpdateSubresource(
                    &tex,
                    0,
                    None,
                    pixels.as_ptr() as *const c_void,
                    row_pitch,
                    0,
                );
            }
        }
    }

    unsafe fn create_texture(
        &self,
        w: u32,
        h: u32,
        pixels: &[u8],
        sampler: u8,
    ) -> WinResult<GpuTexture> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const c_void,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        let tex = tex.expect("d3d11 texture");
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        self.device
            .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        let srv = srv.expect("d3d11 srv");
        Ok(GpuTexture {
            tex,
            srv,
            size: [w, h],
            sampler,
        })
    }

    unsafe fn ensure_capacity(&mut self, vbytes: usize, ibytes: usize) {
        if vbytes > self.vbo_capacity {
            let cap = vbytes.next_power_of_two();
            self.vbo =
                Self::create_dynamic_buffer(&self.device, cap, D3D11_BIND_VERTEX_BUFFER.0 as u32)
                    .expect("d3d11 vbo grow");
            self.vbo_capacity = cap;
        }
        if ibytes > self.ibo_capacity {
            let cap = ibytes.next_power_of_two();
            self.ibo =
                Self::create_dynamic_buffer(&self.device, cap, D3D11_BIND_INDEX_BUFFER.0 as u32)
                    .expect("d3d11 ibo grow");
            self.ibo_capacity = cap;
        }
    }

    unsafe fn upload(&self, buffer: &ID3D11Buffer, bytes: &[u8]) {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .expect("d3d11 map buffer");
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.pData as *mut u8, bytes.len());
        self.context.Unmap(buffer, 0);
    }

    unsafe fn create_dynamic_buffer(
        device: &ID3D11Device,
        byte_width: usize,
        bind_flags: u32,
    ) -> WinResult<ID3D11Buffer> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: byte_width as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: bind_flags,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut buf: Option<ID3D11Buffer> = None;
        device.CreateBuffer(&desc, None, Some(&mut buf))?;
        Ok(buf.expect("d3d11 buffer"))
    }

    unsafe fn create_sampler(
        device: &ID3D11Device,
        filter: windows::Win32::Graphics::Direct3D11::D3D11_FILTER,
    ) -> WinResult<ID3D11SamplerState> {
        let mut desc = D3D11_SAMPLER_DESC::default();
        desc.Filter = filter;
        desc.AddressU = D3D11_TEXTURE_ADDRESS_CLAMP;
        desc.AddressV = D3D11_TEXTURE_ADDRESS_CLAMP;
        desc.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
        desc.MipLODBias = 0.0;
        desc.MaxAnisotropy = 1;
        desc.ComparisonFunc = D3D11_COMPARISON_ALWAYS;
        desc.MinLOD = 0.0;
        desc.MaxLOD = f32::MAX;
        let mut sampler: Option<ID3D11SamplerState> = None;
        device.CreateSamplerState(&desc, Some(&mut sampler))?;
        Ok(sampler.expect("d3d11 sampler"))
    }

    unsafe fn create_blend_state(device: &ID3D11Device) -> WinResult<ID3D11BlendState> {
        // 预乘 alpha：RGB = ONE / INV_SRC_ALPHA；A = INV_DEST_ALPHA / ONE
        // （与 egui_render_glow 的 blend_func_separate 完全一致）。
        let mut desc = D3D11_BLEND_DESC::default();
        desc.AlphaToCoverageEnable = FALSE;
        desc.IndependentBlendEnable = FALSE;
        let rt = &mut desc.RenderTarget[0];
        rt.BlendEnable = TRUE;
        rt.SrcBlend = D3D11_BLEND_ONE;
        rt.DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        rt.BlendOp = D3D11_BLEND_OP_ADD;
        rt.SrcBlendAlpha = D3D11_BLEND_INV_DEST_ALPHA;
        rt.DestBlendAlpha = D3D11_BLEND_ONE;
        rt.BlendOpAlpha = D3D11_BLEND_OP_ADD;
        rt.RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let mut state: Option<ID3D11BlendState> = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.expect("d3d11 blend"))
    }

    unsafe fn create_rasterizer_state(device: &ID3D11Device) -> WinResult<ID3D11RasterizerState> {
        let mut desc = D3D11_RASTERIZER_DESC::default();
        desc.FillMode = D3D11_FILL_SOLID;
        desc.CullMode = D3D11_CULL_NONE;
        desc.FrontCounterClockwise = FALSE;
        desc.DepthBias = 0;
        desc.DepthBiasClamp = 0.0;
        desc.SlopeScaledDepthBias = 0.0;
        desc.DepthClipEnable = TRUE;
        desc.ScissorEnable = TRUE;
        desc.MultisampleEnable = FALSE;
        desc.AntialiasedLineEnable = FALSE;
        let mut state: Option<ID3D11RasterizerState> = None;
        device.CreateRasterizerState(&desc, Some(&mut state))?;
        Ok(state.expect("d3d11 rasterizer"))
    }

    unsafe fn create_depth_stencil_state(
        device: &ID3D11Device,
    ) -> WinResult<ID3D11DepthStencilState> {
        let mut desc = D3D11_DEPTH_STENCIL_DESC::default();
        desc.DepthEnable = FALSE;
        desc.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL;
        desc.DepthFunc = D3D11_COMPARISON_ALWAYS;
        desc.StencilEnable = FALSE;
        desc.StencilReadMask = 0xff;
        desc.StencilWriteMask = 0xff;
        let mut state: Option<ID3D11DepthStencilState> = None;
        device.CreateDepthStencilState(&desc, Some(&mut state))?;
        Ok(state.expect("d3d11 depth stencil"))
    }

    unsafe fn create_input_layout(device: &ID3D11Device) -> WinResult<ID3D11InputLayout> {
        let stride = std::mem::size_of::<Vertex>() as u32;
        debug_assert_eq!(stride, 20);
        let elements = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 8,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("COLOR"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                InputSlot: 0,
                AlignedByteOffset: 16,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let _ = stride;
        let mut layout: Option<ID3D11InputLayout> = None;
        device.CreateInputLayout(&elements, shaders::EGUI_VS, Some(&mut layout))?;
        Ok(layout.expect("d3d11 input layout"))
    }
}

/// clip_rect（egui 逻辑点）→ 物理像素 scissor [x, y, w, h]（顶左原点，
/// D3D 直用）。移植自 egui_render_glow 的 scissor_from_clip_rect（跳过
/// OpenGL 的 Y 翻转层）。
fn scissor_from_clip_rect(
    clip_rect: &egui::Rect,
    scale: f32,
    physical_framebuffer_size: [u32; 2],
) -> Option<[u32; 4]> {
    let clip_min_x = (scale * clip_rect.min.x).round() as i32;
    let clip_min_y = (scale * clip_rect.min.y).round() as i32;
    let clip_max_x = (scale * clip_rect.max.x).round() as i32;
    let clip_max_y = (scale * clip_rect.max.y).round() as i32;

    let clip_min_x = clip_min_x.clamp(0, physical_framebuffer_size[0] as i32);
    let clip_min_y = clip_min_y.clamp(0, physical_framebuffer_size[1] as i32);
    let clip_max_x = clip_max_x.clamp(clip_min_x, physical_framebuffer_size[0] as i32);
    let clip_max_y = clip_max_y.clamp(clip_min_y, physical_framebuffer_size[1] as i32);

    let x = clip_min_x as u32;
    let y = clip_min_y as u32;
    let width = (clip_max_x - clip_min_x) as u32;
    let height = (clip_max_y - clip_min_y) as u32;
    (width != 0 && height != 0).then_some([x, y, width, height])
}
