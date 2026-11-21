//! Redirects Sekiro's save file (`S0000.sl2`) to `S0000.co2` so vanilla and
//! co-op saves stay completely separate without needing `me3`.
//!
//! `coop_sekiro_launcher.exe` injects this DLL directly into `sekiro.exe`, so we:
//! 1. Redirect `S0000.sl2*` -> `S0000.co2*` in Win32 file/find APIs without ever
//!    reading or copying vanilla `S0000.sl2` (fresh co-op saves start clean).
//! 2. Patch `sekiro.exe`'s PE Import Address Table (both the import directory and
//!    SteamStub's unpacked `.rdata`/`.idata`/`.data` IAT slots) so `sekiro.exe`'s
//!    save I/O targets `S0000.co2*` transparently while our DLL keeps direct access.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::{game, log};

const VANILLA_SAVE: &str = "S0000.sl2";
const COOP_SAVE: &str = "S0000.co2";
const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;
const PAGE_READWRITE: u32 = 0x04;
const MEM_COMMIT: u32 = 0x1000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_GUARD: u32 = 0x100;
const IMAGE_SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;
const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
const IMAGE_ORDINAL_FLAG64: u64 = 1u64 << 63;
const CP_ACP: u32 = 0;

#[repr(C)]
struct MemoryBasicInformation {
    base_address: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: u32,
    partition_id: u16,
    region_size: usize,
    state: u32,
    protect: u32,
    type_: u32,
}

const _: () = assert!(size_of::<MemoryBasicInformation>() == 48);

#[repr(C)]
pub struct Win32FindDataW {
    pub dw_file_attributes: u32,
    pub ft_creation_time: [u32; 2],
    pub ft_last_access_time: [u32; 2],
    pub ft_last_write_time: [u32; 2],
    pub n_file_size_high: u32,
    pub n_file_size_low: u32,
    pub dw_reserved0: u32,
    pub dw_reserved1: u32,
    pub c_file_name: [u16; 260],
    pub c_alternate_file_name: [u16; 14],
}

const _: () = assert!(std::mem::offset_of!(Win32FindDataW, c_file_name) == 44);
const _: () = assert!(size_of::<Win32FindDataW>() == 592);

#[repr(C)]
#[derive(Clone, Copy)]
struct ImageImportDescriptor {
    original_first_thunk: u32,
    time_date_stamp: u32,
    forwarder_chain: u32,
    name: u32,
    first_thunk: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualProtect(addr: *mut c_void, size: usize, new_protect: u32, old_protect: *mut u32) -> i32;
    fn VirtualQuery(addr: *const c_void, buf: *mut MemoryBasicInformation, len: usize) -> usize;
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    fn MultiByteToWideChar(
        code_page: u32,
        flags: u32,
        src: *const u8,
        src_len: i32,
        dst: *mut u16,
        dst_len: i32,
    ) -> i32;
}

static ORIG_CREATE_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_CREATE_FILE_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_GET_FILE_ATTRS_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_GET_FILE_ATTRS_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_GET_FILE_ATTRS_EX_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_GET_FILE_ATTRS_EX_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_DELETE_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_DELETE_FILE_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_MOVE_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_MOVE_FILE_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_MOVE_FILE_EX_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_MOVE_FILE_EX_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_COPY_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_COPY_FILE_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_COPY_FILE_EX_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_REPLACE_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_FIND_FIRST_FILE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_FIND_FIRST_FILE_EX_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_FIND_NEXT_FILE_W: AtomicUsize = AtomicUsize::new(0);

static LOGGED_OPS: AtomicUsize = AtomicUsize::new(0);
static SECTION_HOOKED: AtomicBool = AtomicBool::new(false);
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);
static IAT_LOCK: Mutex<()> = Mutex::new(());

fn log_redirect_once(op: &str, path_hint: &str) {
    let n = LOGGED_OPS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        log::line(&format!("savefile: redirected {op} -> {path_hint}"));
    }
}

/// If `path` ends with a filename `S0000.sl2` or `S0000.sl2.<suffix>` (case-insensitive),
/// returns the rewritten path targeting `S0000.co2<suffix>`.
/// Paths already targeting `S0000.co2` return `None`.
pub fn redirect_path_str(path: &str) -> Option<String> {
    let split = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
    let (prefix, file) = path.split_at(split);
    let lower = file.to_ascii_lowercase();
    if lower == "s0000.co2" || lower.starts_with("s0000.co2.") {
        return None;
    }
    if lower == "s0000.sl2" || lower.starts_with("s0000.sl2.") {
        let suffix = &file[VANILLA_SAVE.len()..];
        let mut out = String::with_capacity(prefix.len() + COOP_SAVE.len() + suffix.len());
        out.push_str(prefix);
        out.push_str(COOP_SAVE);
        out.push_str(suffix);
        return Some(out);
    }
    None
}

