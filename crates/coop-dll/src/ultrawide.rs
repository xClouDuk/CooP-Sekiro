//! Ultrawide support (21:9 and 32:9), added in 1.3.0-beta. Patterns and the approach come from
//! Sekiro FPS Unlocker and more (uberhalit, MIT; `GameData.cs`), changed so that the game's own
//! resolutions stay.
//!
//! Three parts:
//!
//! 1. **Letterbox / 16:9 clamp** (`PATTERN_SCALING`): `test ecx, ecx; je` decides whether the
//!    16:9 scaling is enforced. `nop; jmp short` (`90 90 EB`) skips it, so a wider render
//!    resolution fills the screen. Applied only while the game's internal resolution is wider
//!    than 16:9 (checked every half second), so 16:9 players keep the exact vanilla code.
//! 2. **Aspect and FOV**: the engine computes `aspect = width / height` from the internal
//!    resolution (`PATTERN_RESOLUTION`) and keeps the vertical FOV for anything wider than 16:9
//!    (docs/RE-FINDINGS-2.md section 3), so once the clamp is gone the picture is already
//!    "Hor+", nothing is cropped at the top and bottom. `ultrawide_fov` (percent) scales the
//!    camera's degrees-to-radians constant for players who want more or less; it is redirected
//!    through a data cave, so only that one instruction is affected (uberhalit writes the
//!    shared constant itself).
//! 3. **Resolutions** in the Options menu. The game's list is a fixed 12-entry array
//!    (800x450 .. 3840x2160 + a 10000x10001 end marker, `.data`, immediately followed by other
//!    data), so it cannot simply grow. Entries are therefore only taken over where nothing is
//!    lost for this display: first entries larger than the desktop (the game cannot show them
//!    on this monitor anyway), and, only if that is not enough and `ultrawide_use_lowest_slots`
//!    is on, the smallest ones (800x450, 1024x576, 1152x648). Every other entry keeps its
//!    place and value.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

use crate::log;

/// `[DISPLAY] ultrawide` (default on): the clamp patch, the added resolutions and the FOV.
pub static ENABLED: AtomicBool = AtomicBool::new(true);
/// `[DISPLAY] ultrawide_fov` in percent (100 = the game's own).
pub static FOV_PCT: AtomicU32 = AtomicU32::new(100);
/// `[DISPLAY] ultrawide_use_lowest_slots`.
pub static USE_LOWEST: AtomicBool = AtomicBool::new(true);
/// `[DISPLAY] ultrawide_resolutions`: "auto", "off" or "3440x1440, 2560x1080".
pub static RESOLUTIONS: Mutex<String> = Mutex::new(String::new());

/// The game's own list, in order (`.data`, 8 bytes each: width, height).
pub const VANILLA: [(i32, i32); 12] = [
    (800, 450),
    (1024, 576),
    (1152, 648),
    (1280, 720),
    (1440, 810),
    (1600, 900),
    (1680, 945),
    (1920, 1080),
    (2048, 1152),
    (2560, 1440),
    (3200, 1800),
    (3840, 2160),
];
/// The marker after the last entry.
const END_MARKER: (i32, i32) = (10000, 10001);
/// The ultrawide modes that may be added (21:9, then 32:9).
pub const CATALOG: [(i32, i32); 7] =
    [(2560, 1080), (3440, 1440), (5120, 2160), (3840, 1080), (3840, 1440), (5120, 1440), (7680, 2160)];
/// At most this many of the smallest entries are given up when `ultrawide_use_lowest_slots` is on.
const LOWEST_SLOTS: usize = 3;
/// Wider than this counts as ultrawide (16:10 = 1.6 and 16:9 = 1.778 do not).
const UW_ASPECT: f32 = 1.8;

const PATTERN_SCALING: &str = "85 C9 74 ?? 47 8B ?? ?? ?? ?? ?? ?? 45 ?? ?? 74";
const PATTERN_RESOLUTION: &str = "0F 57 D2 89 0D ?? ?? ?? ?? 0F 57 C9";
const PATTERN_FOV: &str = "F3 0F 10 08 F3 0F 59 0D ?? ?? ?? ?? F3 0F 5C 4E";
/// Fallback of the internal width (height follows at +4), docs/RE-FINDINGS-2.md.
const RESOLUTION_RVA: usize = 0x3B1D100;
const DEG2RAD: f32 = 0.017_453_3;

