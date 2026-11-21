//! Hardware DX11 scene depth-buffer occlusion for world-space co-op ground loot overlays.
//!
//! Hooks `ID3D11DeviceContext::OMSetRenderTargets` and `ID3D11DeviceContext::ClearDepthStencilView`
//! (both immediate and deferred contexts) to identify Sekiro's main scene depth buffer, snapshot it
//! before any end-of-frame/next-frame clear, and expose ImGui `RawCallback` brackets that swap in
//! a depth-testing pixel shader (`SV_POSITION.xy` -> `depthTex.SampleLevel(..., 0)`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use hudhook::imgui::sys::{ImDrawCmd, ImDrawList, ImDrawList_AddCallback, igGetForegroundDrawList};
use hudhook::mh::{MH_CreateHook, MH_EnableHook, MH_Initialize, MH_STATUS};
use hudhook::windows::Win32::Foundation::HMODULE;
use hudhook::windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use hudhook::windows::Win32::Graphics::Direct3D::{
    D3D11_SRV_DIMENSION_TEXTURE2D, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_11_0, ID3DBlob,
};
use hudhook::windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DEPTH_STENCIL,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_CLEAR_DEPTH, D3D11_COMPARISON_ALWAYS,
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_FLAG, D3D11_DEPTH_STENCIL_VIEW_DESC,
    D3D11_DEVICE_CONTEXT_IMMEDIATE, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAP_WRITE_DISCARD,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_SRV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, ID3D11Buffer, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11Resource, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use hudhook::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_D16_UNORM, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT,
    DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UNORM,
    DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R24_UNORM_X8_TYPELESS, DXGI_FORMAT_R32_FLOAT,
    DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_SAMPLE_DESC,
};
use hudhook::windows::core::{Interface, s};

use crate::log;

const RENDER_WIDTH_RVA: usize = 0x3B1D100;
const RENDER_HEIGHT_RVA: usize = 0x3B1D104;
const MAX_CANDIDATES: usize = 8;
const STALE_FRAMES: u64 = 30;
/// Require at least a handful of scene draw binds before snapshot-on-clear so start-of-frame
/// clears do not overwrite the prior frame's finished scene depth.
const MIN_BINDS_BEFORE_CLEAR_SNAPSHOT: u32 = 4;

type FnOMSetRenderTargets =
    unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void, *mut c_void);
type FnClearDepthStencilView =
    unsafe extern "system" fn(*mut c_void, *mut c_void, u32, f32, u8);

static INSTALLED: OnceLock<()> = OnceLock::new();

static ORIG_OM_IMM: AtomicUsize = AtomicUsize::new(0);
static ORIG_OM_DEF: AtomicUsize = AtomicUsize::new(0);
static ORIG_CLEAR_IMM: AtomicUsize = AtomicUsize::new(0);
static ORIG_CLEAR_DEF: AtomicUsize = AtomicUsize::new(0);

static KNOWN_MAIN_DSV_PTR: AtomicUsize = AtomicUsize::new(0);
static KNOWN_MAIN_TEX_PTR: AtomicUsize = AtomicUsize::new(0);
static MAIN_DSV_BINDS: AtomicU32 = AtomicU32::new(0);
static WANT_DEPTH_SNAPSHOT: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_TAKEN_THIS_FRAME: AtomicBool = AtomicBool::new(false);
static FRAME_COPIED: AtomicBool = AtomicBool::new(false);
static IS_REVERSED_Z: AtomicBool = AtomicBool::new(true);
static HAS_MAIN_TEX: AtomicBool = AtomicBool::new(false);
static HAS_IMM_CTX: AtomicBool = AtomicBool::new(false);
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_SEEN_FRAME: AtomicU64 = AtomicU64::new(0);
static DISPLAY_W: AtomicU32 = AtomicU32::new(0);
static DISPLAY_H: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static IN_HOOK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct HookGuard;