/// Byte-level equivalent of `redirect_path_str` for `*A` ANSI APIs so non-ASCII
/// Windows ACP codepage directory names stay intact.
pub fn redirect_path_bytes(path: &[u8]) -> Option<Vec<u8>> {
    let split = path
        .iter()
        .rposition(|&b| b == b'\\' || b == b'/')
        .map(|i| i + 1)
        .unwrap_or(0);
    let (prefix, file) = path.split_at(split);
    if file.eq_ignore_ascii_case(b"s0000.co2")
        || (file.len() > 9 && file[..10].eq_ignore_ascii_case(b"s0000.co2."))
    {
        return None;
    }
    if file.eq_ignore_ascii_case(b"s0000.sl2")
        || (file.len() > 9 && file[..10].eq_ignore_ascii_case(b"s0000.sl2."))
    {
        let suffix = &file[VANILLA_SAVE.len()..];
        let mut out = Vec::with_capacity(prefix.len() + COOP_SAVE.len() + suffix.len() + 1);
        out.extend_from_slice(prefix);
        out.extend_from_slice(COOP_SAVE.as_bytes());
        out.extend_from_slice(suffix);
        return Some(out);
    }
    None
}

/// Reverses `S0000.co2*` back to `S0000.sl2*` inside `WIN32_FIND_DATAW::cFileName`.
pub fn unredirect_find_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower == "s0000.co2" || lower.starts_with("s0000.co2.") {
        let suffix = &name[COOP_SAVE.len()..];
        return Some(format!("{VANILLA_SAVE}{suffix}"));
    }
    None
}

unsafe fn wide_ptr_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    unsafe {
        while len < 32768 && *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        Some(String::from_utf16_lossy(slice))
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn redirect_wide(ptr: *const u16, op: &str) -> Option<Vec<u16>> {
    let s = unsafe { wide_ptr_to_string(ptr)? };
    let redirected = redirect_path_str(&s)?;
    log_redirect_once(op, &redirected);
    Some(to_wide_nul(&redirected))
}

unsafe fn acp_bytes_to_string(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    let needed = unsafe {
        MultiByteToWideChar(
            CP_ACP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return None;
    }
    let mut wide = vec![0u16; needed as usize];
    let got = unsafe {
        MultiByteToWideChar(
            CP_ACP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            needed,
        )
    };
    (got > 0).then(|| String::from_utf16_lossy(&wide[..got as usize]))
}

unsafe fn redirect_ansi(ptr: *const u8, op: &str) -> Option<Vec<u8>> {
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(ptr.cast()).to_bytes() };
    let mut out = redirect_path_bytes(bytes)?;
    if let Some(path_str) = unsafe { acp_bytes_to_string(&out) } {
        log_redirect_once(op, &path_str);
    } else {
        log_redirect_once(op, "(ansi)");
    }
    out.push(0);
    Some(out)
}

unsafe fn unredirect_find_data(data: *mut Win32FindDataW) {
    if data.is_null() {
        return;
    }
    unsafe {
        let name_buf = &mut (*data).c_file_name;
        let len = name_buf.iter().position(|&c| c == 0).unwrap_or(name_buf.len());
        let name = String::from_utf16_lossy(&name_buf[..len]);
        let Some(unred) = unredirect_find_name(&name) else {
            return;
        };
        let encoded: Vec<u16> = unred.encode_utf16().collect();
        if encoded.len() < name_buf.len() {
            name_buf[..encoded.len()].copy_from_slice(&encoded);
            name_buf[encoded.len()] = 0;
            (*data).c_alternate_file_name[0] = 0;
        }
    }
}

unsafe extern "system" fn hook_create_file_w(
    file_name: *const u16,
    desired_access: u32,
    share_mode: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: *mut c_void,
) -> *mut c_void {
    type Fn = unsafe extern "system" fn(
        *const u16,
        u32,
        u32,
        *const c_void,
        u32,
        u32,
        *mut c_void,
    ) -> *mut c_void;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_CREATE_FILE_W.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_wide(file_name, "CreateFileW") } {
        return unsafe { orig(red.as_ptr(), desired_access, share_mode, security, disposition, flags, template) };
    }
    unsafe { orig(file_name, desired_access, share_mode, security, disposition, flags, template) }
}