static SCALING_SITE: AtomicUsize = AtomicUsize::new(0);
static RES_ADDR: AtomicUsize = AtomicUsize::new(0);
/// The `mulss xmm1, [rip+rel32]` of the camera (0 = not found) and, once a fov other than 100 %
/// is wanted, the cave it was redirected to.
static FOV_INS: AtomicUsize = AtomicUsize::new(0);
static FOV_CAVE: AtomicUsize = AtomicUsize::new(0);
/// What is written now: 1 vanilla, 2 patched (clamp), and the FOV percent.
static CLAMP_STATE: AtomicU32 = AtomicU32::new(1);
static FOV_STATE: AtomicU32 = AtomicU32::new(100);
static TICKS: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplaySettingsW(device: *const u16, mode: u32, dev_mode: *mut u8) -> i32;
}

// ---------------------------------------------------------------- pure rules

/// "3440x1440, 2560 x 1080" -> the valid modes (width 640..=16384, height 360..=8640).
pub fn parse_list(s: &str) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    // "2560 x 1080" -> "2560x1080" before the entries are cut at commas / spaces.
    let s = s.to_ascii_lowercase().replace(" x", "x").replace("x ", "x");
    for part in s.split([',', ';', ' ']).filter(|p| !p.is_empty()) {
        let Some((w, h)) = part.split_once('x') else { continue };
        if let (Ok(w), Ok(h)) = (w.trim().parse::<i32>(), h.trim().parse::<i32>())
            && (640..=16384).contains(&w)
            && (360..=8640).contains(&h)
            && !out.contains(&(w, h))
        {
            out.push((w, h));
        }
    }
    out
}

pub fn aspect(w: i32, h: i32) -> f32 {
    if h <= 0 { 0.0 } else { w as f32 / h as f32 }
}

pub fn is_ultrawide(w: i32, h: i32) -> bool {
    aspect(w, h) > UW_ASPECT
}

/// The modes to add for a setting: `manual` (None = auto) and the desktop size (None = unknown).
/// Auto adds nothing on a screen that is not ultrawide; otherwise the desktop's own mode first,
/// then the catalog modes that fit the desktop, largest first.
pub fn candidates(desktop: Option<(i32, i32)>, manual: Option<&[(i32, i32)]>) -> Vec<(i32, i32)> {
    let fits = |m: &(i32, i32)| desktop.is_none_or(|(dw, dh)| m.0 <= dw && m.1 <= dh);
    let mut v: Vec<(i32, i32)> = match manual {
        Some(list) => list.iter().copied().filter(fits).collect(),
        None => {
            let Some((dw, dh)) = desktop.filter(|&(w, h)| is_ultrawide(w, h)) else { return Vec::new() };
            let mut rest: Vec<(i32, i32)> = CATALOG.iter().copied().filter(|m| m.0 <= dw && m.1 <= dh).collect();
            rest.sort_by_key(|m| std::cmp::Reverse(m.0 as i64 * m.1 as i64));
            let mut v = vec![(dw, dh)];
            v.extend(rest.into_iter().filter(|m| *m != (dw, dh)));
            v
        }
    };
    v.retain(|m| !VANILLA.contains(m));
    v
}