impl HookGuard {
    fn enter() -> Option<Self> {
        IN_HOOK.with(|c| {
            if c.get() {
                None
            } else {
                c.set(true);
                Some(Self)
            }
        })
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        IN_HOOK.with(|c| c.set(false));
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DepthParams {
    z_params: [f32; 4],  // x = obj_view_z, y = near_plane, z = far_plane, w = bias_m
    vp_params: [f32; 4], // x = inv_vp_w, y = inv_vp_h, z = is_reversed, w = depth_enabled
}

struct Candidate {
    tex_ptr: usize,
    last_dsv_ptr: usize,
    w: u32,
    h: u32,
    max_rtvs: u32,
    binds: u32,
    last_seen_frame: u64,
}

#[derive(Default)]
struct GpuState {
    device: Option<ID3D11Device>,
    imm_ctx: Option<ID3D11DeviceContext>,
    candidates: [Option<Candidate>; MAX_CANDIDATES],
    main_idx: Option<usize>,
    main_tex: Option<ID3D11Texture2D>,
    main_desc: Option<(u32, u32, DXGI_FORMAT)>,
    logged_desc: Option<(u32, u32, u32)>,
    copy_tex: Option<ID3D11Texture2D>,
    copy_srv: Option<ID3D11ShaderResourceView>,
    copy_desc: Option<(u32, u32, DXGI_FORMAT)>,
    ps: Option<ID3D11PixelShader>,
    cb: Option<ID3D11Buffer>,
    sampler: Option<ID3D11SamplerState>,
    ps_failed: bool,
    last_display: (u32, u32),
    saved_active: bool,
    saved_ps: Option<ID3D11PixelShader>,
    saved_srv1: Option<ID3D11ShaderResourceView>,
    saved_samp1: Option<ID3D11SamplerState>,
    saved_cb0: Option<ID3D11Buffer>,
    frame_params: Vec<DepthParams>,
}

static STATE: Mutex<Option<GpuState>> = Mutex::new(None);

fn lock_state() -> Option<std::sync::MutexGuard<'static, Option<GpuState>>> {
    STATE.lock().ok()
}

/// Maps a DXGI depth/typeless format to `(copy_texture_format, srv_format)` for GPU sampling.
pub(crate) fn depth_copy_and_srv_format(fmt: DXGI_FORMAT) -> Option<(DXGI_FORMAT, DXGI_FORMAT)> {
    match fmt {
        DXGI_FORMAT_R32_TYPELESS | DXGI_FORMAT_D32_FLOAT => {
            Some((DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32_FLOAT))
        }
        DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_D32_FLOAT_S8X24_UINT => {
            Some((DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS))
        }
        DXGI_FORMAT_R24G8_TYPELESS | DXGI_FORMAT_D24_UNORM_S8_UINT => {
            Some((DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R24_UNORM_X8_TYPELESS))
        }
        DXGI_FORMAT_R16_TYPELESS | DXGI_FORMAT_D16_UNORM => {
            Some((DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UNORM))
        }
        _ => None,
    }
}

fn close_dim(a: u32, b: u32) -> bool {
    b > 0 && a.abs_diff(b) <= 2
}

/// Filters DSV textures down to full-resolution single-sample main scene depth buffers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn is_scene_depth_candidate(
    w: u32,
    h: u32,
    array_size: u32,
    sample_count: u32,
    bind_flags: u32,
    fmt: DXGI_FORMAT,
    display_w: u32,
    display_h: u32,
    render_w: u32,
    render_h: u32,
) -> bool {
    if array_size != 1
        || sample_count != 1
        || (bind_flags & D3D11_BIND_DEPTH_STENCIL.0 as u32) == 0
        || depth_copy_and_srv_format(fmt).is_none()
        || w < 640
        || h < 360
    {
        return false;
    }
    let display_square = display_w > 0 && display_w == display_h;
    let render_square = render_w > 0 && render_w == render_h;
    if w == h && !display_square && !render_square {
        return false;
    }
    let matches_display = display_w > 0 && display_h > 0 && close_dim(w, display_w) && close_dim(h, display_h);
    let matches_render = render_w > 0 && render_h > 0 && close_dim(w, render_w) && close_dim(h, render_h);
    if (display_w > 0 && display_h > 0) || (render_w > 0 && render_h > 0) {
        matches_display || matches_render
    } else {
        w > h
    }
}

/// Prefers G-buffer depth buffers (multiple RTVs bound alongside DSV) over single-RTV targets,
/// breaking ties by higher bind count this cycle.
pub(crate) fn beats_candidate(
    cur_max_rtvs: u32,
    cur_binds: u32,
    cand_max_rtvs: u32,
    cand_binds: u32,
) -> bool {
    let cur_mrt = cur_max_rtvs >= 2;
    let cand_mrt = cand_max_rtvs >= 2;
    match (cand_mrt, cur_mrt) {
        (true, false) => true,
        (false, true) => false,
        _ => cand_binds > cur_binds,
    }
}

/// Converts a raw D3D11 hardware depth sample `[0, 1]` into view-space meters.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn linearize_depth(raw_d: f32, near: f32, far: f32, reversed_z: bool) -> f32 {
    if reversed_z {
        (near * far) / (near + raw_d * (far - near)).max(1e-6)
    } else {
        (near * far) / (far - raw_d * (far - near)).max(1e-6)
    }
}

/// Pure depth comparison matching the GPU pixel shader: discards when geometry sits clearly closer
/// to the camera than `obj_view_z` (beyond `bias_m`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_occluded(
    obj_view_z: f32,
    raw_d: f32,
    near: f32,
    far: f32,
    reversed_z: bool,
    bias_m: f32,
) -> bool {
    obj_view_z > linearize_depth(raw_d, near, far, reversed_z) + bias_m
}

fn read_render_dims() -> (u32, u32) {
    let base = crate::game::module_base();
    if base == 0 {
        return (0, 0);
    }
    let rw = crate::game::read::<i32>(base + RENDER_WIDTH_RVA).filter(|&v| v > 0).unwrap_or(0) as u32;
    let rh = crate::game::read::<i32>(base + RENDER_HEIGHT_RVA).filter(|&v| v > 0).unwrap_or(0) as u32;
    (rw, rh)
}

fn dsv_format_or_tex(dsv: &ID3D11DepthStencilView, tex_fmt: DXGI_FORMAT) -> DXGI_FORMAT {
    let mut desc = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
    unsafe { dsv.GetDesc(&mut desc) };
    if desc.Format.0 != 0 { desc.Format } else { tex_fmt }
}