unsafe extern "system" fn hook_create_file_a(
    file_name: *const u8,
    desired_access: u32,
    share_mode: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: *mut c_void,
) -> *mut c_void {
    type Fn = unsafe extern "system" fn(
        *const u8,
        u32,
        u32,
        *const c_void,
        u32,
        u32,
        *mut c_void,
    ) -> *mut c_void;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_CREATE_FILE_A.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_ansi(file_name, "CreateFileA") } {
        return unsafe { orig(red.as_ptr(), desired_access, share_mode, security, disposition, flags, template) };
    }
    unsafe { orig(file_name, desired_access, share_mode, security, disposition, flags, template) }
}

unsafe extern "system" fn hook_get_file_attributes_w(file_name: *const u16) -> u32 {
    type Fn = unsafe extern "system" fn(*const u16) -> u32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_GET_FILE_ATTRS_W.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_wide(file_name, "GetFileAttributesW") } {
        return unsafe { orig(red.as_ptr()) };
    }
    unsafe { orig(file_name) }
}

unsafe extern "system" fn hook_get_file_attributes_a(file_name: *const u8) -> u32 {
    type Fn = unsafe extern "system" fn(*const u8) -> u32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_GET_FILE_ATTRS_A.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_ansi(file_name, "GetFileAttributesA") } {
        return unsafe { orig(red.as_ptr()) };
    }
    unsafe { orig(file_name) }
}

unsafe extern "system" fn hook_get_file_attributes_ex_w(
    file_name: *const u16,
    info_level: i32,
    file_info: *mut c_void,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u16, i32, *mut c_void) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_GET_FILE_ATTRS_EX_W.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_wide(file_name, "GetFileAttributesExW") } {
        return unsafe { orig(red.as_ptr(), info_level, file_info) };
    }
    unsafe { orig(file_name, info_level, file_info) }
}

unsafe extern "system" fn hook_get_file_attributes_ex_a(
    file_name: *const u8,
    info_level: i32,
    file_info: *mut c_void,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u8, i32, *mut c_void) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_GET_FILE_ATTRS_EX_A.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_ansi(file_name, "GetFileAttributesExA") } {
        return unsafe { orig(red.as_ptr(), info_level, file_info) };
    }
    unsafe { orig(file_name, info_level, file_info) }
}

unsafe extern "system" fn hook_delete_file_w(file_name: *const u16) -> i32 {
    type Fn = unsafe extern "system" fn(*const u16) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_DELETE_FILE_W.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_wide(file_name, "DeleteFileW") } {
        return unsafe { orig(red.as_ptr()) };
    }
    unsafe { orig(file_name) }
}

unsafe extern "system" fn hook_delete_file_a(file_name: *const u8) -> i32 {
    type Fn = unsafe extern "system" fn(*const u8) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_DELETE_FILE_A.load(Ordering::Acquire)) };
    if let Some(red) = unsafe { redirect_ansi(file_name, "DeleteFileA") } {
        return unsafe { orig(red.as_ptr()) };
    }
    unsafe { orig(file_name) }
}

unsafe extern "system" fn hook_move_file_w(existing: *const u16, new: *const u16) -> i32 {
    type Fn = unsafe extern "system" fn(*const u16, *const u16) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_MOVE_FILE_W.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_wide(existing, "MoveFileW") };
    let r2 = unsafe { redirect_wide(new, "MoveFileW") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2) }
}

unsafe extern "system" fn hook_move_file_a(existing: *const u8, new: *const u8) -> i32 {
    type Fn = unsafe extern "system" fn(*const u8, *const u8) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_MOVE_FILE_A.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_ansi(existing, "MoveFileA") };
    let r2 = unsafe { redirect_ansi(new, "MoveFileA") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2) }
}

unsafe extern "system" fn hook_move_file_ex_w(
    existing: *const u16,
    new: *const u16,
    flags: u32,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u16, *const u16, u32) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_MOVE_FILE_EX_W.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_wide(existing, "MoveFileExW") };
    let r2 = unsafe { redirect_wide(new, "MoveFileExW") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2, flags) }
}

unsafe extern "system" fn hook_move_file_ex_a(
    existing: *const u8,
    new: *const u8,
    flags: u32,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u8, *const u8, u32) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_MOVE_FILE_EX_A.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_ansi(existing, "MoveFileExA") };
    let r2 = unsafe { redirect_ansi(new, "MoveFileExA") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2, flags) }
}