/// (slot, mode) pairs to write into the 12-entry list. Slots are taken from the entries that
/// are larger than the desktop; with `use_lowest`, up to `LOWEST_SLOTS` of the smallest ones
/// for what is left over. More candidates than slots: the first ones (the most useful) win.
pub fn plan(desktop: Option<(i32, i32)>, cands: &[(i32, i32)], use_lowest: bool) -> Vec<(usize, (i32, i32))> {
    let mut slots: Vec<usize> = match desktop {
        Some((dw, dh)) => (0..VANILLA.len()).filter(|&i| VANILLA[i].0 > dw || VANILLA[i].1 > dh).collect(),
        None => Vec::new(),
    };
    let unusable = slots.len();
    if use_lowest && cands.len() > unusable {
        let extra = (cands.len() - unusable).min(LOWEST_SLOTS);
        let more: Vec<usize> = (0..VANILLA.len()).filter(|i| !slots.contains(i)).take(extra).collect();
        slots.extend(more);
    }
    let n = cands.len().min(slots.len());
    let mut chosen: Vec<(i32, i32)> = cands[..n].to_vec();
    chosen.sort_by_key(|m| m.0 as i64 * m.1 as i64);
    // Slots in list order; the unusable (large) ones first take the large modes.
    let mut used: Vec<usize> = slots[..n].to_vec();
    used.sort_unstable();
    used.into_iter().zip(chosen).collect()
}

/// Percent -> the camera constant (degrees to radians, scaled).
pub fn fov_constant(pct: u32) -> f32 {
    DEG2RAD * (pct.clamp(50, 200) as f32 / 100.0)
}

// ---------------------------------------------------------------- memory

/// A PE section of the exe as a slice.
fn section(name: &[u8]) -> Option<(usize, &'static [u8])> {
    let base = crate::game::module_base();
    unsafe {
        let pe = base + *((base + 0x3C) as *const u32) as usize;
        let sections = *((pe + 6) as *const u16) as usize;
        let opt_size = *((pe + 20) as *const u16) as usize;
        let first = pe + 24 + opt_size;
        for i in 0..sections {
            let s = first + i * 40;
            let n = std::slice::from_raw_parts(s as *const u8, 8);
            if n.starts_with(name) {
                let vsize = *((s + 8) as *const u32) as usize;
                let rva = *((s + 12) as *const u32) as usize;
                return Some((base + rva, std::slice::from_raw_parts((base + rva) as *const u8, vsize)));
            }
        }
    }
    None
}

fn table_bytes(entries: &[(i32, i32)]) -> Vec<u8> {
    entries.iter().flat_map(|&(w, h)| w.to_le_bytes().into_iter().chain(h.to_le_bytes())).collect()
}

/// The desktop resolution of the primary display (None if Windows does not say).
fn desktop() -> Option<(i32, i32)> {
    // DEVMODEW is 220 bytes: dmSize u16 at 68, dmPelsWidth u32 at 172, dmPelsHeight u32 at 176.
    let mut dm = [0u8; 220];
    dm[68..70].copy_from_slice(&220u16.to_le_bytes());
    // ENUM_CURRENT_SETTINGS = -1
    if unsafe { EnumDisplaySettingsW(std::ptr::null(), u32::MAX, dm.as_mut_ptr()) } == 0 {
        return None;
    }
    let w = u32::from_le_bytes(dm[172..176].try_into().ok()?) as i32;
    let h = u32::from_le_bytes(dm[176..180].try_into().ok()?) as i32;
    (w >= 640 && h >= 360).then_some((w, h))
}

/// The game's internal render size now.
fn internal_size() -> Option<(i32, i32)> {
    let a = RES_ADDR.load(Acquire);
    if a == 0 {
        return None;
    }
    let w = crate::game::read::<i32>(a)?;
    let h = crate::game::read::<i32>(a + 4)?;
    (w > 0 && h > 0 && w < 20000 && h < 20000).then_some((w, h))
}

