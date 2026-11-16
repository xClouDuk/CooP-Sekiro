#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::fs;
use std::mem::{MaybeUninit, size_of};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

const DEFAULT_SETTINGS_INI: &str = include_str!("../../../ship-public/sekiro_coop_settings.ini");
const SEKIRO_APP_ID: &str = "814380";
const DLL_SUBFOLDER: &str = "CoopSekiro";
const DLL_NAME: &str = "sekiro_coop.dll";
const SETTINGS_NAME: &str = "sekiro_coop_settings.ini";
const GAME_EXE_NAME: &str = "sekiro.exe";

const CREATE_SUSPENDED: u32 = 0x0000_0004;
const MEM_COMMIT_RESERVE: u32 = 0x1000 | 0x2000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_READWRITE: u32 = 0x04;
const WAIT_OBJECT_0: u32 = 0x0000_0000;
const MB_OK: u32 = 0x0000_0000;
const MB_ICONERROR: u32 = 0x0000_0010;
const HKEY_CURRENT_USER: isize = -0x7FFF_FFFF;
const HKEY_LOCAL_MACHINE: isize = -0x7FFF_FFFE;
const KEY_READ: u32 = 0x20019;
const REG_SZ: u32 = 1;
const ERROR_ELEVATION_REQUIRED: u32 = 740;
const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: *mut c_void,
    h_std_output: *mut c_void,
    h_std_error: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    h_process: *mut c_void,
    h_thread: *mut c_void,
    dw_process_id: u32,
    dw_thread_id: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn SetEnvironmentVariableW(name: *const u16, value: *const u16) -> i32;
    fn GetFullPathNameW(
        file_name: *const u16,
        buffer_len: u32,
        buffer: *mut u16,
        file_part: *mut *mut u16,
    ) -> u32;
    fn CreateProcessW(
        app_name: *const u16,
        cmd_line: *mut u16,
        proc_attrs: *const c_void,
        thread_attrs: *const c_void,
        inherit_handles: i32,
        creation_flags: u32,
        environment: *const c_void,
        current_dir: *const u16,
        startup_info: *const StartupInfoW,
        proc_info: *mut ProcessInformation,
    ) -> i32;
    fn VirtualAllocEx(
        process: *mut c_void,
        addr: *mut c_void,
        size: usize,
        alloc_type: u32,
        protect: u32,
    ) -> *mut c_void;
    fn VirtualFreeEx(process: *mut c_void, addr: *mut c_void, size: usize, free_type: u32) -> i32;
    fn WriteProcessMemory(
        process: *mut c_void,
        base_addr: *mut c_void,
        buf: *const c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
    fn GetModuleHandleW(module_name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    fn CreateRemoteThread(
        process: *mut c_void,
        thread_attrs: *const c_void,
        stack_size: usize,
        start_addr: usize,
        param: *mut c_void,
        creation_flags: u32,
        thread_id: *mut u32,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    fn GetExitCodeThread(thread: *mut c_void, exit_code: *mut u32) -> i32;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    fn FormatMessageW(
        flags: u32,
        source: *const c_void,
        message_id: u32,
        language_id: u32,
        buffer: *mut u16,
        size: u32,
        args: *const c_void,
    ) -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        hkey: isize,
        sub_key: *const u16,
        options: u32,
        sam_desired: u32,
        result: *mut isize,
    ) -> i32;
    fn RegQueryValueExW(
        hkey: isize,
        value_name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        data_len: *mut u32,
    ) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
}

struct OwnedHandle(*mut c_void);

impl OwnedHandle {
    fn as_raw(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct SuspendedChild {
    process: OwnedHandle,
    thread: OwnedHandle,
    resumed: bool,
}

impl Drop for SuspendedChild {
    fn drop(&mut self) {
        if !self.resumed && !self.process.0.is_null() {
            unsafe { TerminateProcess(self.process.0, 1) };
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win32_err(code: u32) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            null(),
        )
    } as usize;
    if n > 0 {
        let msg = String::from_utf16_lossy(&buf[..n]).trim().to_string();
        format!("{msg} (error {code})")
    } else {
        format!("Windows error {code}")
    }
}

fn show_error(msg: &str) {
    let title = to_wide("CooP Sekiro Launcher");
    let body = to_wide(msg);
    unsafe { MessageBoxW(null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
}

fn abs_windows_path(p: &Path) -> Result<PathBuf, String> {
    let wide = to_wide(&p.to_string_lossy());
    let mut buf = vec![0u16; 4096];
    let n = unsafe { GetFullPathNameW(wide.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), null_mut()) }
        as usize;
    if n == 0 || n >= buf.len() {
        return Err(format!("Invalid path '{}': {}", p.display(), win32_err(unsafe { GetLastError() })));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Locates `sekiro_coop.dll` preferring `<launcher_dir>\CoopSekiro\sekiro_coop.dll`,
/// then `<launcher_dir>\sekiro_coop.dll`.
fn find_dll_path(launcher_dir: &Path) -> Option<PathBuf> {
    let in_subdir = launcher_dir.join(DLL_SUBFOLDER).join(DLL_NAME);
    if in_subdir.is_file() {
        return Some(in_subdir);
    }
    let adjacent = launcher_dir.join(DLL_NAME);
    if adjacent.is_file() {
        return Some(adjacent);
    }
    None
}

/// Writes default `sekiro_coop_settings.ini` beside `sekiro_coop.dll` if missing.
fn ensure_default_ini(dll_path: &Path) {
    let Some(dll_dir) = dll_path.parent() else {
        return;
    };
    let ini = dll_dir.join(SETTINGS_NAME);
    if !ini.exists() {
        let _ = fs::write(&ini, DEFAULT_SETTINGS_INI);
    }
}

fn read_reg_sz(root: isize, subkey: &str, value_name: &str) -> Option<String> {
    let sub_w = to_wide(subkey);
    let val_w = to_wide(value_name);
    let mut hkey: isize = 0;
    if unsafe { RegOpenKeyExW(root, sub_w.as_ptr(), 0, KEY_READ, &mut hkey) } != 0 {
        return None;
    }
    let mut kind = 0u32;
    let mut bytes = 0u32;
    let status = unsafe {
        RegQueryValueExW(hkey, val_w.as_ptr(), null_mut(), &mut kind, null_mut(), &mut bytes)
    };
    if status != 0 || kind != REG_SZ || bytes < 2 {
        unsafe { RegCloseKey(hkey) };
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status2 = unsafe {
        RegQueryValueExW(
            hkey,
            val_w.as_ptr(),
            null_mut(),
            &mut kind,
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    unsafe { RegCloseKey(hkey) };
    if status2 != 0 {
        return None;
    }
    let u16_len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..u16_len]))
}

/// Parses quoted VDF library paths from Steam's `libraryfolders.vdf`.
fn parse_vdf_library_paths(content: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // Matches `"path"   "D:\\SteamLibrary"` or legacy `"1"   "D:\\SteamLibrary"`
        if parts.len() >= 5 {
            let key = parts[1].trim();
            let val = parts[3].trim();
            let is_path_key = key.eq_ignore_ascii_case("path")
                || (!key.is_empty() && key.chars().all(|c| c.is_ascii_digit()));
            if is_path_key && (val.contains(':') || val.starts_with('/') || val.starts_with("\\\\")) {
                let unescaped = val.replace("\\\\", "\\");
                let p = PathBuf::from(unescaped);
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn find_steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let queries = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"),
        (HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath"),
    ];
    for (hive, key, val) in queries {
        if let Some(s) = read_reg_sz(hive, key, val) {
            let p = PathBuf::from(s.replace('/', "\\"));
            if p.exists() && !roots.contains(&p) {
                roots.push(p);
            }
        }
    }
    roots
}

fn find_sekiro_in_steam() -> Option<PathBuf> {
    for root in find_steam_roots() {
        let mut libs = vec![root.clone()];
        let vdf_path = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(text) = fs::read_to_string(vdf_path) {
            for p in parse_vdf_library_paths(&text) {
                if !libs.contains(&p) {
                    libs.push(p);
                }
            }
        }
        for lib in libs {
            let candidate = lib
                .join("steamapps")
                .join("common")
                .join("Sekiro")
                .join(GAME_EXE_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locates `sekiro.exe` next to the launcher, in the parent folder, or via Steam library.
fn find_game_exe(launcher_dir: &Path) -> Option<PathBuf> {
    let direct = launcher_dir.join(GAME_EXE_NAME);
    if direct.is_file() {
        return Some(direct);
    }
    if let Some(parent) = launcher_dir.parent() {
        let up = parent.join(GAME_EXE_NAME);
        if up.is_file() {
            return Some(up);
        }
    }
    find_sekiro_in_steam()
}

fn build_cmd_line(exe_path: &Path, extra_args: &[String]) -> String {
    let mut cmd = format!("\"{}\"", exe_path.display());
    for arg in extra_args {
        cmd.push(' ');
        if arg.is_empty() || arg.contains([' ', '\t', '"']) {
            cmd.push('"');
            for ch in arg.chars() {
                if ch == '"' {
                    cmd.push_str("\\\"");
                } else {
                    cmd.push(ch);
                }
            }
            cmd.push('"');
        } else {
            cmd.push_str(arg);
        }
    }
    cmd
}

fn inject_and_resume(child: &mut SuspendedChild, dll_path: &Path) -> Result<(), String> {
    let dll_wide = to_wide(&dll_path.to_string_lossy());
    let byte_len = dll_wide.len() * size_of::<u16>();

    let remote_mem = unsafe {
        VirtualAllocEx(
            child.process.as_raw(),
            null_mut(),
            byte_len,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote_mem.is_null() {
        return Err(format!(
            "VirtualAllocEx failed in sekiro.exe: {}",
            win32_err(unsafe { GetLastError() })
        ));
    }

    let result = (|| -> Result<(), String> {
        let mut written = 0usize;
        let ok = unsafe {
            WriteProcessMemory(
                child.process.as_raw(),
                remote_mem,
                dll_wide.as_ptr().cast(),
                byte_len,
                &mut written,
            )
        };
        if ok == 0 || written != byte_len {
            return Err(format!(
                "WriteProcessMemory failed in sekiro.exe: {}",
                win32_err(unsafe { GetLastError() })
            ));
        }

        let k32_name = to_wide("kernel32.dll");
        let k32 = unsafe { GetModuleHandleW(k32_name.as_ptr()) };
        if k32.is_null() {
            return Err("Could not locate kernel32.dll.".into());
        }
        let load_lib = unsafe { GetProcAddress(k32, b"LoadLibraryW\0".as_ptr()) } as usize;
        if load_lib == 0 {
            return Err("Could not locate kernel32!LoadLibraryW.".into());
        }

        let raw_thread = unsafe {
            CreateRemoteThread(
                child.process.as_raw(),
                null(),
                0,
                load_lib,
                remote_mem,
                0,
                null_mut(),
            )
        };
        if raw_thread.is_null() {
            return Err(format!(
                "CreateRemoteThread failed: {}",
                win32_err(unsafe { GetLastError() })
            ));
        }
        let remote_thread = OwnedHandle(raw_thread);

        let wait = unsafe { WaitForSingleObject(remote_thread.as_raw(), 15_000) };
        if wait != WAIT_OBJECT_0 {
            return Err(format!(
                "Timed out or failed waiting for DLL load in sekiro.exe (wait={wait:#x}, {}).",
                win32_err(unsafe { GetLastError() })
            ));
        }

        let mut exit_code = 0u32;
        if unsafe { GetExitCodeThread(remote_thread.as_raw(), &mut exit_code) } == 0
            || exit_code == 0
        {
            return Err(format!(
                "sekiro.exe could not load '{}'. Verify the file is intact and not blocked by antivirus.",
                dll_path.display()
            ));
        }

        Ok(())
    })();

    unsafe { VirtualFreeEx(child.process.as_raw(), remote_mem, 0, MEM_RELEASE) };
    result?;

    if unsafe { ResumeThread(child.thread.as_raw()) } == u32::MAX {
        return Err(format!(
            "ResumeThread failed on sekiro.exe: {}",
            win32_err(unsafe { GetLastError() })
        ));
    }
    child.resumed = true;
    Ok(())
}

fn launch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Failed to locate launcher path: {e}"))?;
    let launcher_dir = exe
        .parent()
        .ok_or_else(|| "Launcher has no parent directory.".to_string())?;

    let dll_rel = find_dll_path(launcher_dir).ok_or_else(|| {
        format!(
            "Could not find {DLL_NAME}.\n\n\
             Make sure the '{DLL_SUBFOLDER}' folder (containing {DLL_NAME}) is in the same folder as coop_sekiro_launcher.exe."
        )
    })?;
    let dll_path = abs_windows_path(&dll_rel)?;
    ensure_default_ini(&dll_path);

    let game_rel = find_game_exe(launcher_dir).ok_or_else(|| {
        format!(
            "Could not find {GAME_EXE_NAME}.\n\n\
             Copy coop_sekiro_launcher.exe and the '{DLL_SUBFOLDER}' folder into your Sekiro game directory (next to {GAME_EXE_NAME}), then launch coop_sekiro_launcher.exe."
        )
    })?;
    let game_exe = abs_windows_path(&game_rel)?;
    let game_dir = game_exe
        .parent()
        .ok_or_else(|| "Game executable has no parent directory.".to_string())?;

    let app_id_k = to_wide("SteamAppId");
    let game_id_k = to_wide("SteamGameId");
    let app_id_v = to_wide(SEKIRO_APP_ID);
    unsafe {
        SetEnvironmentVariableW(app_id_k.as_ptr(), app_id_v.as_ptr());
        SetEnvironmentVariableW(game_id_k.as_ptr(), app_id_v.as_ptr());
    }

    let extra_args: Vec<String> = std::env::args().skip(1).collect();
    let app_w = to_wide(&game_exe.to_string_lossy());
    let mut cmd_w = to_wide(&build_cmd_line(&game_exe, &extra_args));
    let dir_w = to_wide(&game_dir.to_string_lossy());

    let mut si: StartupInfoW = unsafe { MaybeUninit::zeroed().assume_init() };
    si.cb = size_of::<StartupInfoW>() as u32;
    let mut pi: ProcessInformation = unsafe { MaybeUninit::zeroed().assume_init() };

    let ok = unsafe {
        CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED,
            null(),
            dir_w.as_ptr(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_ELEVATION_REQUIRED {
            return Err(
                "sekiro.exe is configured to run as administrator.\n\n\
                 Either disable 'Run this program as an administrator' on sekiro.exe, or run coop_sekiro_launcher.exe as administrator."
                    .into(),
            );
        }
        return Err(format!(
            "Failed to start '{}': {}",
            game_exe.display(),
            win32_err(code)
        ));
    }

    let mut child = SuspendedChild {
        process: OwnedHandle(pi.h_process),
        thread: OwnedHandle(pi.h_thread),
        resumed: false,
    };

    inject_and_resume(&mut child, &dll_path)
}

fn main() {
    if let Err(err) = launch() {
        show_error(&err);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "coop-sekiro-launcher-test-{}-{tag}-{ns}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_dll_in_coop_sekiro_subfolder_first() {
        let root = temp_dir("dll-prio");
        let sub = root.join(DLL_SUBFOLDER);
        fs::create_dir_all(&sub).unwrap();
        let sub_dll = sub.join(DLL_NAME);
        let adj_dll = root.join(DLL_NAME);
        fs::write(&sub_dll, b"sub").unwrap();
        fs::write(&adj_dll, b"adj").unwrap();

        assert_eq!(find_dll_path(&root), Some(sub_dll));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn falls_back_to_dll_adjacent_to_launcher() {
        let root = temp_dir("dll-adj");
        let adj_dll = root.join(DLL_NAME);
        fs::write(&adj_dll, b"adj").unwrap();

        assert_eq!(find_dll_path(&root), Some(adj_dll));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn finds_game_exe_adjacent_or_in_parent() {
        let root = temp_dir("exe");
        let exe = root.join(GAME_EXE_NAME);
        fs::write(&exe, b"mz").unwrap();
        let sub = root.join(DLL_SUBFOLDER);
        fs::create_dir_all(&sub).unwrap();

        assert_eq!(find_game_exe(&root), Some(exe.clone()));
        assert_eq!(find_game_exe(&sub), Some(exe));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_vdf_library_paths() {
        let sample = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
	}
	"1"
	{
		"path"		"D:\\Games\\SteamLibrary"
	}
	"2"		"E:\\MoreGames"
}
"#;
        let libs = parse_vdf_library_paths(sample);
        assert_eq!(
            libs,
            vec![
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\Games\SteamLibrary"),
                PathBuf::from(r"E:\MoreGames"),
            ]
        );
    }

    #[test]
    fn ensures_default_ini_without_overwriting_existing() {
        let root = temp_dir("ini");
        let dll = root.join(DLL_NAME);
        fs::write(&dll, b"dll").unwrap();

        ensure_default_ini(&dll);
        let ini = root.join(SETTINGS_NAME);
        assert_eq!(fs::read_to_string(&ini).unwrap(), DEFAULT_SETTINGS_INI);

        fs::write(&ini, "password = secret\n").unwrap();
        ensure_default_ini(&dll);
        assert_eq!(fs::read_to_string(&ini).unwrap(), "password = secret\n");

        fs::remove_dir_all(&root).ok();
    }
}