unsafe extern "system" fn hook_copy_file_w(
    existing: *const u16,
    new: *const u16,
    fail_if_exists: i32,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u16, *const u16, i32) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_COPY_FILE_W.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_wide(existing, "CopyFileW") };
    let r2 = unsafe { redirect_wide(new, "CopyFileW") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2, fail_if_exists) }
}

unsafe extern "system" fn hook_copy_file_a(
    existing: *const u8,
    new: *const u8,
    fail_if_exists: i32,
) -> i32 {
    type Fn = unsafe extern "system" fn(*const u8, *const u8, i32) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_COPY_FILE_A.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_ansi(existing, "CopyFileA") };
    let r2 = unsafe { redirect_ansi(new, "CopyFileA") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2, fail_if_exists) }
}

unsafe extern "system" fn hook_copy_file_ex_w(
    existing: *const u16,
    new: *const u16,
    progress: *const c_void,
    data: *const c_void,
    cancel: *mut i32,
    flags: u32,
) -> i32 {
    type Fn = unsafe extern "system" fn(
        *const u16,
        *const u16,
        *const c_void,
        *const c_void,
        *mut i32,
        u32,
    ) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_COPY_FILE_EX_W.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_wide(existing, "CopyFileExW") };
    let r2 = unsafe { redirect_wide(new, "CopyFileExW") };
    let p1 = r1.as_ref().map_or(existing, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(new, |v| v.as_ptr());
    unsafe { orig(p1, p2, progress, data, cancel, flags) }
}

unsafe extern "system" fn hook_replace_file_w(
    replaced: *const u16,
    replacement: *const u16,
    backup: *const u16,
    flags: u32,
    exclude: *mut c_void,
    reserved: *mut c_void,
) -> i32 {
    type Fn = unsafe extern "system" fn(
        *const u16,
        *const u16,
        *const u16,
        u32,
        *mut c_void,
        *mut c_void,
    ) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_REPLACE_FILE_W.load(Ordering::Acquire)) };
    let r1 = unsafe { redirect_wide(replaced, "ReplaceFileW") };
    let r2 = unsafe { redirect_wide(replacement, "ReplaceFileW") };
    let r3 = unsafe { redirect_wide(backup, "ReplaceFileW") };
    let p1 = r1.as_ref().map_or(replaced, |v| v.as_ptr());
    let p2 = r2.as_ref().map_or(replacement, |v| v.as_ptr());
    let p3 = r3.as_ref().map_or(backup, |v| v.as_ptr());
    unsafe { orig(p1, p2, p3, flags, exclude, reserved) }
}

unsafe extern "system" fn hook_find_first_file_w(
    file_name: *const u16,
    find_data: *mut Win32FindDataW,
) -> *mut c_void {
    type Fn = unsafe extern "system" fn(*const u16, *mut Win32FindDataW) -> *mut c_void;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_FIND_FIRST_FILE_W.load(Ordering::Acquire)) };
    let red = unsafe { redirect_wide(file_name, "FindFirstFileW") };
    let p = red.as_ref().map_or(file_name, |v| v.as_ptr());
    let handle = unsafe { orig(p, find_data) };
    if !handle.is_null() && handle != INVALID_HANDLE {
        unsafe { unredirect_find_data(find_data) };
    }
    handle
}

unsafe extern "system" fn hook_find_first_file_ex_w(
    file_name: *const u16,
    info_level: i32,
    find_data: *mut Win32FindDataW,
    search_op: i32,
    search_filter: *const c_void,
    additional_flags: u32,
) -> *mut c_void {
    type Fn = unsafe extern "system" fn(
        *const u16,
        i32,
        *mut Win32FindDataW,
        i32,
        *const c_void,
        u32,
    ) -> *mut c_void;
    let orig: Fn =
        unsafe { std::mem::transmute(ORIG_FIND_FIRST_FILE_EX_W.load(Ordering::Acquire)) };
    let red = unsafe { redirect_wide(file_name, "FindFirstFileExW") };
    let p = red.as_ref().map_or(file_name, |v| v.as_ptr());
    let handle = unsafe { orig(p, info_level, find_data, search_op, search_filter, additional_flags) };
    if !handle.is_null() && handle != INVALID_HANDLE {
        unsafe { unredirect_find_data(find_data) };
    }
    handle
}

