//! Temporarily pauses Sekiro's own input polling while modal co-op dialogs (such as the
//! Drop Item quantity prompt) are open, plus a short button-release soak after closing so
//! pressing Confirm/Cancel never leaks into Sekiro's Inventory behind the dialog.
//!
//! Covers all three input paths a DX11 FromSoftware game can read:
//! 1. Win32 window messages (`hudhook::MessageFilter::InputAll` in `ui.rs`).
//! 2. `XInputGetState` / `XInputGetStateEx` (ordinal 100) across loaded XInput DLLs.
//! 3. `user32.dll` (`GetKeyboardState`, `GetAsyncKeyState`) and `dinput8.dll`
//!    (`IDirectInputDevice8W::GetDeviceState`, `GetDeviceData`).
//!
//! CooP Sekiro's own polling (`spectate_input`, `itemdrop`) wraps its Win32 calls in
//! `with_mod_poll(...)` so the mod still reads real input while Sekiro sees neutral state.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{self, null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use hudhook::mh::{MH_CreateHook, MH_EnableHook, MH_Initialize, MH_STATUS};

use crate::log;

/// Keep blocking Sekiro briefly after closing the dialog so key/button release doesn't leak.
const CLOSE_SOAK_MS: u64 = 220;

static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);
static BLOCK_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static IN_MOD_POLL: Cell<bool> = const { Cell::new(false) };
}

fn now_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Pure blocking predicate: true while dialog is open or within the post-close release soak.
pub fn should_block_at(dialog_open: bool, block_until_ms: u64, now_ms: u64) -> bool {
    dialog_open || now_ms < block_until_ms
}

/// Pure soak deadline calculation without wrapping overflow.
pub fn soak_deadline(now_ms: u64, soak_ms: u64) -> u64 {
    now_ms.saturating_add(soak_ms)
}

/// Called from the render loop to declare whether the modal Drop Item dialog is open.
pub fn set_dialog_open(open: bool) {
    let was_open = DIALOG_OPEN.swap(open, Ordering::AcqRel);
    if was_open && !open {
        BLOCK_UNTIL_MS.store(soak_deadline(now_ms(), CLOSE_SOAK_MS), Ordering::Release);
    }
}

/// Emergency release (used by `stuck.rs` recovery if the game ever needs mod holds cleared).
pub fn force_release() {
    DIALOG_OPEN.store(false, Ordering::Release);
    BLOCK_UNTIL_MS.store(0, Ordering::Release);
}

/// True when game input should be suppressed right now.
pub fn blocked() -> bool {
    crate::mod_alive()
        && should_block_at(
            DIALOG_OPEN.load(Ordering::Acquire),
            BLOCK_UNTIL_MS.load(Ordering::Acquire),
            now_ms(),
        )
}

struct ModPollGuard {
    prev: bool,
}

impl Drop for ModPollGuard {
    fn drop(&mut self) {
        IN_MOD_POLL.with(|c| c.set(self.prev));
    }
}

/// Runs `f` while bypassing our own `XInputGetState` / `GetAsyncKeyState` input-block detours,
/// so the mod overlay still reads real controller/keyboard input during modal dialogs.
pub fn with_mod_poll<R>(f: impl FnOnce() -> R) -> R {
    let prev = IN_MOD_POLL.with(|c| c.replace(true));
    let _guard = ModPollGuard { prev };
    f()
}

fn in_mod_poll() -> bool {
    IN_MOD_POLL.with(|c| c.get())
}

// --- Win32 / COM FFI ------------------------------------------------------------------------

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// --- XInput detours -------------------------------------------------------------------------

#[repr(C)]
struct XInputState {
    packet: u32,
    buttons: u16,
    lt: u8,
    rt: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XInputState) -> u32;