fn ensure_copy_resources(
    st: &mut GpuState,
    src_desc: (u32, u32, DXGI_FORMAT),
) -> Option<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let dev = st.device.as_ref()?.clone();
    if st.copy_desc == Some(src_desc)
        && let (Some(ct), Some(srv)) = (&st.copy_tex, &st.copy_srv)
    {
        return Some((ct.clone(), srv.clone()));
    }
    let (copy_fmt, srv_fmt) = depth_copy_and_srv_format(src_desc.2)?;
    st.copy_tex = None;
    st.copy_srv = None;
    st.copy_desc = None;

    let make_tex = |bind_flags: u32| -> hudhook::windows::core::Result<ID3D11Texture2D> {
        let mut tex: Option<ID3D11Texture2D> = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src_desc.0,
            Height: src_desc.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: copy_fmt,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind_flags,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.expect("CreateTexture2D returned null"))
    };

    let tex = make_tex((D3D11_BIND_DEPTH_STENCIL.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)
        .or_else(|_| make_tex(D3D11_BIND_SHADER_RESOURCE.0 as u32))
        .ok()?;

    let mut srv: Option<ID3D11ShaderResourceView> = None;
    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: srv_fmt,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 },
        },
    };
    unsafe { dev.CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv)).ok()? };
    let srv = srv?;
    st.copy_tex = Some(tex.clone());
    st.copy_srv = Some(srv.clone());
    st.copy_desc = Some(src_desc);
    Some((tex, srv))
}

const DEPTH_TEST_PS_HLSL: &str = r"
cbuffer DepthParams : register(b0) {
    float4 z_params;
    float4 vp_params;
};
struct PS_INPUT {
    float4 pos : SV_POSITION;
    float4 col : COLOR0;
    float2 uv  : TEXCOORD0;
};
Texture2D texture0 : register(t0);
Texture2D<float> depthTex : register(t1);
SamplerState sampler0 : register(s0);
SamplerState depthSampler : register(s1);
float4 main(PS_INPUT input) : SV_Target {
    if (vp_params.w > 0.5) {
        float2 uv = input.pos.xy * vp_params.xy;
        float d = depthTex.SampleLevel(depthSampler, uv, 0);
        float n = z_params.y;
        float f = z_params.z;
        float scene_z = (vp_params.z > 0.5)
            ? (n * f) / max(n + d * (f - n), 1e-6)
            : (n * f) / max(f - d * (f - n), 1e-6);
        if (z_params.x > scene_z + z_params.w) {
            discard;
        }
    }
    return input.col * texture0.Sample(sampler0, input.uv);
}
";

fn ensure_shader_pipeline(
    st: &mut GpuState,
) -> Option<(ID3D11PixelShader, ID3D11Buffer, ID3D11SamplerState)> {
    if st.ps_failed {
        return None;
    }
    if let (Some(ps), Some(cb), Some(samp)) = (&st.ps, &st.cb, &st.sampler) {
        return Some((ps.clone(), cb.clone(), samp.clone()));
    }
    let dev = st.device.as_ref()?.clone();

    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            DEPTH_TEST_PS_HLSL.as_ptr().cast(),
            DEPTH_TEST_PS_HLSL.len(),
            None,
            None,
            None,
            s!("main\0"),
            s!("ps_4_0\0"),
            0,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if hr.is_err() || blob.is_none() {
        let msg = err
            .map(|e| unsafe {
                let s = std::slice::from_raw_parts(
                    e.GetBufferPointer().cast::<u8>(),
                    e.GetBufferSize(),
                );
                String::from_utf8_lossy(s).into_owned()
            })
            .unwrap_or_else(|| format!("{hr:?}"));
        log::line(&format!("depth: D3DCompile failed: {msg}"));
        st.ps_failed = true;
        return None;
    }
    let blob = blob.unwrap();
    let bytecode = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    };
    let mut ps: Option<ID3D11PixelShader> = None;
    if let Err(e) = unsafe { dev.CreatePixelShader(bytecode, None, Some(&mut ps)) } {
        log::line(&format!("depth: CreatePixelShader failed: {e:?}"));
        st.ps_failed = true;
        return None;
    }

    let mut cb: Option<ID3D11Buffer> = None;
    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<DepthParams>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    if let Err(e) = unsafe { dev.CreateBuffer(&cb_desc, None, Some(&mut cb)) } {
        log::line(&format!("depth: CreateBuffer failed: {e:?}"));
        st.ps_failed = true;
        return None;
    }

    let mut samp: Option<ID3D11SamplerState> = None;
    let samp_desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D11_COMPARISON_ALWAYS,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    if let Err(e) = unsafe { dev.CreateSamplerState(&samp_desc, Some(&mut samp)) } {
        log::line(&format!("depth: CreateSamplerState failed: {e:?}"));
        st.ps_failed = true;
        return None;
    }

    let (ps, cb, samp) = (ps?, cb?, samp?);
    st.ps = Some(ps.clone());
    st.cb = Some(cb.clone());
    st.sampler = Some(samp.clone());
    Some((ps, cb, samp))
}