unsafe extern "system" fn hook_find_next_file_w(
    find_handle: *mut c_void,
    find_data: *mut Win32FindDataW,
) -> i32 {
    type Fn = unsafe extern "system" fn(*mut c_void, *mut Win32FindDataW) -> i32;
    let orig: Fn = unsafe { std::mem::transmute(ORIG_FIND_NEXT_FILE_W.load(Ordering::Acquire)) };
    let ok = unsafe { orig(find_handle, find_data) };
    if ok != 0 {
        unsafe { unredirect_find_data(find_data) };
    }
    ok
}

struct IatTarget {
    name: &'static str,
    c_name: &'static [u8],
    hook_addr: usize,
    orig_slot: &'static AtomicUsize,
}

fn targets() -> [IatTarget; 19] {
    [
        IatTarget { name: "CreateFileW", c_name: b"CreateFileW\0", hook_addr: hook_create_file_w as *const () as usize, orig_slot: &ORIG_CREATE_FILE_W },
        IatTarget { name: "CreateFileA", c_name: b"CreateFileA\0", hook_addr: hook_create_file_a as *const () as usize, orig_slot: &ORIG_CREATE_FILE_A },
        IatTarget { name: "GetFileAttributesW", c_name: b"GetFileAttributesW\0", hook_addr: hook_get_file_attributes_w as *const () as usize, orig_slot: &ORIG_GET_FILE_ATTRS_W },
        IatTarget { name: "GetFileAttributesA", c_name: b"GetFileAttributesA\0", hook_addr: hook_get_file_attributes_a as *const () as usize, orig_slot: &ORIG_GET_FILE_ATTRS_A },
        IatTarget { name: "GetFileAttributesExW", c_name: b"GetFileAttributesExW\0", hook_addr: hook_get_file_attributes_ex_w as *const () as usize, orig_slot: &ORIG_GET_FILE_ATTRS_EX_W },
        IatTarget { name: "GetFileAttributesExA", c_name: b"GetFileAttributesExA\0", hook_addr: hook_get_file_attributes_ex_a as *const () as usize, orig_slot: &ORIG_GET_FILE_ATTRS_EX_A },
        IatTarget { name: "DeleteFileW", c_name: b"DeleteFileW\0", hook_addr: hook_delete_file_w as *const () as usize, orig_slot: &ORIG_DELETE_FILE_W },
        IatTarget { name: "DeleteFileA", c_name: b"DeleteFileA\0", hook_addr: hook_delete_file_a as *const () as usize, orig_slot: &ORIG_DELETE_FILE_A },
        IatTarget { name: "MoveFileW", c_name: b"MoveFileW\0", hook_addr: hook_move_file_w as *const () as usize, orig_slot: &ORIG_MOVE_FILE_W },
        IatTarget { name: "MoveFileA", c_name: b"MoveFileA\0", hook_addr: hook_move_file_a as *const () as usize, orig_slot: &ORIG_MOVE_FILE_A },
        IatTarget { name: "MoveFileExW", c_name: b"MoveFileExW\0", hook_addr: hook_move_file_ex_w as *const () as usize, orig_slot: &ORIG_MOVE_FILE_EX_W },
        IatTarget { name: "MoveFileExA", c_name: b"MoveFileExA\0", hook_addr: hook_move_file_ex_a as *const () as usize, orig_slot: &ORIG_MOVE_FILE_EX_A },
        IatTarget { name: "CopyFileW", c_name: b"CopyFileW\0", hook_addr: hook_copy_file_w as *const () as usize, orig_slot: &ORIG_COPY_FILE_W },
        IatTarget { name: "CopyFileA", c_name: b"CopyFileA\0", hook_addr: hook_copy_file_a as *const () as usize, orig_slot: &ORIG_COPY_FILE_A },
        IatTarget { name: "CopyFileExW", c_name: b"CopyFileExW\0", hook_addr: hook_copy_file_ex_w as *const () as usize, orig_slot: &ORIG_COPY_FILE_EX_W },
        IatTarget { name: "ReplaceFileW", c_name: b"ReplaceFileW\0", hook_addr: hook_replace_file_w as *const () as usize, orig_slot: &ORIG_REPLACE_FILE_W },
        IatTarget { name: "FindFirstFileW", c_name: b"FindFirstFileW\0", hook_addr: hook_find_first_file_w as *const () as usize, orig_slot: &ORIG_FIND_FIRST_FILE_W },
        IatTarget { name: "FindFirstFileExW", c_name: b"FindFirstFileExW\0", hook_addr: hook_find_first_file_ex_w as *const () as usize, orig_slot: &ORIG_FIND_FIRST_FILE_EX_W },
        IatTarget { name: "FindNextFileW", c_name: b"FindNextFileW\0", hook_addr: hook_find_next_file_w as *const () as usize, orig_slot: &ORIG_FIND_NEXT_FILE_W },
    ]
}