const MAX_XINPUT_SLOTS: usize = 6;
static ORIG_XINPUT: [AtomicUsize; MAX_XINPUT_SLOTS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

#[inline]
unsafe fn xinput_detour_common(slot: usize, user: u32, state: *mut XInputState) -> u32 {
    let orig_addr = ORIG_XINPUT[slot].load(Ordering::Acquire);
    if orig_addr == 0 {
        return 0;
    }
    let orig: XInputGetStateFn = unsafe { std::mem::transmute(orig_addr) };
    let ret = unsafe { orig(user, state) };
    if ret == 0 && !state.is_null() && blocked() && !in_mod_poll() {
        // Bump dwPacketNumber while active so the game's diff check latches the neutral state
        // instead of holding whichever button/direction opened the dialog.
        let s = unsafe { &mut *state };
        s.packet = s.packet.wrapping_add(0x4000_0000);
        s.buttons = 0;
        s.lt = 0;
        s.rt = 0;
        s.lx = 0;
        s.ly = 0;
        s.rx = 0;
        s.ry = 0;
    }
    ret
}

unsafe extern "system" fn xinput_detour_0(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(0, u, s) }
}
unsafe extern "system" fn xinput_detour_1(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(1, u, s) }
}
unsafe extern "system" fn xinput_detour_2(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(2, u, s) }
}
unsafe extern "system" fn xinput_detour_3(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(3, u, s) }
}
unsafe extern "system" fn xinput_detour_4(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(4, u, s) }
}
unsafe extern "system" fn xinput_detour_5(u: u32, s: *mut XInputState) -> u32 {
    unsafe { xinput_detour_common(5, u, s) }
}

const XINPUT_DETOURS: [XInputGetStateFn; MAX_XINPUT_SLOTS] = [
    xinput_detour_0,
    xinput_detour_1,
    xinput_detour_2,
    xinput_detour_3,
    xinput_detour_4,
    xinput_detour_5,
];

// --- User32 polling detours -----------------------------------------------------------------

type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> i32;
type GetAsyncKeyStateFn = unsafe extern "system" fn(i32) -> i16;

static ORIG_GET_KEYBOARD_STATE: AtomicUsize = AtomicUsize::new(0);
static ORIG_GET_ASYNC_KEY_STATE: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn get_keyboard_state_detour(buf: *mut u8) -> i32 {
    if blocked() && !in_mod_poll() {
        if !buf.is_null() {
            unsafe { ptr::write_bytes(buf, 0, 256) };
        }
        return 1;
    }
    let addr = ORIG_GET_KEYBOARD_STATE.load(Ordering::Acquire);
    if addr == 0 {
        return 0;
    }
    let orig: GetKeyboardStateFn = unsafe { std::mem::transmute(addr) };
    unsafe { orig(buf) }
}

unsafe extern "system" fn get_async_key_state_detour(vk: i32) -> i16 {
    if blocked() && !in_mod_poll() {
        return 0;
    }
    let addr = ORIG_GET_ASYNC_KEY_STATE.load(Ordering::Acquire);
    if addr == 0 {
        return 0;
    }
    let orig: GetAsyncKeyStateFn = unsafe { std::mem::transmute(addr) };
    unsafe { orig(vk) }
}

// --- DirectInput8 vtable detours ------------------------------------------------------------

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

type DirectInput8CreateFn = unsafe extern "system" fn(
    hinst: *mut c_void,
    dw_version: u32,
    riidltf: *const Guid,
    ppv_out: *mut *mut c_void,
    punk_outer: *mut c_void,
) -> i32;
type ComReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type DiCreateDeviceFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void, *mut c_void) -> i32;
type DiGetDeviceStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;
type DiGetDeviceDataFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32, u32) -> i32;

const DIRECTINPUT_VERSION: u32 = 0x0800;
const IID_IDIRECTINPUT8W: Guid = Guid {
    data1: 0xBF798031,
    data2: 0x483A,
    data3: 0x4DA2,
    data4: [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00],
};
const GUID_SYS_KEYBOARD: Guid = Guid {
    data1: 0x6F1D2B61,
    data2: 0xD5A0,
    data3: 0x11CF,
    data4: [0xBF, 0xC7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00],
};