unsafe fn om_set_render_targets_common(
    this: *mut c_void,
    num_views: u32,
    pp_rtvs: *const *mut c_void,
    p_dsv: *mut c_void,
    orig: FnOMSetRenderTargets,
) {
    unsafe { orig(this, num_views, pp_rtvs, p_dsv) };
    if !crate::mod_alive() {
        return;
    }
    let Some(_guard) = HookGuard::enter() else { return };
    let Some(ctx) = (unsafe { ID3D11DeviceContext::from_raw_borrowed(&this) }) else {
        return;
    };
    if !HAS_IMM_CTX.load(Ordering::Relaxed)
        && unsafe { ctx.GetType() } == D3D11_DEVICE_CONTEXT_IMMEDIATE
    {
        let dev = unsafe { ctx.GetDevice() }.ok();
        if let Some(mut g) = lock_state() {
            let st = g.get_or_insert_with(GpuState::default);
            if st.imm_ctx.is_none() {
                st.imm_ctx = Some(ctx.clone());
                if st.device.is_none() {
                    st.device = dev;
                }
                HAS_IMM_CTX.store(true, Ordering::Release);
            }
        }
    }
    if p_dsv.is_null() {
        return;
    }
    let dsv_ptr = p_dsv as usize;
    if dsv_ptr == KNOWN_MAIN_DSV_PTR.load(Ordering::Relaxed) {
        MAIN_DSV_BINDS.fetch_add(1, Ordering::Relaxed);
        LAST_SEEN_FRAME.store(FRAME_SEQ.load(Ordering::Relaxed), Ordering::Relaxed);
        return;
    }
    let Some(dsv) = (unsafe { ID3D11DepthStencilView::from_raw_borrowed(&p_dsv) }) else {
        return;
    };
    let Ok(res): hudhook::windows::core::Result<ID3D11Resource> = (unsafe { dsv.GetResource() }) else {
        return;
    };
    let dim = unsafe { res.GetType() };
    if dim != D3D11_RESOURCE_DIMENSION_TEXTURE2D {
        return;
    }
    let Ok(tex) = res.cast::<ID3D11Texture2D>() else { return };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    let fmt = dsv_format_or_tex(dsv, desc.Format);
    let dw = DISPLAY_W.load(Ordering::Relaxed);
    let dh = DISPLAY_H.load(Ordering::Relaxed);
    let (rw, rh) = read_render_dims();
    if !is_scene_depth_candidate(
        desc.Width,
        desc.Height,
        desc.ArraySize,
        desc.SampleDesc.Count,
        desc.BindFlags,
        fmt,
        dw,
        dh,
        rw,
        rh,
    ) {
        return;
    }

    let tex_ptr = tex.as_raw() as usize;
    let cur_frame = FRAME_SEQ.load(Ordering::Relaxed);
    let mut log_msg: Option<String> = None;

    if let Some(mut g) = lock_state() {
        let st = g.get_or_insert_with(GpuState::default);
        if st.device.is_none()
            && let Ok(dev) = unsafe { ctx.GetDevice() }
        {
            st.device = Some(dev);
        }
        let mut idx = st.candidates.iter().position(|c| {
            c.as_ref()
                .is_some_and(|cand| cand.tex_ptr == tex_ptr && cand.w == desc.Width && cand.h == desc.Height)
        });
        if idx.is_none() {
            idx = st.candidates.iter().position(Option::is_none).or_else(|| {
                st.candidates
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| st.main_idx != Some(*i))
                    .min_by_key(|(_, c)| c.as_ref().map_or(0, |x| x.binds))
                    .map(|(i, _)| i)
            });
            if let Some(slot) = idx {
                st.candidates[slot] = Some(Candidate {
                    tex_ptr,
                    last_dsv_ptr: dsv_ptr,
                    w: desc.Width,
                    h: desc.Height,
                    max_rtvs: num_views,
                    binds: 1,
                    last_seen_frame: cur_frame,
                });
            }
        } else if let Some(slot) = idx
            && let Some(c) = st.candidates[slot].as_mut()
        {
            c.last_dsv_ptr = dsv_ptr;
            c.max_rtvs = c.max_rtvs.max(num_views);
            c.binds = c.binds.saturating_add(1);
            c.last_seen_frame = cur_frame;
        }

        if let Some(slot) = idx {
            let promote = match st.main_idx.and_then(|mi| st.candidates[mi].as_ref()) {
                None => true,
                Some(_) if st.main_idx == Some(slot) => false,
                Some(cur) => {
                    cur_frame.saturating_sub(cur.last_seen_frame) > STALE_FRAMES
                        || beats_candidate(
                            cur.max_rtvs,
                            cur.binds,
                            st.candidates[slot].as_ref().map_or(0, |c| c.max_rtvs),
                            st.candidates[slot].as_ref().map_or(0, |c| c.binds),
                        )
                }
            };
            if promote || st.main_idx == Some(slot) {
                let (m_binds, m_rtvs) = st.candidates[slot]
                    .as_ref()
                    .map_or((0, 0), |c| (c.binds, c.max_rtvs));
                st.main_idx = Some(slot);
                st.main_tex = Some(tex);
                st.main_desc = Some((desc.Width, desc.Height, fmt));
                KNOWN_MAIN_DSV_PTR.store(dsv_ptr, Ordering::Relaxed);
                KNOWN_MAIN_TEX_PTR.store(tex_ptr, Ordering::Relaxed);
                MAIN_DSV_BINDS.store(m_binds, Ordering::Relaxed);
                LAST_SEEN_FRAME.store(cur_frame, Ordering::Relaxed);
                HAS_MAIN_TEX.store(true, Ordering::Release);
                let key = (desc.Width, desc.Height, fmt.0 as u32);
                if promote && st.logged_desc != Some(key) {
                    st.logged_desc = Some(key);
                    log_msg = Some(format!(
                        "depth: scene depth buffer {}x{} fmt={} (rtvs={m_rtvs})",
                        desc.Width, desc.Height, fmt.0
                    ));
                }
            }
        }
    }
    if let Some(msg) = log_msg {
        log::line(&msg);
    }
}