fn is_file_import_dll(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "kernel32.dll"
        || lower == "kernelbase.dll"
        || lower.starts_with("api-ms-win-core-file-")
}

unsafe fn patch_iat_slot(slot: *mut usize, target: &IatTarget) -> bool {
    let current = unsafe { *slot };
    if current == 0 || current == target.hook_addr {
        return false;
    }
    let mut old_protect = 0u32;
    let ok = unsafe { VirtualProtect(slot.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old_protect) };
    if ok == 0 {
        return false;
    }
    target.orig_slot.store(current, Ordering::Release);
    unsafe { *slot = target.hook_addr };
    let mut tmp = 0u32;
    unsafe { VirtualProtect(slot.cast(), size_of::<usize>(), old_protect, &mut tmp) };
    true
}

fn resolve_kernel_exports(targets: &[IatTarget]) -> Vec<(usize, &IatTarget)> {
    let mut out = Vec::new();
    for dll in [b"kernel32.dll\0".as_slice(), b"kernelbase.dll\0".as_slice()] {
        let h = unsafe { GetModuleHandleA(dll.as_ptr()) };
        if h.is_null() {
            continue;
        }
        for t in targets {
            let p = unsafe { GetProcAddress(h, t.c_name.as_ptr()) } as usize;
            if p != 0 {
                out.push((p, t));
            }
        }
    }
    out
}

fn is_readable_prot(protect: u32) -> bool {
    (protect & (PAGE_NOACCESS | PAGE_GUARD)) == 0 && (protect & 0xEE) != 0
}

fn scan_readable_sections_for_iat(
    base: usize,
    nt: usize,
    opt: usize,
    size_of_image: usize,
    targets: &[IatTarget],
    kernel_exports: &[(usize, &IatTarget)],
    hooked_names: &mut Vec<&'static str>,
) -> (usize, usize) {
    let Some(num_sections) = game::read::<u16>(nt + 6) else {
        return (0, 0);
    };
    let Some(size_of_opt) = game::read::<u16>(nt + 20) else {
        return (0, 0);
    };
    let sec_base = opt + size_of_opt as usize;
    let align = size_of::<usize>();
    let mut min_addr = usize::MAX;
    let mut max_addr = 0usize;
    for &(exp, _) in kernel_exports {
        min_addr = min_addr.min(exp);
        max_addr = max_addr.max(exp);
    }
    for t in targets {
        min_addr = min_addr.min(t.hook_addr);
        max_addr = max_addr.max(t.hook_addr);
    }
    if max_addr == 0 {
        return (0, 0);
    }
    let mut patched = 0usize;
    let mut already = 0usize;

    for i in 0..num_sections as usize {
        let sh = sec_base + i * 40;
        let Some(name_bytes) = game::read::<[u8; 8]>(sh) else {
            break;
        };
        if name_bytes.starts_with(b".rsrc")
            || name_bytes.starts_with(b".reloc")
            || name_bytes.starts_with(b".pdata")
        {
            continue;
        }
        let Some(vsize) = game::read::<u32>(sh + 8) else {
            break;
        };
        let Some(va) = game::read::<u32>(sh + 12) else {
            break;
        };
        let Some(raw_size) = game::read::<u32>(sh + 16) else {
            break;
        };
        let Some(chars) = game::read::<u32>(sh + 36) else {
            break;
        };

        if (chars & IMAGE_SCN_MEM_READ) == 0
            || (chars & IMAGE_SCN_MEM_EXECUTE) != 0
            || (chars & IMAGE_SCN_MEM_DISCARDABLE) != 0
        {
            continue;
        }
        let va = va as usize;
        let span = (vsize.max(raw_size) as usize).min(size_of_image.saturating_sub(va));
        if va == 0 || span < align {
            continue;
        }

        let mut cursor = base + va;
        let sec_end = cursor + span;
        while cursor < sec_end {
            let mut mbi: MemoryBasicInformation = unsafe { std::mem::zeroed() };
            let queried = unsafe {
                VirtualQuery(
                    cursor as *const c_void,
                    &mut mbi,
                    size_of::<MemoryBasicInformation>(),
                )
            };
            if queried == 0 || mbi.region_size == 0 {
                break;
            }
            let reg_end = (mbi.base_address as usize)
                .saturating_add(mbi.region_size)
                .min(sec_end);
            if mbi.state == MEM_COMMIT && is_readable_prot(mbi.protect) {
                let mut addr = (cursor + (align - 1)) & !(align - 1);
                while addr + align <= reg_end {
                    let val = unsafe { *(addr as *const usize) };
                    if val >= min_addr && val <= max_addr {
                        if let Some(&(_, t)) = kernel_exports.iter().find(|(exp, _)| *exp == val) {
                            if unsafe { patch_iat_slot(addr as *mut usize, t) } {
                                patched += 1;
                                hooked_names.push(t.name);
                            }
                        } else if targets.iter().any(|t| t.hook_addr == val) {
                            already += 1;
                        }
                    }
                    addr += align;
                }
            }
            if reg_end <= cursor {
                break;
            }
            cursor = reg_end;
        }
    }

    (patched, already)
}