static ORIG_DI_GET_DEVICE_STATE: AtomicUsize = AtomicUsize::new(0);
static ORIG_DI_GET_DEVICE_DATA: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn di_get_device_state_detour(this: *mut c_void, cb_data: u32, lpv_data: *mut c_void) -> i32 {
    let addr = ORIG_DI_GET_DEVICE_STATE.load(Ordering::Acquire);
    if addr == 0 {
        return -1;
    }
    let orig: DiGetDeviceStateFn = unsafe { std::mem::transmute(addr) };
    let hr = unsafe { orig(this, cb_data, lpv_data) };
    if hr == 0 && !lpv_data.is_null() && blocked() {
        match cb_data {
            // DIMOUSESTATE (16), DIMOUSESTATE2 (20), or Keyboard (256)
            16 | 20 | 256 => unsafe { ptr::write_bytes(lpv_data as *mut u8, 0, cb_data as usize) },
            // DIJOYSTATE (80): rgdwPOV[0..4] at +32..48 must be 0xFFFF_FFFF (centered, not 0=up!)
            80 => unsafe {
                let pov = (lpv_data as *mut u8).add(32) as *mut u32;
                for i in 0..4 {
                    pov.add(i).write_unaligned(0xFFFF_FFFF);
                }
                ptr::write_bytes((lpv_data as *mut u8).add(48), 0, 32);
            },
            // DIJOYSTATE2 (272): rgdwPOV[0..4] at +32..48 centered, rgbButtons[128] at +48..176 zeroed
            272 => unsafe {
                let pov = (lpv_data as *mut u8).add(32) as *mut u32;
                for i in 0..4 {
                    pov.add(i).write_unaligned(0xFFFF_FFFF);
                }
                ptr::write_bytes((lpv_data as *mut u8).add(48), 0, 128);
            },
            _ => {}
        }
    }
    hr
}

unsafe extern "system" fn di_get_device_data_detour(
    this: *mut c_void,
    cb_object_data: u32,
    rgdod: *mut c_void,
    pdw_in_out: *mut u32,
    dw_flags: u32,
) -> i32 {
    let addr = ORIG_DI_GET_DEVICE_DATA.load(Ordering::Acquire);
    if addr == 0 {
        return -1;
    }
    let orig: DiGetDeviceDataFn = unsafe { std::mem::transmute(addr) };
    // Always call original so buffered events drain rather than replaying when dialog closes.
    let hr = unsafe { orig(this, cb_object_data, rgdod, pdw_in_out, dw_flags) };
    if hr == 0 && !pdw_in_out.is_null() && blocked() {
        unsafe { *pdw_in_out = 0 };
    }
    hr
}

// --- Installer helpers ----------------------------------------------------------------------

unsafe fn create_and_enable_hook(
    target: *mut c_void,
    detour: *mut c_void,
    orig_store: &AtomicUsize,
    seen: &mut Vec<usize>,
) -> bool {
    let addr = target as usize;
    if addr == 0 || seen.contains(&addr) {
        return false;
    }
    let mut trampoline = null_mut();
    let st = unsafe { MH_CreateHook(target, detour, &mut trampoline) };
    if !matches!(st, MH_STATUS::MH_OK | MH_STATUS::MH_ERROR_ALREADY_CREATED) {
        return false;
    }
    orig_store.store(trampoline as usize, Ordering::Release);
    let en = unsafe { MH_EnableHook(target) };
    if matches!(en, MH_STATUS::MH_OK | MH_STATUS::MH_ERROR_ENABLED) {
        seen.push(addr);
        true
    } else {
        false
    }
}

unsafe fn com_vtable_slot(obj: *mut c_void, index: usize) -> *mut c_void {
    let vtable = unsafe { *(obj as *const *const *mut c_void) };
    unsafe { *vtable.add(index) }
}

unsafe fn install_dinput8_hooks(seen: &mut Vec<usize>) -> usize {
    let w = wide_str("dinput8.dll");
    let hmod = unsafe { GetModuleHandleW(w.as_ptr()) };
    if hmod.is_null() {
        return 0;
    }
    let p_create = unsafe { GetProcAddress(hmod, c"DirectInput8Create".as_ptr() as *const u8) };
    if p_create.is_null() {
        return 0;
    }
    let create: DirectInput8CreateFn = unsafe { std::mem::transmute(p_create) };
    let hinst = unsafe { GetModuleHandleW(null()) };
    let mut di: *mut c_void = null_mut();
    let hr = unsafe { create(hinst, DIRECTINPUT_VERSION, &IID_IDIRECTINPUT8W, &mut di, null_mut()) };
    if hr != 0 || di.is_null() {
        return 0;
    }
    let create_dev: DiCreateDeviceFn = unsafe { std::mem::transmute(com_vtable_slot(di, 3)) };
    let mut dev: *mut c_void = null_mut();
    let hr_dev = unsafe { create_dev(di, &GUID_SYS_KEYBOARD, &mut dev, null_mut()) };
    let mut state_slot = null_mut();
    let mut data_slot = null_mut();
    if hr_dev == 0 && !dev.is_null() {
        state_slot = unsafe { com_vtable_slot(dev, 9) };
        data_slot = unsafe { com_vtable_slot(dev, 10) };
        let rel_dev: ComReleaseFn = unsafe { std::mem::transmute(com_vtable_slot(dev, 2)) };
        unsafe { rel_dev(dev) };
    }
    let rel_di: ComReleaseFn = unsafe { std::mem::transmute(com_vtable_slot(di, 2)) };
    unsafe { rel_di(di) };

    let mut count = 0usize;
    if !state_slot.is_null()
        && unsafe {
            create_and_enable_hook(
                state_slot,
                di_get_device_state_detour as *const () as *mut c_void,
                &ORIG_DI_GET_DEVICE_STATE,
                seen,
            )
        }
    {
        count += 1;
    }
    if !data_slot.is_null()
        && unsafe {
            create_and_enable_hook(
                data_slot,
                di_get_device_data_detour as *const () as *mut c_void,
                &ORIG_DI_GET_DEVICE_DATA,
                seen,
            )
        }
    {
        count += 1;
    }
    count
}