unsafe fn clear_depth_stencil_view_common(
    this: *mut c_void,
    p_dsv: *mut c_void,
    clear_flags: u32,
    depth: f32,
    stencil: u8,
    orig: FnClearDepthStencilView,
) {
    if !crate::mod_alive()
        || (clear_flags & D3D11_CLEAR_DEPTH.0 as u32) == 0
        || p_dsv.is_null()
    {
        unsafe { orig(this, p_dsv, clear_flags, depth, stencil) };
        return;
    }
    let Some(_guard) = HookGuard::enter() else {
        unsafe { orig(this, p_dsv, clear_flags, depth, stencil) };
        return;
    };

    let dsv_ptr = p_dsv as usize;
    let mut is_main = dsv_ptr == KNOWN_MAIN_DSV_PTR.load(Ordering::Relaxed);
    if !is_main {
        let known_tex = KNOWN_MAIN_TEX_PTR.load(Ordering::Relaxed);
        if known_tex != 0
            && let Some(dsv) = unsafe { ID3D11DepthStencilView::from_raw_borrowed(&p_dsv) }
            && let Ok(res) = unsafe { dsv.GetResource() }
            && res.as_raw() as usize == known_tex
        {
            is_main = true;
            KNOWN_MAIN_DSV_PTR.store(dsv_ptr, Ordering::Relaxed);
        }
    }

    if is_main {
        IS_REVERSED_Z.store(depth <= 0.5, Ordering::Relaxed);
        if WANT_DEPTH_SNAPSHOT.load(Ordering::Relaxed)
            && MAIN_DSV_BINDS.load(Ordering::Relaxed) >= MIN_BINDS_BEFORE_CLEAR_SNAPSHOT
            && !SNAPSHOT_TAKEN_THIS_FRAME.load(Ordering::Acquire)
            && let Some(ctx) = unsafe { ID3D11DeviceContext::from_raw_borrowed(&this) }
        {
            let copy_pair = if let Some(mut g) = lock_state() {
                if let Some(st) = g.as_mut()
                    && let (Some(src), Some(desc)) = (st.main_tex.clone(), st.main_desc)
                {
                    ensure_copy_resources(st, desc).map(|(dst, _)| (dst, src))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((dst, src)) = copy_pair {
                unsafe { ctx.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None) };
                SNAPSHOT_TAKEN_THIS_FRAME.store(true, Ordering::Release);
            }
        }
        MAIN_DSV_BINDS.store(0, Ordering::Relaxed);
    }

    unsafe { orig(this, p_dsv, clear_flags, depth, stencil) };
}

unsafe extern "system" fn om_set_render_targets_imm(
    this: *mut c_void,
    num_views: u32,
    pp_rtvs: *const *mut c_void,
    p_dsv: *mut c_void,
) {
    let orig = ORIG_OM_IMM.load(Ordering::Acquire);
    if orig != 0 {
        let f: FnOMSetRenderTargets = unsafe { std::mem::transmute(orig) };
        unsafe { om_set_render_targets_common(this, num_views, pp_rtvs, p_dsv, f) };
    }
}

unsafe extern "system" fn om_set_render_targets_def(
    this: *mut c_void,
    num_views: u32,
    pp_rtvs: *const *mut c_void,
    p_dsv: *mut c_void,
) {
    let orig = ORIG_OM_DEF.load(Ordering::Acquire);
    if orig != 0 {
        let f: FnOMSetRenderTargets = unsafe { std::mem::transmute(orig) };
        unsafe { om_set_render_targets_common(this, num_views, pp_rtvs, p_dsv, f) };
    }
}

unsafe extern "system" fn clear_dsv_imm(
    this: *mut c_void,
    p_dsv: *mut c_void,
    clear_flags: u32,
    depth: f32,
    stencil: u8,
) {
    let orig = ORIG_CLEAR_IMM.load(Ordering::Acquire);
    if orig != 0 {
        let f: FnClearDepthStencilView = unsafe { std::mem::transmute(orig) };
        unsafe { clear_depth_stencil_view_common(this, p_dsv, clear_flags, depth, stencil, f) };
    }
}

unsafe extern "system" fn clear_dsv_def(
    this: *mut c_void,
    p_dsv: *mut c_void,
    clear_flags: u32,
    depth: f32,
    stencil: u8,
) {
    let orig = ORIG_CLEAR_DEF.load(Ordering::Acquire);
    if orig != 0 {
        let f: FnClearDepthStencilView = unsafe { std::mem::transmute(orig) };
        unsafe { clear_depth_stencil_view_common(this, p_dsv, clear_flags, depth, stencil, f) };
    }
}

fn hook_one(addr: usize, detour: *mut c_void, slot: &AtomicUsize, label: &str) -> bool {
    let mut tramp = std::ptr::null_mut();
    let st = unsafe { MH_CreateHook(addr as *mut c_void, detour, &mut tramp) };
    if st != MH_STATUS::MH_OK {
        log::line(&format!("depth: MH_CreateHook {label} failed: {st:?}"));
        return false;
    }
    slot.store(tramp as usize, Ordering::Release);
    let en = unsafe { MH_EnableHook(addr as *mut c_void) };
    if en != MH_STATUS::MH_OK {
        log::line(&format!("depth: MH_EnableHook {label} failed: {en:?}"));
        return false;
    }
    true
}

/// Installs D3D11 depth-buffer hooks (once). Safe to call right after `Hudhook::apply()`.
pub fn install() {
    INSTALLED.get_or_init(|| {
        let mut dev: Option<ID3D11Device> = None;
        let mut imm: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_0];
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut imm),
            )
        };
        let (Ok(()), Some(dev), Some(imm)) = (&hr, dev, imm) else {
            log::line(&format!("depth: D3D11CreateDevice failed ({hr:?})"));
            return;
        };

        let mut def: Option<ID3D11DeviceContext> = None;
        let _ = unsafe { dev.CreateDeferredContext(0, Some(&mut def)) };

        let imm_vt = imm.vtable();
        let imm_om = imm_vt.OMSetRenderTargets as usize;
        let imm_clr = imm_vt.ClearDepthStencilView as usize;

        let mh = unsafe { MH_Initialize() };
        if mh != MH_STATUS::MH_OK && mh != MH_STATUS::MH_ERROR_ALREADY_INITIALIZED {
            log::line(&format!("depth: MH_Initialize failed: {mh:?}"));
            return;
        }

        let mut ok = 0u32;
        if hook_one(imm_om, om_set_render_targets_imm as *mut c_void, &ORIG_OM_IMM, "imm OMSetRenderTargets") {
            ok += 1;
        }
        if hook_one(imm_clr, clear_dsv_imm as *mut c_void, &ORIG_CLEAR_IMM, "imm ClearDepthStencilView") {
            ok += 1;
        }
        if let Some(ref d) = def {
            let def_vt = d.vtable();
            let def_om = def_vt.OMSetRenderTargets as usize;
            let def_clr = def_vt.ClearDepthStencilView as usize;
            if def_om != imm_om
                && hook_one(def_om, om_set_render_targets_def as *mut c_void, &ORIG_OM_DEF, "def OMSetRenderTargets")
            {
                ok += 1;
            }
            if def_clr != imm_clr
                && hook_one(def_clr, clear_dsv_def as *mut c_void, &ORIG_CLEAR_DEF, "def ClearDepthStencilView")
            {
                ok += 1;
            }
        }
        log::line(&format!("depth: installed {ok} DX11 depth hook(s)"));
    });
}