/// Scans `sekiro.exe`'s PE64 import directory and readable non-executable sections
/// (to catch SteamStub's unpacked `.rdata`/`.idata`/`.data` IAT) and hooks any matching
/// Win32 file/find IAT entries that are not already patched. Safe to call multiple times.
pub fn reinstall_iat() -> usize {
    let _guard = IAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = game::module_base();
    if base == 0 {
        return 0;
    }

    let Some(dos_magic) = game::read::<u16>(base) else { return 0 };
    if dos_magic != 0x5A4D {
        return 0;
    }
    let Some(lfanew) = game::read::<i32>(base + 0x3C) else { return 0 };
    if lfanew <= 0 {
        return 0;
    }
    let nt = base + lfanew as usize;
    let Some(pe_sig) = game::read::<u32>(nt) else { return 0 };
    if pe_sig != 0x0000_4550 {
        return 0;
    }
    let opt = nt + 24;
    let Some(opt_magic) = game::read::<u16>(opt) else { return 0 };
    if opt_magic != 0x020B {
        return 0;
    }
    let Some(size_of_image) = game::read::<u32>(opt + 0x38) else { return 0 };
    let Some(num_dirs) = game::read::<u32>(opt + 0x6C) else { return 0 };

    let targets = targets();
    let kernel_exports = resolve_kernel_exports(&targets);
    let mut patched = 0usize;
    let mut hooked_names = Vec::new();

    if num_dirs > 1 {
        let import_rva = game::read::<u32>(opt + 0x78).unwrap_or(0);
        let import_size = game::read::<u32>(opt + 0x7C).unwrap_or(0);
        if import_rva != 0 && import_size != 0 && import_rva < size_of_image {
            let mut desc_idx = 0usize;
            loop {
                let desc_rva = import_rva as usize + desc_idx * size_of::<ImageImportDescriptor>();
                if desc_rva + size_of::<ImageImportDescriptor>() > size_of_image as usize {
                    break;
                }
                let Some(desc) = game::read::<ImageImportDescriptor>(base + desc_rva) else {
                    break;
                };
                if desc.name == 0 || desc.first_thunk == 0 {
                    break;
                }
                desc_idx += 1;
                if desc.name >= size_of_image || desc.first_thunk >= size_of_image {
                    continue;
                }

                let dll_name = unsafe { CStr::from_ptr((base + desc.name as usize) as *const i8) }
                    .to_string_lossy();
                if !is_file_import_dll(&dll_name) {
                    continue;
                }

                let has_ilt =
                    desc.original_first_thunk != 0 && desc.original_first_thunk < size_of_image;
                let mut idx = 0usize;
                loop {
                    let iat_rva = desc.first_thunk as usize + idx * size_of::<usize>();
                    if iat_rva + size_of::<usize>() > size_of_image as usize {
                        break;
                    }
                    let slot = (base + iat_rva) as *mut usize;
                    let Some(iat_val) = game::read::<usize>(slot as usize) else {
                        break;
                    };
                    if iat_val == 0 {
                        break;
                    }

                    let mut matched: Option<&IatTarget> = None;
                    if has_ilt {
                        let ilt_rva =
                            desc.original_first_thunk as usize + idx * size_of::<u64>();
                        if let Some(thunk) = game::read::<u64>(base + ilt_rva) {
                            if thunk == 0 {
                                break;
                            }
                            if (thunk & IMAGE_ORDINAL_FLAG64) == 0 {
                                let name_rva = (thunk & 0x7FFF_FFFF) as usize + 2;
                                if name_rva < size_of_image as usize {
                                    let sym = unsafe {
                                        CStr::from_ptr((base + name_rva) as *const i8)
                                            .to_string_lossy()
                                    };
                                    matched = targets.iter().find(|t| t.name == sym.as_ref());
                                }
                            }
                        }
                    }
                    if matched.is_none() {
                        matched = kernel_exports
                            .iter()
                            .find(|(addr, _)| *addr == iat_val)
                            .map(|(_, t)| *t);
                    }

                    if let Some(t) = matched
                        && unsafe { patch_iat_slot(slot, t) }
                    {
                        patched += 1;
                        hooked_names.push(t.name);
                    }

                    idx += 1;
                }
            }
        }
    }

    let (sec_patched, sec_already) = scan_readable_sections_for_iat(
        base,
        nt,
        opt,
        size_of_image as usize,
        &targets,
        &kernel_exports,
        &mut hooked_names,
    );
    patched += sec_patched;
    if sec_patched > 0 || sec_already > 0 {
        SECTION_HOOKED.store(true, Ordering::Release);
    }

    if patched > 0 {
        log::line(&format!(
            "savefile: hooked {} IAT entries (sec={sec_patched}: {})",
            patched,
            hooked_names.join(", ")
        ));
    }
    patched
}