/// Installs Win32 / XInput / DirectInput hooks once so modal UI dialogs can pause Sekiro's
/// background navigation safely. Fails soft on any error so it never blocks game startup.
pub fn install() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let st = unsafe { MH_Initialize() };
    if !matches!(st, MH_STATUS::MH_OK | MH_STATUS::MH_ERROR_ALREADY_INITIALIZED) {
        log::line(&format!("inputblock: MH_Initialize failed: {st:?}"));
        return;
    }

    let mut seen: Vec<usize> = Vec::new();
    let mut xinput_count = 0usize;
    for dll in ["xinput1_3.dll", "xinput1_4.dll", "xinput9_1_0.dll"] {
        let w = wide_str(dll);
        let hmod = unsafe { GetModuleHandleW(w.as_ptr()) };
        if hmod.is_null() {
            continue;
        }
        let targets = [
            unsafe { GetProcAddress(hmod, c"XInputGetState".as_ptr() as *const u8) },
            // Ordinal 100 = XInputGetStateEx (also reports the Guide button on xinput1_3/1_4).
            unsafe { GetProcAddress(hmod, 100usize as *const u8) },
        ];
        for target in targets {
            if xinput_count >= MAX_XINPUT_SLOTS {
                break;
            }
            if unsafe {
                create_and_enable_hook(
                    target,
                    XINPUT_DETOURS[xinput_count] as *const () as *mut c_void,
                    &ORIG_XINPUT[xinput_count],
                    &mut seen,
                )
            } {
                xinput_count += 1;
            }
        }
    }

    let mut user32_count = 0usize;
    let user32_w = wide_str("user32.dll");
    let user32 = unsafe { GetModuleHandleW(user32_w.as_ptr()) };
    if !user32.is_null() {
        let p_kb = unsafe { GetProcAddress(user32, c"GetKeyboardState".as_ptr() as *const u8) };
        if unsafe {
            create_and_enable_hook(
                p_kb,
                get_keyboard_state_detour as *const () as *mut c_void,
                &ORIG_GET_KEYBOARD_STATE,
                &mut seen,
            )
        } {
            user32_count += 1;
        }
        let p_async = unsafe { GetProcAddress(user32, c"GetAsyncKeyState".as_ptr() as *const u8) };
        if unsafe {
            create_and_enable_hook(
                p_async,
                get_async_key_state_detour as *const () as *mut c_void,
                &ORIG_GET_ASYNC_KEY_STATE,
                &mut seen,
            )
        } {
            user32_count += 1;
        }
    }

    let dinput_count = unsafe { install_dinput8_hooks(&mut seen) };
    log::line(&format!(
        "inputblock: installed (xinput={xinput_count}, user32={user32_count}, dinput8={dinput_count})"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_window_covers_dialog_and_soak_only() {
        assert!(!should_block_at(false, 0, 100));
        assert!(should_block_at(true, 0, 100));
        // Released at t=500 -> deadline = 720
        let deadline = soak_deadline(500, CLOSE_SOAK_MS);
        assert_eq!(deadline, 720);
        assert!(should_block_at(false, deadline, 500));
        assert!(should_block_at(false, deadline, 719));
        assert!(!should_block_at(false, deadline, 720));
        assert!(!should_block_at(false, deadline, 1000));
    }

    #[test]
    fn soak_deadline_saturates_on_max() {
        assert_eq!(soak_deadline(u64::MAX - 10, 500), u64::MAX);
    }
}