/// Resets per-frame state at the start of `Overlay::render`.
pub fn begin_overlay_frame(display_w: f32, display_h: f32, want_depth: bool) {
    let dw = display_w.max(0.0) as u32;
    let dh = display_h.max(0.0) as u32;
    DISPLAY_W.store(dw, Ordering::Relaxed);
    DISPLAY_H.store(dh, Ordering::Relaxed);
    WANT_DEPTH_SNAPSHOT.store(want_depth, Ordering::Relaxed);
    FRAME_COPIED.store(false, Ordering::Release);

    let frame = FRAME_SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if !want_depth {
        SNAPSHOT_TAKEN_THIS_FRAME.store(false, Ordering::Release);
        MAIN_DSV_BINDS.store(0, Ordering::Relaxed);
    }

    let stale = frame.saturating_sub(LAST_SEEN_FRAME.load(Ordering::Relaxed)) > STALE_FRAMES;
    if let Some(mut g) = lock_state()
        && let Some(st) = g.as_mut()
    {
        st.frame_params.clear();
        let size_changed = dw > 0 && dh > 0 && st.last_display != (0, 0) && st.last_display != (dw, dh);
        if dw > 0 && dh > 0 {
            st.last_display = (dw, dh);
        }
        if size_changed || stale {
            if size_changed {
                for c in &mut st.candidates {
                    *c = None;
                }
                st.main_idx = None;
                st.copy_tex = None;
                st.copy_srv = None;
                st.copy_desc = None;
            }
            st.main_tex = None;
            st.main_desc = None;
            KNOWN_MAIN_DSV_PTR.store(0, Ordering::Relaxed);
            KNOWN_MAIN_TEX_PTR.store(0, Ordering::Relaxed);
            MAIN_DSV_BINDS.store(0, Ordering::Relaxed);
            HAS_MAIN_TEX.store(false, Ordering::Release);
        }
    }
}

/// Resets end-of-frame snapshot flags when `want_depth` was set but no markers emitted callbacks.
pub fn finish_frame_without_callbacks() {
    SNAPSHOT_TAKEN_THIS_FRAME.store(false, Ordering::Release);
    MAIN_DSV_BINDS.store(0, Ordering::Relaxed);
    FRAME_COPIED.store(false, Ordering::Release);
}

/// True once the DX11 immediate context and scene depth buffer are captured.
pub fn ready() -> bool {
    HAS_IMM_CTX.load(Ordering::Acquire) && HAS_MAIN_TEX.load(Ordering::Acquire)
}