/// Hooks `sekiro.exe`'s IAT and watches for SteamStub unpacking so all save I/O redirects
/// cleanly to an isolated `S0000.co2` file without touching standalone `S0000.sl2`.
pub fn install() {
    let n = reinstall_iat();
    if n == 0 {
        log::line("savefile: initial IAT scan patched 0 entries (watching for SteamStub unpack)");
    }
    if !WATCHER_STARTED.swap(true, Ordering::AcqRel) {
        let _ = std::thread::Builder::new()
            .name("sekiro-coop-save-iat".into())
            .spawn(|| {
                let mut settle_polls = 0u32;
                for _ in 0..3000 {
                    std::thread::sleep(Duration::from_millis(5));
                    let _ = reinstall_iat();
                    if SECTION_HOOKED.load(Ordering::Acquire) {
                        settle_polls += 1;
                        if settle_polls >= 8 {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(45));
                    }
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_vanilla_save_paths() {
        assert_eq!(
            redirect_path_str(r"C:\Users\xClouD\AppData\Roaming\Sekiro\76561198000000000\S0000.sl2"),
            Some(r"C:\Users\xClouD\AppData\Roaming\Sekiro\76561198000000000\S0000.co2".to_string())
        );
        assert_eq!(
            redirect_path_str(r"\\?\C:\Users\a\AppData\Roaming\Sekiro\123\s0000.sl2.bak"),
            Some(r"\\?\C:\Users\a\AppData\Roaming\Sekiro\123\S0000.co2.bak".to_string())
        );
        assert_eq!(
            redirect_path_str("Sekiro/123/S0000.sl2.tmp"),
            Some("Sekiro/123/S0000.co2.tmp".to_string())
        );
        assert_eq!(redirect_path_str("S0000.sl2"), Some("S0000.co2".to_string()));
    }

    #[test]
    fn leaves_coop_and_other_paths_untouched() {
        assert_eq!(
            redirect_path_str(r"C:\Users\a\AppData\Roaming\Sekiro\123\S0000.co2"),
            None
        );
        assert_eq!(
            redirect_path_str(r"C:\Users\a\AppData\Roaming\Sekiro\123\S0000.co2.bak"),
            None
        );
        assert_eq!(
            redirect_path_str(r"C:\Users\a\AppData\Roaming\Sekiro\GraphicsConfig.xml"),
            None
        );
        assert_eq!(redirect_path_str(r"C:\S0000.sl2\something.txt"), None);
    }

    #[test]
    fn redirects_ansi_byte_paths() {
        let raw = b"C:\\Users\\xCl\xf8uD\\AppData\\Roaming\\Sekiro\\1\\S0000.sl2";
        let expected = b"C:\\Users\\xCl\xf8uD\\AppData\\Roaming\\Sekiro\\1\\S0000.co2";
        assert_eq!(redirect_path_bytes(raw).as_deref(), Some(expected.as_slice()));
        assert_eq!(redirect_path_bytes(expected), None);
    }

    #[test]
    fn unredirects_find_names() {
        assert_eq!(unredirect_find_name("S0000.co2"), Some("S0000.sl2".to_string()));
        assert_eq!(
            unredirect_find_name("s0000.co2.bak"),
            Some("S0000.sl2.bak".to_string())
        );
        assert_eq!(unredirect_find_name("S0000.sl2"), None);
    }
}