/// Waits briefly for SteamStub to finish unpacking `.text`, then installs ultrawide patches
/// *before* the game's boot code reads `GraphicsConfig.xml` and computes the initial viewport.
pub fn install_early() -> bool {
    if !ENABLED.load(Relaxed) {
        return install();
    }
    let pat = crate::fps::pattern(PATTERN_SCALING);
    let t0 = std::time::Instant::now();
    for _ in 0..600 {
        if let Some((_, text)) = crate::fps::text()
            && crate::fps::find(text, &pat).is_some()
        {
            let ok = install();
            log::line(&format!("ultrawide: early install finished in {} ms (ok = {ok})", t0.elapsed().as_millis()));
            return ok;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    log::line("ultrawide: early unpack wait timed out, retrying after task system ready");
    false
}

pub fn install() -> bool {
    if !ENABLED.load(Relaxed) {
        if !INSTALLED.swap(true, Release) {
            log::line("ultrawide: off (ultrawide = 0), the game's own 16:9 handling stays");
        }
        return false;
    }
    if INSTALLED.swap(true, Release) {
        return SCALING_SITE.load(Acquire) != 0;
    }
    let desk = desktop();
    let mut ok = false;

    // 1. The internal resolution (the clamp is switched by its aspect).
    if let Some((start, text)) = crate::fps::text() {
        let res = crate::fps::find(text, &crate::fps::pattern(PATTERN_RESOLUTION)).map(|i| {
            let ins = start + i + 3; // mov [rip+rel32], ecx (6 bytes)
            let rel = i32::from_le_bytes(unsafe { *((ins + 2) as *const [u8; 4]) });
            (ins as i64 + 6 + rel as i64) as usize
        });
        let res = res.filter(|&a| crate::game::read::<i32>(a).is_some()).unwrap_or(crate::game::module_base() + RESOLUTION_RVA);
        RES_ADDR.store(res, Release);

        // 2. The 16:9 clamp.
        let pat = crate::fps::pattern(PATTERN_SCALING);
        let hits = text.windows(pat.len()).filter(|w| w.iter().zip(&pat).all(|(b, p)| p.is_none_or(|p| *b == p))).count();
        match crate::fps::find(text, &pat).map(|i| start + i) {
            Some(site) if unsafe { *((site + 2) as *const u8) } == 0x74 => {
                SCALING_SITE.store(site, Release);
                ok = true;
                log::line(&format!("ultrawide: 16:9 clamp at {site:#x} ({hits} match(es), first used), internal size at {res:#x}"));
            }
            _ => log::line("ultrawide: 16:9 clamp pattern not found, black bars stay"),
        }

        // 3. FOV: found now, redirected to a cave only when a fov other than 100 % is wanted.
        match crate::fps::find(text, &crate::fps::pattern(PATTERN_FOV)).map(|i| start + i + 4) {
            Some(ins) => {
                let value = crate::game::read::<f32>(fov_constant_addr(ins)).unwrap_or(0.0);
                if (0.0174..0.0175).contains(&value) {
                    FOV_INS.store(ins, Release);
                    log::line(&format!("ultrawide: fov instruction at {ins:#x}, constant {value}"));
                } else {
                    log::line(&format!("ultrawide: fov instruction found but its constant is {value}, fov stays"));
                }
            }
            None => log::line("ultrawide: fov pattern not found, fov stays"),
        }
    } else {
        log::line("ultrawide: .text not found, nothing patched");
    }

    // 4. The resolution list.
    ok |= add_resolutions(desk);
    TICKS.store(0, Relaxed);
    apply();
    ok
}

fn add_resolutions(desk: Option<(i32, i32)>) -> bool {
    let setting = RESOLUTIONS.lock().map(|s| s.trim().to_owned()).unwrap_or_default();
    let manual = match setting.to_ascii_lowercase().as_str() {
        "off" | "none" | "0" => {
            log::line("ultrawide: ultrawide_resolutions = off, the resolution list is not touched");
            return false;
        }
        "" | "auto" => None,
        _ => Some(parse_list(&setting)),
    };
    let cands = candidates(desk, manual.as_deref());
    if cands.is_empty() {
        log::line(&format!("ultrawide: no resolution to add (desktop {desk:?}, setting {setting:?})"));
        return false;
    }
    let changes = plan(desk, &cands, USE_LOWEST.load(Relaxed));
    if changes.is_empty() {
        log::line(&format!("ultrawide: wanted {cands:?} but no free slot (desktop {desk:?}; ultrawide_use_lowest_slots = 0?)"));
        return false;
    }
    let Some((start, data)) = section(b".data") else {
        log::line("ultrawide: .data not found, resolution list unchanged");
        return false;
    };
    let mut want = VANILLA.to_vec();
    want.push(END_MARKER);
    let needle = table_bytes(&want);
    let Some(off) = data.windows(needle.len()).position(|w| w == needle.as_slice()) else {
        log::line("ultrawide: the game's resolution list was not found unchanged (another mod?), left alone");
        return false;
    };
    let table = start + off;
    let mut done = Vec::new();
    for (slot, (w, h)) in &changes {
        if crate::game::write_bytes(table + slot * 8, &table_bytes(&[(*w, *h)])) {
            done.push(format!("{}x{} for {}x{}", w, h, VANILLA[*slot].0, VANILLA[*slot].1));
        }
    }
    log::line(&format!("ultrawide: resolution list at {table:#x}: added {} (desktop {desk:?})", done.join(", ")));
    !done.is_empty()
}

/// Every frame (cheap: works every ~30th call): the clamp follows the internal aspect, the fov
/// follows the setting.
pub fn apply() {
    if TICKS.fetch_add(1, Relaxed) % 30 != 0 {
        return;
    }
    let on = ENABLED.load(Relaxed);
    let site = SCALING_SITE.load(Acquire);
    if site != 0 {
        // Patch the 16:9 scaling/letterbox check unconditionally when `ultrawide = 1`
        // (Sekiro clamps internal render dimensions to 16:9 until this jump is bypassed).
        let want = if on { 2 } else { 1 };
        if CLAMP_STATE.load(Relaxed) != want {
            let bytes: [u8; 3] = if on { [0x90, 0x90, 0xEB] } else { [0x85, 0xC9, 0x74] };
            if crate::fps::write_code(site, &bytes) {
                CLAMP_STATE.store(want, Relaxed);
                log::line(&format!(
                    "ultrawide: 16:9 clamp {} (internal size {:?})",
                    if on { "removed" } else { "back" },
                    internal_size()
                ));
            }
        }
    }
    let ins = FOV_INS.load(Acquire);
    if ins != 0 {
        let pct = if on { FOV_PCT.load(Relaxed).clamp(50, 200) } else { 100 };
        if FOV_STATE.load(Relaxed) != pct {
            let mut cave = FOV_CAVE.load(Acquire);
            if cave == 0 && pct != 100 {
                cave = redirect_fov(ins).unwrap_or(0);
                FOV_CAVE.store(cave, Release);
            }
            if cave != 0 {
                unsafe { *(cave as *mut f32) = fov_constant(pct) };
                FOV_STATE.store(pct, Relaxed);
                log::line(&format!("ultrawide: fov {pct} %"));
            } else if pct != 100 {
                // No cave: do not try again every half second.
                FOV_STATE.store(pct, Relaxed);
                log::line("ultrawide: fov cave not available, fov stays");
            }
        }
    }
}

/// Address of the float a `mulss xmm1, [rip+rel32]` at `ins` reads.
fn fov_constant_addr(ins: usize) -> usize {
    let rel = i32::from_le_bytes(unsafe { *((ins + 4) as *const [u8; 4]) });
    (ins as i64 + 8 + rel as i64) as usize
}

/// A cave near `ins` holding the original constant, and the mulss pointed at it.
fn redirect_fov(ins: usize) -> Option<usize> {
    let cave = crate::hook::alloc_near_pub(ins)?;
    let orig = crate::game::read::<f32>(fov_constant_addr(ins))?;
    unsafe { *(cave as *mut f32) = orig };
    let rel = i32::try_from(cave as i64 - (ins as i64 + 8)).ok()?;
    crate::fps::write_code(ins + 4, &rel.to_le_bytes()).then_some(cave)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vanilla_table_is_what_the_exe_holds() {
        // docs: dumped from sekiro.exe 1.06 (.data 0x143B1CB30): 800x450 .. 3840x2160, 10000 / 10001.
        let b = table_bytes(&VANILLA);
        assert_eq!(b.len(), 96);
        assert_eq!(&b[..8], &[0x20, 0x03, 0, 0, 0xC2, 0x01, 0, 0]);
        assert_eq!(&b[56..64], &[0x80, 0x07, 0, 0, 0x38, 0x04, 0, 0]);
    }

    #[test]
    fn manual_lists_parse_and_bad_entries_drop() {
        assert_eq!(parse_list("3440x1440, 2560 x 1080;5120X1440"), vec![(3440, 1440), (2560, 1080), (5120, 1440)]);
        assert_eq!(parse_list("3440x1440 3440x1440"), vec![(3440, 1440)]);
        assert!(parse_list("auto, 10x10, 99999x1440, abc").is_empty());
    }

    #[test]
    fn auto_adds_nothing_on_a_16_9_screen() {
        assert!(candidates(Some((1920, 1080)), None).is_empty());
        assert!(candidates(Some((2560, 1600)), None).is_empty());
        assert!(candidates(None, None).is_empty());
    }

    #[test]
    fn auto_puts_the_desktop_first_then_what_fits() {
        assert_eq!(candidates(Some((3440, 1440)), None), vec![(3440, 1440), (2560, 1080)]);
        assert_eq!(candidates(Some((2560, 1080)), None), vec![(2560, 1080)]);
        assert_eq!(candidates(Some((5120, 1440)), None), vec![(5120, 1440), (3840, 1440), (3440, 1440), (3840, 1080), (2560, 1080)]);
        // A desktop that is not in the catalog is still the first.
        assert_eq!(candidates(Some((3840, 1600)), None)[0], (3840, 1600));
    }

    #[test]
    fn manual_lists_keep_their_order_and_drop_what_does_not_fit() {
        let m = [(5120, 2160), (3440, 1440), (2560, 1080)];
        assert_eq!(candidates(Some((3440, 1440)), Some(&m[..])), vec![(3440, 1440), (2560, 1080)]);
        assert_eq!(candidates(None, Some(&m[..])), m.to_vec());
        // Never one the game already lists.
        assert!(candidates(Some((3840, 2160)), Some(&[(1920, 1080)][..])).is_empty());
    }

    #[test]
    fn slots_come_from_what_the_desktop_cannot_show() {
        // 3440x1440: 3200x1800 and 3840x2160 are too tall, two free slots, both used.
        let c = candidates(Some((3440, 1440)), None);
        assert_eq!(plan(Some((3440, 1440)), &c, false), vec![(10, (2560, 1080)), (11, (3440, 1440))]);
        // 2560x1080 needs one; four entries are taller than the desktop (2048x1152 and up), the
        // first of them is used.
        let c = candidates(Some((2560, 1080)), None);
        assert_eq!(plan(Some((2560, 1080)), &c, false), vec![(8, (2560, 1080))]);
        // Nothing is ever taken from an entry the desktop can show when use_lowest is off.
        let c = candidates(Some((5120, 1440)), None);
        let p = plan(Some((5120, 1440)), &c, false);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|(s, _)| *s >= 10), "{p:?}");
    }

    #[test]
    fn lowest_slots_only_when_needed_and_allowed() {
        // 7680x2160: all 12 fit the desktop, so only the lowest slots can host new modes.
        let c = candidates(Some((7680, 2160)), None);
        assert_eq!(plan(Some((7680, 2160)), &c, false), vec![]);
        let p = plan(Some((7680, 2160)), &c, true);
        assert_eq!(p.len(), 3);
        assert_eq!(p.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![0, 1, 2]);
        // The most useful (the desktop's own) is among them.
        assert!(p.iter().any(|(_, m)| *m == (7680, 2160)));
        // Free slots first: 3440x1440 has two of its own, no lowest slot is touched.
        let c = candidates(Some((3440, 1440)), None);
        assert!(plan(Some((3440, 1440)), &c, true).iter().all(|(s, _)| *s >= 10));
    }

    #[test]
    fn the_fov_constant_scales_and_clamps() {
        assert!((fov_constant(100) - DEG2RAD).abs() < 1e-7);
        assert!((fov_constant(125) - DEG2RAD * 1.25).abs() < 1e-7);
        assert_eq!(fov_constant(5), fov_constant(50));
        assert_eq!(fov_constant(999), fov_constant(200));
    }

    #[test]
    fn ultrawide_starts_above_16_9() {
        assert!(!is_ultrawide(1920, 1080));
        assert!(!is_ultrawide(1920, 1200));
        assert!(is_ultrawide(2560, 1080));
        assert!(is_ultrawide(3840, 1080));
        assert!((aspect(3440, 1440) - 2.388_889).abs() < 1e-5);
    }
}