/// Brackets subsequent foreground ImGui draws with depth testing against `view_z` (in meters).
pub fn push_depth_begin(view_z: f32, near: f32, far: f32, bias_m: f32) -> bool {
    if !ready() {
        return false;
    }
    let dw = DISPLAY_W.load(Ordering::Relaxed).max(1) as f32;
    let dh = DISPLAY_H.load(Ordering::Relaxed).max(1) as f32;
    let rev = if IS_REVERSED_Z.load(Ordering::Relaxed) { 1.0 } else { 0.0 };
    let params = DepthParams {
        z_params: [view_z.max(near), near.max(0.01), far.max(near + 1.0), bias_m],
        vp_params: [1.0 / dw, 1.0 / dh, rev, 1.0],
    };
    let idx = if let Some(mut g) = lock_state() {
        let st = g.get_or_insert_with(GpuState::default);
        let idx = st.frame_params.len();
        st.frame_params.push(params);
        idx
    } else {
        return false;
    };
    unsafe {
        let dl = igGetForegroundDrawList();
        if dl.is_null() {
            return false;
        }
        ImDrawList_AddCallback(dl, Some(begin_depth_cb), (idx + 1) as *mut c_void);
    }
    true
}

/// Restores `hudhook`'s normal unoccluded pixel shader after depth-tested loot markers.
pub fn push_depth_end() {
    unsafe {
        let dl = igGetForegroundDrawList();
        if !dl.is_null() {
            ImDrawList_AddCallback(dl, Some(end_depth_cb), std::ptr::null_mut());
        }
    }
}

unsafe extern "C" fn begin_depth_cb(_dl: *const ImDrawList, cmd: *const ImDrawCmd) {
    let Some(idx) = (unsafe { (*cmd).UserCallbackData } as usize).checked_sub(1) else {
        return;
    };

    let (imm_ctx, params, copy_pair, srv, pipe, save_needed) = {
        let Some(mut g) = lock_state() else { return };
        let Some(st) = g.as_mut() else { return };
        let Some(imm_ctx) = st.imm_ctx.clone() else { return };
        let Some(&params) = st.frame_params.get(idx) else { return };

        let pipe = ensure_shader_pipeline(st);
        let copy_res = st.main_desc.and_then(|d| ensure_copy_resources(st, d));
        let copy_pair = if !FRAME_COPIED.load(Ordering::Acquire) && !SNAPSHOT_TAKEN_THIS_FRAME.load(Ordering::Acquire) {
            st.main_tex
                .clone()
                .zip(copy_res.as_ref().map(|(ct, _)| ct.clone()))
        } else {
            None
        };
        let srv = copy_res.map(|(_, s)| s);
        let save_needed = !st.saved_active;
        (imm_ctx, params, copy_pair, srv, pipe, save_needed)
    };

    let Some((ps, cb, samp)) = pipe else { return };

    if let Some((src, dst)) = copy_pair {
        if let Some(_guard) = HookGuard::enter() {
            unsafe { imm_ctx.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None) };
            FRAME_COPIED.store(true, Ordering::Release);
            SNAPSHOT_TAKEN_THIS_FRAME.store(true, Ordering::Release);
        }
    } else {
        FRAME_COPIED.store(true, Ordering::Release);
    }

    if save_needed {
        let mut orig_ps = None;
        let mut ps_inst_count = 0u32;
        let mut orig_srv1: [Option<ID3D11ShaderResourceView>; 1] = [None];
        let mut orig_samp1: [Option<ID3D11SamplerState>; 1] = [None];
        let mut orig_cb0: [Option<ID3D11Buffer>; 1] = [None];
        unsafe {
            imm_ctx.PSGetShader(&mut orig_ps, None, Some(&mut ps_inst_count));
            imm_ctx.PSGetShaderResources(1, Some(&mut orig_srv1));
            imm_ctx.PSGetSamplers(1, Some(&mut orig_samp1));
            imm_ctx.PSGetConstantBuffers(0, Some(&mut orig_cb0));
        }
        if let Some(mut g) = lock_state()
            && let Some(st) = g.as_mut()
            && !st.saved_active
        {
            st.saved_ps = orig_ps;
            st.saved_srv1 = orig_srv1[0].take();
            st.saved_samp1 = orig_samp1[0].take();
            st.saved_cb0 = orig_cb0[0].take();
            st.saved_active = true;
        }
    }

    let mut effective = params;
    if srv.is_none() {
        effective.vp_params[3] = 0.0;
    }

    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if imm_ctx
            .Map(&cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .is_ok()
        {
            std::ptr::copy_nonoverlapping(&effective, mapped.pData.cast::<DepthParams>(), 1);
            imm_ctx.Unmap(&cb, 0);
        }
        imm_ctx.PSSetShader(&ps, Some(&[]));
        imm_ctx.PSSetShaderResources(1, Some(&[srv]));
        imm_ctx.PSSetSamplers(1, Some(&[Some(samp)]));
        imm_ctx.PSSetConstantBuffers(0, Some(&[Some(cb)]));
    }
}

unsafe extern "C" fn end_depth_cb(_dl: *const ImDrawList, _cmd: *const ImDrawCmd) {
    let restore = if let Some(mut g) = lock_state() {
        if let Some(st) = g.as_mut()
            && st.saved_active
        {
            st.saved_active = false;
            st.imm_ctx.clone().map(|ctx| {
                (
                    ctx,
                    st.saved_ps.take(),
                    st.saved_srv1.take(),
                    st.saved_samp1.take(),
                    st.saved_cb0.take(),
                )
            })
        } else {
            None
        }
    } else {
        None
    };

    if let Some((ctx, ps, srv1, samp1, cb0)) = restore {
        unsafe {
            ctx.PSSetShader(ps.as_ref(), Some(&[]));
            ctx.PSSetShaderResources(1, Some(&[srv1]));
            ctx.PSSetSamplers(1, Some(&[samp1]));
            ctx.PSSetConstantBuffers(0, Some(&[cb0]));
        }
    }
    SNAPSHOT_TAKEN_THIS_FRAME.store(false, Ordering::Release);
    MAIN_DSV_BINDS.store(0, Ordering::Relaxed);
    FRAME_COPIED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudhook::windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    };

    #[test]
    fn depth_formats_map_typeless_and_depth_to_copy_and_srv() {
        assert_eq!(
            depth_copy_and_srv_format(DXGI_FORMAT_D32_FLOAT),
            Some((DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32_FLOAT))
        );
        assert_eq!(
            depth_copy_and_srv_format(DXGI_FORMAT_R32_TYPELESS),
            Some((DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32_FLOAT))
        );
        assert_eq!(
            depth_copy_and_srv_format(DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            Some((DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS))
        );
        assert_eq!(
            depth_copy_and_srv_format(DXGI_FORMAT_D24_UNORM_S8_UINT),
            Some((DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R24_UNORM_X8_TYPELESS))
        );
        assert_eq!(
            depth_copy_and_srv_format(DXGI_FORMAT_D16_UNORM),
            Some((DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UNORM))
        );
        assert_eq!(depth_copy_and_srv_format(DXGI_FORMAT_R8G8B8A8_UNORM), None);
        assert_eq!(depth_copy_and_srv_format(DXGI_FORMAT_B8G8R8A8_UNORM), None);
    }

    #[test]
    fn scene_depth_candidate_filters_shadow_maps_msaa_and_wrong_resolutions() {
        let bind_dsv = D3D11_BIND_DEPTH_STENCIL.0 as u32;
        assert!(is_scene_depth_candidate(
            1920, 1080, 1, 1, bind_dsv, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, 1920, 1080, 1920, 1080
        ));
        // Ultrawide display with matching depth size
        assert!(is_scene_depth_candidate(
            3440, 1440, 1, 1, bind_dsv, DXGI_FORMAT_D32_FLOAT, 3440, 1440, 3440, 1440
        ));
        // Square shadow map rejected
        assert!(!is_scene_depth_candidate(
            2048, 2048, 1, 1, bind_dsv, DXGI_FORMAT_D32_FLOAT, 1920, 1080, 1920, 1080
        ));
        // MSAA or array > 1 or missing depth bind rejected
        assert!(!is_scene_depth_candidate(
            1920, 1080, 1, 4, bind_dsv, DXGI_FORMAT_D32_FLOAT, 1920, 1080, 1920, 1080
        ));
        assert!(!is_scene_depth_candidate(
            1920, 1080, 2, 1, bind_dsv, DXGI_FORMAT_D32_FLOAT, 1920, 1080, 1920, 1080
        ));
        assert!(!is_scene_depth_candidate(
            1920, 1080, 1, 1, 0, DXGI_FORMAT_D32_FLOAT, 1920, 1080, 1920, 1080
        ));
        // Half-res bloom/SSAO depth rejected
        assert!(!is_scene_depth_candidate(
            960, 540, 1, 1, bind_dsv, DXGI_FORMAT_D32_FLOAT, 1920, 1080, 1920, 1080
        ));
    }

    #[test]
    fn beats_candidate_prefers_mrt_gbuffer_over_single_rtv_and_breaks_ties_by_binds() {
        assert!(beats_candidate(1, 50, 3, 2));
        assert!(!beats_candidate(3, 2, 1, 50));
        assert!(beats_candidate(3, 5, 3, 6));
        assert!(!beats_candidate(3, 6, 3, 5));
    }

    #[test]
    fn linearize_depth_and_occlusion_match_reversed_z_and_standard_z() {
        let near = 0.08f32;
        let far = 10_000.0f32;
        // Reversed-Z: raw_d = 1 at near, ~near/z at z meters. For z = 4.0 m: raw_d ~= near / 4.0
        let raw_4m_rev = (near * (far - 4.0)) / (4.0 * (far - near));
        let z_rev = linearize_depth(raw_4m_rev, near, far, true);
        assert!((z_rev - 4.0).abs() < 1e-3, "got {z_rev}");
        assert!(is_occluded(5.0, raw_4m_rev, near, far, true, 0.06));
        assert!(!is_occluded(3.5, raw_4m_rev, near, far, true, 0.06));
        // Slight bias prevents self-occlusion right on the floor/pouch surface.
        assert!(!is_occluded(4.02, raw_4m_rev, near, far, true, 0.06));

        // Standard-Z: raw_d = (far * (z - near)) / (z * (far - near))
        let raw_4m_std = (far * (4.0 - near)) / (4.0 * (far - near));
        let z_std = linearize_depth(raw_4m_std, near, far, false);
        assert!((z_std - 4.0).abs() < 1e-3, "got {z_std}");
        assert!(is_occluded(5.0, raw_4m_std, near, far, false, 0.06));
        assert!(!is_occluded(3.5, raw_4m_std, near, far, false, 0.06));
    }
}
