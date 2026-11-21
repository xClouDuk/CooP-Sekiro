//! Overlay (ImGui via hudhook, DX11): a status line in the top left corner, notices, the
//! friend's nameplate and the off-screen arrow. There is no menu; settings live in
//! `sekiro_coop_settings.ini` (`settings.rs`).
//!
//! The render thread never touches game memory: it reads a status snapshot the game task
//! publishes and posts the debug hotkeys (`debug_hotkeys = 1`: F8, F9, F11) back through
//! `SHARED`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::imgui::{Condition, Context, FontConfig, FontSource, Io, Key, Ui};
use hudhook::windows::Win32::Foundation::HINSTANCE;
use hudhook::{Hudhook, ImguiRenderLoop, MessageFilter, RenderContext};

use crate::peers::PeerId;
use crate::{log, settings, tuning};

/// True when the last observed user input came from an XInput controller; false for keyboard/mouse.
static INPUT_IS_PAD: AtomicBool = AtomicBool::new(false);

pub fn is_pad_mode() -> bool {
    INPUT_IS_PAD.load(Ordering::Relaxed)
}

/// True when world-space player overlays (nameplates, edge markers, lock mark, party HUD)
/// should be visible (in-world, not mid-warp, no menu open, and not in a cutscene).
static HUD_VISIBLE: AtomicBool = AtomicBool::new(false);

pub fn set_hud_visible(visible: bool) {
    HUD_VISIBLE.store(visible, Ordering::Relaxed);
}

pub fn hud_visible() -> bool {
    HUD_VISIBLE.load(Ordering::Relaxed)
}

pub fn should_show_world_hud(
    in_world: bool,
    hands_off: bool,
    menu_mask: Option<u32>,
    cutscene_active: bool,
) -> bool {
    in_world && !hands_off && menu_mask == Some(0) && !cutscene_active
}

/// Pure input-mode state transition: only flips when one source shows clear activity alone.
pub fn detect_input_mode(prev_pad: bool, pad_active: bool, kbm_active: bool) -> bool {
    if pad_active && !kbm_active {
        true
    } else if kbm_active && !pad_active {
        false
    } else {
        prev_pad
    }
}

fn pad_has_activity(pad: Option<crate::spectate_input::PadState>) -> bool {
    pad.is_some_and(crate::spectate_input::pad_has_input)
}

fn kbm_has_activity(ui: &Ui) -> bool {
    let io = ui.io();
    let delta = io.mouse_delta[0].abs() + io.mouse_delta[1].abs();
    if delta > 2.5
        || io.mouse_down.iter().any(|&b| b)
        || io.mouse_wheel.abs() > 0.0
        || io.mouse_wheel_h.abs() > 0.0
        || io.keys_down.iter().any(|&k| k)
        || io.key_ctrl
        || io.key_shift
        || io.key_alt
        || io.key_super
    {
        return true;
    }
    Key::VARIANTS
        .iter()
        .copied()
        .filter(|&k| (k as u32) <= (Key::KeypadEqual as u32))
        .any(|k| ui.is_key_down(k))
}

/// Game task → overlay.
#[derive(Default, Clone)]
pub struct Status {
    /// Co-op connection state, shown after "Co-op: ".
    pub net: String,
    /// One nameplate per other player shown: world position (feet), name, ping. A peer
    /// without one is hidden (not connected, other area, or dead).
    pub friend_tags: Vec<FriendTag>,
    /// Extra status lines (limbo, rest, boss), shown under the status line.
    pub notice: Option<String>,
    /// Party HP bars (companion), drawn on the left of the screen. Empty = none.
    pub party: Vec<PartyMember>,
}

/// One row of the party HP bar. `state`: 0 = here, 1 = other area / loading, 2 = dead.
#[derive(Clone)]
pub struct PartyMember {
    /// Whose row (head mode hangs it under that peer's nameplate).
    pub id: PeerId,
    pub name: String,
    pub hp: i32,
    pub max_hp: i32,
    pub state: u8,
}

#[derive(Clone, Debug)]
pub struct FriendTag {
    /// Whose nameplate (PLAN-MULTIPLAYER §5.3). The tags come in id order, so stacked
    /// off-screen markers (`edge_stack`) keep a stable order; head mode finds its HP row.
    pub id: PeerId,
    pub pos: [f32; 3],
    pub name: String,
    pub ping_ms: Option<u32>,
}

/// Overlay → game task (consumed once). Only the debug hotkeys post these.
#[derive(Default)]
pub struct Commands {
    /// F8: stop waiting on the death screen and go to the idol (bossrules.rs).
    pub wake: bool,
    /// F9: warp to the friend now (join.rs go_to_friend).
    pub go_to_friend: bool,
    /// F11: one log line with the unix time (ms) to line up what was seen.
    pub mark: bool,
}

#[derive(Default)]
struct Shared {
    status: Status,
    commands: Commands,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

pub fn publish(status: Status) {
    if let Ok(mut s) = SHARED.lock() {
        s.get_or_insert_with(Shared::default).status = status;
    }
}

/// The nameplates every frame (the rest of the status only every 10 frames:
/// the tag moved in steps, "slideshow", test 2026-10-22).
pub fn publish_tags(tags: Vec<FriendTag>) {
    if let Ok(mut s) = SHARED.lock() {
        s.get_or_insert_with(Shared::default).status.friend_tags = tags;
    }
}

/// One change in who is connected (`presence`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    Joined(PeerId, String),
    Left(PeerId, String),
}

/// The connected peers as `presence` last saw them.
static PRESENT: Mutex<Vec<(PeerId, String)>> = Mutex::new(Vec::new());

/// The changes from `before` to `now` (both in id order, names as shown): a new id
/// joined, a missing id left, the same id with another name (a rejoin under another
/// name) is only a join, as v7.5's single "NAME joined".
pub fn presence_diff(before: &[(PeerId, String)], now: &[(PeerId, String)]) -> Vec<Presence> {
    let mut out = Vec::new();
    for (id, name) in before {
        if !now.iter().any(|(n, _)| n == id) {
            out.push(Presence::Left(*id, name.clone()));
        }
    }
    for (id, name) in now {
        if !before.iter().any(|(b, n)| b == id && n == name) {
            out.push(Presence::Joined(*id, name.clone()));
        }
    }
    out
}

/// Game task, every frame: the connected peers now (id order; empty while unlinked) ->
/// the overlay text for whoever joined or left since the last call ("NAME joined" /
/// "NAME left", one line each, as v7.5 for the one friend). The game's notice "... has
/// entered your world." is leave.rs' alone (host, `entered_text`): a second one from here
/// was a duplicate (review finding 6).
pub fn presence(now: &[(PeerId, String)]) -> Option<String> {
    let Ok(mut present) = PRESENT.lock() else { return None };
    let before = std::mem::replace(&mut *present, now.to_vec());
    drop(present);
    let changes = presence_diff(&before, now);
    if changes.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for c in &changes {
        let text = match c {
            Presence::Joined(_, name) => format!("{name} joined"),
            Presence::Left(_, name) => format!("{name} left"),
        };
        log::line(&format!("session: {text}"));
        lines.push(text);
    }
    Some(lines.join("\n"))
}

pub fn take_commands() -> Commands {
    SHARED
        .lock()
        .ok()
        .and_then(|mut s| s.as_mut().map(|s| std::mem::take(&mut s.commands)))
        .unwrap_or_default()
}

fn post(f: impl FnOnce(&mut Commands)) {
    if let Ok(mut s) = SHARED.lock() {
        f(&mut s.get_or_insert_with(Shared::default).commands);
    }
}

/// Shown in the status line, so both players see at once whether the mod was updated.
/// Bump it with every release together with the first line of `dist/sekiro-coop/OHJE.txt`.
/// Public numbering from 1.0-beta (= internal build 8.62, 2026-10-28); the 8.x numbers live on
/// only in docs/JATKA-TASTA-MULTIPLAYER.md. 1.0.1-beta: SprjTask lookup retried (Nexus "no ini" reports). 1.0.2-beta: clean public build (build-public.cmd). 1.0.3-beta: Steam lobby AccessDenied explained, joiner without a host told how to use UDP. 1.0.4-beta: co-op items work on non-English games (template row by id). 1.0.5-beta: Kuro's Charm follows the host during a session. 1.0.6-beta: no void fall after a cutscene that changes the map, bosses scaled on every machine. 1.0.7-beta: a player who died in a boss fight follows the killer's post-boss warp, doors checked after every load, notice when the last run did not close normally. 1.0.8-beta: the joiner's own-world restore no longer wipes areas it never saw (closed doors, replayed cutscenes) and mirrors a world behind the host's, Sword Saint's gauge gap is no retreat, a stuck-input watchdog, a living player leaves a phase A wait. 1.0.9-beta: players are warned when their mod versions differ on the same protocol, talk-lock diagnostics.
/// 1.3.1-beta: CooP Sekiro fork by xCløuD: apply ultrawide resolution table and 16:9 clamp
/// bypass early at boot before GraphicsConfig.xml initializes the initial viewport.
/// 1.3.5-beta: Native Steam Invite mode (private invite-only lobbies with per-lobby ephemeral
/// token auth, lobby-member transport allowlist, Kindred Bell invite menu, quit-while-hosting
/// protection, and performance sweep; steam_invites = 1 by default).
/// 1.3.6-beta: Verified boss/miniboss/Great Serpent/Raven deathblow & single-player lockout
/// hardening (client stealth/event claims on untouched minibosses & c5010, Type-5 deathblow
/// guard, long-throw lock extension up to 12 s) and player-to-player consumable/upgrade item drop & pickup.
pub const MOD_VERSION: &str = "1.3.6-beta";

const YELLOW: [f32; 4] = [1.0, 0.85, 0.3, 1.0];
/// Nameplate: full opacity up to this distance, then slowly lighter.
const FADE_FROM_M: f32 = 100.0;
/// Nameplate opacity far away (never hidden by distance).
const MIN_FAR_ALPHA: f32 = 0.85;
/// From this distance the nameplate shows "name  N m".
const SHOW_DIST_FROM_M: f32 = 40.0;

#[derive(Default)]
struct Overlay {
    panel: Panel,
    drop_panel: DropPanel,
}

const PAD_DPAD_UP: u16 = 0x0001;
const PAD_DPAD_DOWN: u16 = 0x0002;
const PAD_DPAD_LEFT: u16 = 0x0004;
const PAD_DPAD_RIGHT: u16 = 0x0008;
const PAD_R3: u16 = 0x0080;
const PAD_LB: u16 = 0x0100;
const PAD_RB: u16 = 0x0200;
const PAD_A: u16 = 0x1000;
const PAD_B: u16 = 0x2000;
const PAD_Y: u16 = 0x8000;

const DLG_STICK_THRESHOLD: i32 = 18_000;
const DLG_FIRST_DELAY_MS: u64 = 350;
const DLG_X_REPEAT_MS: u64 = 90;
const DLG_Y_REPEAT_MS: u64 = 120;

/// Natural controller navigation for the Drop Quantity Dialog using D-pad + either analog stick.
#[derive(Default, Debug)]
struct DialogPadNav {
    x_dir: i8,
    x_next_ms: u64,
    y_dir: i8,
    y_next_ms: u64,
}

impl DialogPadNav {
    fn step(&mut self, pad: Option<crate::spectate_input::PadState>, now_ms: u64) -> i32 {
        let Some(p) = pad else {
            *self = Self::default();
            return 0;
        };
        let x = if (p.buttons & PAD_DPAD_RIGHT) != 0
            || (p.lx as i32) > DLG_STICK_THRESHOLD
            || (p.rx as i32) > DLG_STICK_THRESHOLD
        {
            1i8
        } else if (p.buttons & PAD_DPAD_LEFT) != 0
            || (p.lx as i32) < -DLG_STICK_THRESHOLD
            || (p.rx as i32) < -DLG_STICK_THRESHOLD
        {
            -1i8
        } else {
            0i8
        };
        let y = if (p.buttons & PAD_DPAD_UP) != 0
            || (p.ly as i32) > DLG_STICK_THRESHOLD
            || (p.ry as i32) > DLG_STICK_THRESHOLD
        {
            1i8
        } else if (p.buttons & PAD_DPAD_DOWN) != 0
            || (p.ly as i32) < -DLG_STICK_THRESHOLD
            || (p.ry as i32) < -DLG_STICK_THRESHOLD
        {
            -1i8
        } else {
            0i8
        };

        let mut delta = 0i32;
        if x == 0 {
            self.x_dir = 0;
        } else if x != self.x_dir || now_ms >= self.x_next_ms {
            let first = x != self.x_dir;
            self.x_dir = x;
            self.x_next_ms = now_ms + if first { DLG_FIRST_DELAY_MS } else { DLG_X_REPEAT_MS };
            delta += x as i32;
        }
        if y == 0 {
            self.y_dir = 0;
        } else if y != self.y_dir || now_ms >= self.y_next_ms {
            let first = y != self.y_dir;
            self.y_dir = y;
            self.y_next_ms = now_ms + if first { DLG_FIRST_DELAY_MS } else { DLG_Y_REPEAT_MS };
            delta += (y as i32) * 5;
        }
        delta
    }
}

struct DropDialog {
    full_id: u32,
    name: String,
    max_qty: u32,
    qty: u32,
    just_opened: bool,
}

#[derive(Default)]
struct DropPanel {
    open_hotkey: bool,
    collapsed: bool,
    selected: usize,
    qty: u32,
    last_item: u32,
    nav: crate::menu::PadNav,
    dialog: Option<DropDialog>,
    dialog_nav: DialogPadNav,
    pad_a_was_down: bool,
    pad_b_was_down: bool,
    pad_y_was_down: bool,
    pad_lb_was_down: bool,
    pad_rb_was_down: bool,
    pad_r3_was_down: bool,
}

impl DropPanel {
    fn draw(&mut self, ui: &Ui, snap: &crate::itemdrop::UiSnapshot, pad: Option<crate::spectate_input::PadState>) {
        use crate::menu::NavAction;
        if !snap.connected {
            self.open_hotkey = false;
            self.collapsed = false;
            self.nav = crate::menu::PadNav::default();
            self.dialog = None;
            self.dialog_nav = DialogPadNav::default();
            self.pad_a_was_down = false;
            self.pad_b_was_down = false;
            self.pad_y_was_down = false;
            self.pad_lb_was_down = false;
            self.pad_rb_was_down = false;
            self.pad_r3_was_down = false;
            crate::inputblock::set_dialog_open(false);
            crate::itemdrop::set_panel_visible(false);
            return;
        }

        let focused = crate::spectate_input::game_focused();
        let pad_mode = is_pad_mode();
        let pad_a_down = pad.is_some_and(|p| p.buttons & PAD_A != 0);
        let pad_b_down = pad.is_some_and(|p| p.buttons & PAD_B != 0);
        let pad_y_down = pad.is_some_and(|p| p.buttons & PAD_Y != 0);
        let pad_lb_down = pad.is_some_and(|p| p.buttons & PAD_LB != 0);
        let pad_rb_down = pad.is_some_and(|p| p.buttons & PAD_RB != 0);
        let pad_r3_down = pad.is_some_and(|p| p.buttons & PAD_R3 != 0);
        let pad_a_edge = pad_a_down && !self.pad_a_was_down;
        let pad_b_edge = pad_b_down && !self.pad_b_was_down;
        let pad_y_edge = pad_y_down && !self.pad_y_was_down;
        let pad_lb_edge = pad_lb_down && !self.pad_lb_was_down;
        let pad_rb_edge = pad_rb_down && !self.pad_rb_was_down;
        let pad_r3_edge = pad_r3_down && !self.pad_r3_was_down;
        self.pad_a_was_down = pad_a_down;
        self.pad_b_was_down = pad_b_down;
        self.pad_y_was_down = pad_y_down;
        self.pad_lb_was_down = pad_lb_down;
        self.pad_rb_was_down = pad_rb_down;
        self.pad_r3_was_down = pad_r3_down;

        if focused && ui.is_key_pressed_no_repeat(Key::F6) {
            self.open_hotkey = !self.open_hotkey;
            self.collapsed = false;
        }
        if self.open_hotkey && self.dialog.is_none() && focused && ui.is_key_pressed_no_repeat(Key::Escape) {
            self.open_hotkey = false;
        }

        let [dw, dh] = ui.io().display_size;
        let settings_up = PANEL_VISIBLE.load(Ordering::Relaxed);

        // Feedback toast (visible whether Inventory, world, or F6 drawer is up).
        if let Some(fb) = &snap.feedback
            && dw >= 1.0
            && dh >= 1.0
        {
            let dl = ui.get_foreground_draw_list();
            let [tw, th] = ui.calc_text_size(fb);
            let cx = dw * 0.5;
            let cy = dh * 0.14;
            let (px, py) = (16.0, 8.0);
            let r = [cx - tw * 0.5 - px, cy - th * 0.5 - py, cx + tw * 0.5 + px, cy + th * 0.5 + py];
            dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.06, 0.05, 0.04, 0.88]).filled(true).rounding(6.0).build();
            dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.92, 0.86, 0.68, 0.85]).rounding(6.0).thickness(1.5).build();
            dl.add_text([cx - tw * 0.5, cy - th * 0.5], [1.0, 0.96, 0.84, 1.0], fb);
        }

        // Hover-to-drop prompt in Sekiro Inventory + Y/Triangle open trigger.
        if snap.in_inventory_menu && self.dialog.is_none()
            && let Some(hovered) = &snap.hovered_droppable
        {
            if dw >= 1.0 && dh >= 1.0 {
                let prompt = if pad_mode {
                    format!("[Y / Triangle] Drop {} (held: {})", hovered.name, hovered.qty)
                } else {
                    format!("[Y] Drop {} (held: {})", hovered.name, hovered.qty)
                };
                let dl = ui.get_foreground_draw_list();
                let [tw, th] = ui.calc_text_size(&prompt);
                let cx = dw * 0.5;
                let cy = dh * 0.885;
                let (px, py) = (16.0, 7.0);
                let r = [cx - tw * 0.5 - px, cy - th * 0.5 - py, cx + tw * 0.5 + px, cy + th * 0.5 + py];
                dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.06, 0.05, 0.04, 0.88]).filled(true).rounding(6.0).build();
                dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.92, 0.86, 0.68, 0.85]).rounding(6.0).thickness(1.5).build();
                dl.add_text([cx - tw * 0.5, cy - th * 0.5], [1.0, 0.96, 0.84, 1.0], &prompt);
            }
            if pad_y_edge || (focused && ui.is_key_pressed_no_repeat(Key::Y)) {
                self.dialog = Some(DropDialog {
                    full_id: hovered.full_id,
                    name: hovered.name.clone(),
                    max_qty: hovered.qty,
                    qty: 1,
                    just_opened: true,
                });
                self.dialog_nav = DialogPadNav::default();
            }
        }

        // Auto-close dialog if Inventory menu closes.
        if self.dialog.is_some() && !snap.in_inventory_menu {
            self.dialog = None;
        }

        // Centered Drop Quantity Dialog (background Sekiro menu navigation is paused while open).
        if let Some(mut dlg) = self.dialog.take() {
            if let Some(live) = snap.items.iter().find(|it| it.full_id == dlg.full_id && it.qty > 0) {
                dlg.max_qty = live.qty;
                dlg.qty = dlg.qty.clamp(1, dlg.max_qty);

                let pad_delta = self.dialog_nav.step(pad, nav_now_ms());
                if pad_delta != 0 {
                    dlg.qty = (dlg.qty as i32 + pad_delta).clamp(1, dlg.max_qty as i32) as u32;
                }
                if pad_lb_edge {
                    dlg.qty = 1;
                }
                if pad_rb_edge {
                    dlg.qty = dlg.max_qty;
                }
                if focused {
                    if ui.is_key_pressed_no_repeat(Key::LeftArrow) || ui.is_key_pressed_no_repeat(Key::A) {
                        dlg.qty = dlg.qty.saturating_sub(1).max(1);
                    }
                    if ui.is_key_pressed_no_repeat(Key::RightArrow) || ui.is_key_pressed_no_repeat(Key::D) {
                        dlg.qty = dlg.qty.saturating_add(1).min(dlg.max_qty);
                    }
                    if ui.is_key_pressed_no_repeat(Key::UpArrow) || ui.is_key_pressed_no_repeat(Key::W) {
                        dlg.qty = dlg.qty.saturating_add(5).min(dlg.max_qty);
                    }
                    if ui.is_key_pressed_no_repeat(Key::DownArrow) || ui.is_key_pressed_no_repeat(Key::S) {
                        dlg.qty = dlg.qty.saturating_sub(5).max(1);
                    }
                    if ui.is_key_pressed_no_repeat(Key::Home) {
                        dlg.qty = 1;
                    }
                    if ui.is_key_pressed_no_repeat(Key::End) {
                        dlg.qty = dlg.max_qty;
                    }
                }

                let mut confirm = !dlg.just_opened
                    && (pad_a_edge
                        || pad_y_edge
                        || pad_r3_edge
                        || (focused
                            && (ui.is_key_pressed_no_repeat(Key::Enter)
                                || ui.is_key_pressed_no_repeat(Key::KeypadEnter)
                                || ui.is_key_pressed_no_repeat(Key::E)
                                || ui.is_key_pressed_no_repeat(Key::Y))));
                let mut cancel = pad_b_edge
                    || (focused
                        && (ui.is_key_pressed_no_repeat(Key::Escape)
                            || ui.is_key_pressed_no_repeat(Key::Backspace)));

                let confirm_lbl = if pad_mode {
                    "Confirm Drop  [A / Y]"
                } else {
                    "Confirm Drop  [Enter / Y]"
                };
                let cancel_lbl = if pad_mode { "Cancel  [B]" } else { "Cancel  [Esc]" };
                let (legend_1, legend_2) = if pad_mode {
                    (
                        "D-Pad / Sticks: L/R +/-1, U/D +/-5 | LB/RB: Min/Max",
                        "A / Y: Confirm  |  B: Cancel",
                    )
                } else {
                    (
                        "Arrows / WASD: L/R +/-1, U/D +/-5 | Home/End: Min/Max",
                        "Enter / Y: Confirm  |  Esc: Cancel",
                    )
                };

                ui.window("Drop Item##coop-drop-dialog")
                    .position([dw * 0.5, dh * 0.5], Condition::Always)
                    .position_pivot([0.5, 0.5])
                    .always_auto_resize(true)
                    .resizable(false)
                    .movable(false)
                    .collapsible(false)
                    .save_settings(false)
                    .bg_alpha(0.94)
                    .build(|| {
                        ui.text_colored(YELLOW, format!("Drop {}", dlg.name));
                        ui.text_disabled(format!("Held: {}", dlg.max_qty));
                        ui.separator();
                        if dlg.max_qty > 1 {
                            ui.set_next_item_width(280.0);
                            let mut v = dlg.qty as i32;
                            if ui.slider("##drop-qty-slider", 1, dlg.max_qty as i32, &mut v) {
                                dlg.qty = (v as u32).clamp(1, dlg.max_qty);
                            }
                        }
                        if ui.small_button("Min") {
                            dlg.qty = 1;
                        }
                        ui.same_line();
                        if ui.small_button("-5") {
                            dlg.qty = dlg.qty.saturating_sub(5).max(1);
                        }
                        ui.same_line();
                        if ui.small_button("-1") {
                            dlg.qty = dlg.qty.saturating_sub(1).max(1);
                        }
                        ui.same_line();
                        ui.text(format!("  {} / {}  ", dlg.qty, dlg.max_qty));
                        ui.same_line();
                        if ui.small_button("+1") {
                            dlg.qty = dlg.qty.saturating_add(1).min(dlg.max_qty);
                        }
                        ui.same_line();
                        if ui.small_button("+5") {
                            dlg.qty = dlg.qty.saturating_add(5).min(dlg.max_qty);
                        }
                        ui.same_line();
                        if ui.small_button("Max") {
                            dlg.qty = dlg.max_qty;
                        }
                        ui.spacing();
                        ui.separator();
                        ui.spacing();
                        if ui.button(confirm_lbl) {
                            confirm = true;
                        }
                        ui.same_line();
                        if ui.button(cancel_lbl) {
                            cancel = true;
                        }
                        ui.text_disabled(legend_1);
                        ui.text_disabled(legend_2);
                    });

                dlg.just_opened = false;
                if !cancel {
                    if confirm {
                        crate::itemdrop::post_drop_request(dlg.full_id, dlg.qty.clamp(1, dlg.max_qty));
                    } else {
                        self.dialog = Some(dlg);
                    }
                }
            }
        } else {
            self.dialog_nav = DialogPadNav::default();
        }

        // Sync Sekiro background input block immediately when the dialog opens or closes.
        crate::inputblock::set_dialog_open(self.dialog.is_some());

        let drawer_visible = self.open_hotkey;
        crate::itemdrop::set_panel_visible(drawer_visible || self.dialog.is_some());
        if !drawer_visible {
            self.nav = crate::menu::PadNav::default();
            return;
        }

        if snap.items.is_empty() {
            self.selected = 0;
            self.qty = 0;
            self.last_item = 0;
        } else {
            if self.selected >= snap.items.len() {
                self.selected = snap.items.len() - 1;
            }
            let cur = &snap.items[self.selected];
            if cur.full_id != self.last_item {
                self.last_item = cur.full_id;
                self.qty = 1;
            }
            self.qty = self.qty.clamp(1, cur.qty);
        }

        let mut drop_now = false;
        let pad_on = !self.collapsed && !settings_up && self.dialog.is_none();
        if pad_on {
            for act in self.nav.step(pad, nav_now_ms()) {
                match act {
                    NavAction::Move(dir) if !snap.items.is_empty() => {
                        let len = snap.items.len() as i32;
                        self.selected = (self.selected as i32 + dir).rem_euclid(len) as usize;
                        let cur = &snap.items[self.selected];
                        self.last_item = cur.full_id;
                        self.qty = 1.clamp(1, cur.qty);
                    }
                    NavAction::Adjust(n) if !snap.items.is_empty() => {
                        let max = snap.items[self.selected].qty;
                        let delta = if n >= 25 {
                            5
                        } else if n <= -25 {
                            -5
                        } else if n > 0 {
                            1
                        } else {
                            -1
                        };
                        self.qty = (self.qty as i32 + delta).clamp(1, max as i32) as u32;
                    }
                    NavAction::Apply => drop_now = true,
                    NavAction::Toggle => self.collapsed = !self.collapsed,
                    _ => {}
                }
            }
        } else {
            self.nav = crate::menu::PadNav::default();
        }

        let width = (dw * 0.19).clamp(270.0, 440.0);
        let (pos, pivot) = if settings_up { ([12.0, dh * 0.5], [0.0, 0.5]) } else { ([dw - 12.0, dh * 0.5], [1.0, 0.5]) };

        ui.window("Drop Item (Co-op)")
            .position(pos, Condition::Always)
            .position_pivot(pivot)
            .always_auto_resize(true)
            .resizable(false)
            .movable(false)
            .collapsible(false)
            .focus_on_appearing(false)
            .save_settings(false)
            .bg_alpha(0.86)
            .build(|| {
                ui.text("Drop Item  [F6]");
                ui.same_line();
                if ui.small_button(if self.collapsed { "Expand" } else { "Hide" }) {
                    self.collapsed = !self.collapsed;
                }
                if self.collapsed {
                    return;
                }
                ui.separator();
                if snap.items.is_empty() {
                    ui.text_disabled("No droppable consumables or upgrade materials.");
                } else {
                    ui.child_window("##coop-drop-list").size([width, 220.0]).border(true).build(|| {
                        for (i, it) in snap.items.iter().enumerate() {
                            let selected = i == self.selected;
                            let label = format!("{} (x{})##drop{i}", it.name, it.qty);
                            if ui.selectable_config(&label).selected(selected).build() {
                                self.selected = i;
                                self.last_item = it.full_id;
                                self.qty = 1.clamp(1, it.qty);
                            }
                        }
                    });
                    let cur = &snap.items[self.selected];
                    ui.text_colored(YELLOW, format!("Selected: {} (owned: {})", cur.name, cur.qty));
                    if ui.small_button("-5") {
                        self.qty = self.qty.saturating_sub(5).max(1);
                    }
                    ui.same_line();
                    if ui.small_button("-1") {
                        self.qty = self.qty.saturating_sub(1).max(1);
                    }
                    ui.same_line();
                    ui.text(format!("Qty: {}", self.qty));
                    ui.same_line();
                    if ui.small_button("+1") {
                        self.qty = self.qty.saturating_add(1).min(cur.qty);
                    }
                    ui.same_line();
                    if ui.small_button("+5") {
                        self.qty = self.qty.saturating_add(5).min(cur.qty);
                    }
                    ui.same_line();
                    if ui.small_button("All") {
                        self.qty = cur.qty;
                    }
                    if ui.button("Drop Item") {
                        drop_now = true;
                    }
                }
                if let Some(fb) = &snap.feedback {
                    ui.text_colored(YELLOW, fb);
                }
                if pad_mode {
                    ui.text_disabled("Tip: Hover item in Inventory and press Y / Triangle.");
                    if pad_on {
                        ui.text_disabled("Pad: R-Stick = select/qty, RT = Drop");
                    }
                } else {
                    ui.text_disabled("Tip: Hover item in Inventory and press Y.");
                    ui.text_disabled("Mouse: click item & quantity, or click Drop Item");
                }
            });

        if drop_now && let Some(cur) = snap.items.get(self.selected) {
            crate::itemdrop::post_drop_request(cur.full_id, self.qty.clamp(1, cur.qty));
        }
    }
}

/// The 'Mod settings' panel is on screen right now (render thread writes, `message_filter`
/// reads: it takes `&self`).
static PANEL_VISIBLE: AtomicBool = AtomicBool::new(false);

/// The 'Mod settings' panel is up (stuck.rs: Esc goes to it, not to the game's menu).
pub fn panel_visible() -> bool {
    PANEL_VISIBLE.load(Ordering::Relaxed)
}

/// The imgui key for a Windows virtual-key code (`ingame_menu_key`); Insert for anything
/// that has no mapping here.
fn vk_to_key(vk: u32) -> Key {
    const F: [Key; 12] = [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12];
    match vk {
        33 => Key::PageUp,
        34 => Key::PageDown,
        35 => Key::End,
        36 => Key::Home,
        46 => Key::Delete,
        112..=123 => F[(vk - 112) as usize],
        192 => Key::GraveAccent,
        _ => Key::Insert,
    }
}

fn vk_name(vk: u32) -> String {
    match vk {
        33 => "PgUp".into(),
        34 => "PgDn".into(),
        35 => "End".into(),
        36 => "Home".into(),
        46 => "Delete".into(),
        112..=123 => format!("F{}", vk - 111),
        192 => "`".into(),
        _ => "Insert".into(),
    }
}

/// Draft of the 'Mod settings' panel (render thread only). Edited locally; the Apply button
/// posts it to the game task (`menu::post_edit`), which applies it and writes the ini.
#[derive(Default)]
struct Panel {
    draft: crate::menu::Values,
    /// What the draft was last synced from: while `draft == synced` nothing is being edited
    /// and the draft follows the live values.
    synced: crate::menu::Values,
    was_visible: bool,
    collapsed: bool,
    /// Controller navigation (v8.32a): the selected row (0..6) and the pure navigator.
    selected: usize,
    nav: crate::menu::PadNav,
}

/// Milliseconds since the first call (clock of the pad navigator).
fn nav_now_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

impl Panel {
    fn draw(&mut self, ui: &Ui) {
        use crate::menu;
        let (role, mine, host, open) = menu::snapshot();
        let visible = tuning::ingame_menu() && open;
        PANEL_VISIBLE.store(visible, Ordering::Relaxed);
        if !visible {
            self.was_visible = false;
            return;
        }
        let editable = menu::can_edit(role);
        // A client sees the host's values (its own ini does not apply to it).
        let source = if editable { mine } else { host.unwrap_or(mine) };
        if !self.was_visible || self.draft == self.synced {
            self.draft = source;
        }
        self.synced = source;
        self.was_visible = true;

        let vk = tuning::ingame_menu_key();
        if ui.is_key_pressed_no_repeat(vk_to_key(vk)) {
            self.collapsed = !self.collapsed;
        }
        let [dw, dh] = ui.io().display_size;
        let width = (dw * 0.17).clamp(260.0, 520.0);
        let mut apply = false;
        // Controller: right stick / triggers / R3 only (render thread; XInputGetState only).
        let pad_on = editable && !self.collapsed && tuning::ingame_menu_pad() && !crate::inputblock::blocked();
        if pad_on {
            let now = nav_now_ms();
            for a in self.nav.step(crate::spectate_input::pad_state(), now) {
                let (sel, d, ap) = menu::apply_nav(self.selected, self.draft, a);
                self.selected = sel;
                self.draft = d;
                apply |= ap;
            }
        } else {
            self.nav = menu::PadNav::default();
        }
        let sel = if pad_on { Some(self.selected) } else { None };
        let mark = |ui: &Ui, row: usize| {
            if let Some(s) = sel {
                if s == row {
                    ui.text_colored(YELLOW, ">");
                } else {
                    ui.text(" ");
                }
                ui.same_line();
            }
        };
        ui.window("CooP Sekiro settings")
            .position([dw - 12.0, dh * 0.5], Condition::Always)
            .position_pivot([1.0, 0.5])
            .always_auto_resize(true)
            .resizable(false)
            .movable(false)
            .collapsible(false)
            .focus_on_appearing(false)
            .save_settings(false)
            .bg_alpha(0.85)
            .build(|| {
                if self.collapsed {
                    ui.text(format!("[{}] expand", vk_name(vk)));
                    return;
                }
                if editable {
                    ui.text(format!("Host settings  [{}] collapse", vk_name(vk)));
                    if pad_on {
                        ui.text_wrapped("Pad: right stick up/down = row, left/right = value (LB/RB = x5), RT = Apply, R3 = toggle");
                    }
                } else {
                    ui.text_colored(YELLOW, "Set by the host");
                    if host.is_none() {
                        ui.text("Waiting for the host's values...");
                    }
                }
                ui.separator();
                let d = &mut self.draft;
                ui.disabled(!editable, || {
                    mark(ui, 0);
                    ui.set_next_item_width(width * 0.55);
                    let mut v = d.enemy_hp as i32;
                    ui.slider("Enemy HP %", 0, 1000, &mut v);
                    d.enemy_hp = menu::clamp_pct(v as i64);
                    mark(ui, 1);
                    ui.set_next_item_width(width * 0.55);
                    let mut v = d.enemy_posture as i32;
                    ui.slider("Enemy posture %", 0, 1000, &mut v);
                    d.enemy_posture = menu::clamp_pct(v as i64);
                    mark(ui, 2);
                    ui.set_next_item_width(width * 0.55);
                    let mut v = d.boss_hp as i32;
                    ui.slider("Boss HP %", 0, 1000, &mut v);
                    d.boss_hp = menu::clamp_pct(v as i64);
                    mark(ui, 3);
                    ui.set_next_item_width(width * 0.55);
                    let mut v = d.boss_posture as i32;
                    ui.slider("Boss posture %", 0, 1000, &mut v);
                    d.boss_posture = menu::clamp_pct(v as i64);
                    mark(ui, 4);
                    ui.checkbox("Death debuff (-10 % max HP/posture per death)", &mut d.death_debuffs);
                    mark(ui, 5);
                    if editable && ui.button("Apply") {
                        apply = true;
                    }
                });
                if editable {
                    ui.text_disabled("Percent added per extra player. Ctrl+click a slider to type.");
                    let r = menu::save_result();
                    if !r.is_empty() {
                        ui.text_wrapped(r);
                    }
                }
            });
        if apply {
            menu::post_edit(self.draft);
        }
    }
}

struct Camera {
    rows: [[f32; 4]; 4],
    fov: f32,
    near: f32,
    far: f32,
}

/// Render camera (right, up, forward, position rows; vertical FOV), None on any failed read.
/// Spectate lock-on (v8.10): a white dot in a dark ring on the locked target, like the
/// game's own lock-on marker while alive.
fn draw_lock_mark(ui: &Ui) {
    let Some(p) = crate::spectate_cam::lock_mark() else { return };
    let Some(cam) = read_camera() else { return };
    let [w, h] = ui.io().display_size;
    if w < 1.0 || h < 1.0 {
        return;
    }
    let Some([sx, sy]) = project(&cam.rows, cam.fov, [w, h], p) else { return };
    let dl = ui.get_foreground_draw_list();
    dl.add_circle([sx, sy], 9.0, [0.0, 0.0, 0.0, 0.55]).thickness(3.0).num_segments(24).build();
    dl.add_circle([sx, sy], 5.5, [1.0, 1.0, 1.0, 0.95]).filled(true).num_segments(20).build();
    dl.add_circle([sx, sy], 5.5, [0.85, 0.15, 0.1, 0.9]).thickness(1.5).num_segments(20).build();
}

/// World point -> screen pixel with the render camera (None behind it or outside wide guard bounds).
fn project_unclipped(m: &[[f32; 4]; 4], fov: f32, [w, h]: [f32; 2], p: [f32; 3]) -> Option<[f32; 2]> {
    let aspect = w / h;
    let vfov = if aspect < 16.0 / 9.0 { 2.0 * ((fov * 0.5).tan() * (16.0 / 9.0) / aspect).atan() } else { fov };
    let d = [p[0] - m[3][0], p[1] - m[3][1], p[2] - m[3][2]];
    let dot = |r: &[f32; 4]| d[0] * r[0] + d[1] * r[1] + d[2] * r[2];
    let (x, y, z) = (dot(&m[0]), dot(&m[1]), dot(&m[2]));
    if z < 0.2 {
        return None;
    }
    let t = (vfov * 0.5).tan();
    let sx = (0.5 + 0.5 * x / (z * t * aspect)) * w;
    let sy = (0.5 - 0.5 * y / (z * t)) * h;
    (sx.is_finite()
        && sy.is_finite()
        && (-w..=2.0 * w).contains(&sx)
        && (-h..=2.0 * h).contains(&sy))
    .then_some([sx, sy])
}

/// World point -> screen pixel with the render camera (None behind it or off screen).
fn project(m: &[[f32; 4]; 4], fov: f32, [w, h]: [f32; 2], p: [f32; 3]) -> Option<[f32; 2]> {
    let [sx, sy] = project_unclipped(m, fov, [w, h], p)?;
    let d = [p[0] - m[3][0], p[1] - m[3][1], p[2] - m[3][2]];
    let z = d[0] * m[2][0] + d[1] * m[2][1] + d[2] * m[2][2];
    (z >= 0.3 && (0.0..=w).contains(&sx) && (0.0..=h).contains(&sy)).then_some([sx, sy])
}

fn draw_filled_quad(dl: &hudhook::imgui::DrawListMut<'_>, p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], col: [f32; 4]) {
    dl.add_triangle(p0, p1, p2, col).filled(true).build();
    dl.add_triangle(p0, p2, p3, col).filled(true).build();
}

fn read_camera() -> Option<Camera> {
    use crate::game::{module_base, read, read_ptr};
    let base = module_base();
    if base == 0 {
        return None;
    }
    let field_area = read_ptr(base + 0x3D5C0A0).filter(|&p| p != 0)?;
    let game_rend = read_ptr(field_area + 0x20).filter(|&p| p != 0)?;
    let cam = read_ptr(game_rend + 0x18).filter(|&p| p != 0)?;
    let rows: [[f32; 4]; 4] = read(cam + 0x10)?;
    let fov: f32 = read(cam + 0x50)?;
    if rows.iter().flatten().any(|v| !v.is_finite()) || !fov.is_finite() || !(0.05..=3.0).contains(&fov) {
        return None;
    }
    let near = read::<f32>(cam + 0x58)
        .filter(|&v| v.is_finite() && v > 0.001 && v < 10.0)
        .unwrap_or(0.08);
    let far = read::<f32>(cam + 0x5C)
        .filter(|&v| v.is_finite() && v > near + 1.0)
        .unwrap_or(10_000.0);
    Some(Camera { rows, fov, near, far })
}


impl Overlay {
    fn draw_item_drops(&self, ui: &Ui, snap: &crate::itemdrop::UiSnapshot) {
        if !snap.connected || (snap.markers.is_empty() && snap.nearby_drop.is_none()) {
            return;
        }
        let [w, h] = ui.io().display_size;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let cam = if snap.markers.is_empty() { None } else { read_camera() };
        let dl = ui.get_foreground_draw_list();
        let mut depth_started = false;
        if let Some(cam) = cam {
            let m = cam.rows;
            let fov = cam.fov;
            let lh = ui.calc_text_size("Ag")[1];
            let t_sec = nav_now_ms() as f32 * 0.001;
            let cam_pos = [m[3][0], m[3][1], m[3][2]];
            let cam_right = [m[0][0], m[0][1], m[0][2]];
            let cam_fwd = [m[2][0], m[2][1], m[2][2]];
            let view_z = |wp: [f32; 3]| -> f32 {
                (wp[0] - cam_pos[0]) * cam_fwd[0]
                    + (wp[1] - cam_pos[1]) * cam_fwd[1]
                    + (wp[2] - cam_pos[2]) * cam_fwd[2]
            };

            // Keep horizontal billboard right vector upright around world Y so the pouch stands naturally.
            let xz_len = cam_right[0].hypot(cam_right[2]);
            let right_xz = if xz_len > 1e-3 {
                [cam_right[0] / xz_len, 0.0, cam_right[2] / xz_len]
            } else {
                cam_right
            };

            // Project and sort markers back-to-front so nearer loot draws cleanly over farther loot.
            let mut visible: Vec<(f32, &crate::itemdrop::DropMarker, [f32; 2])> = snap
                .markers
                .iter()
                .filter_map(|mk| {
                    let cz = view_z(mk.pos);
                    if cz < cam.near {
                        return None;
                    }
                    let anchor_pt = project(&m, fov, [w, h], [mk.pos[0], mk.pos[1] + 0.14, mk.pos[2]])
                        .or_else(|| project(&m, fov, [w, h], [mk.pos[0], mk.pos[1] + 0.65, mk.pos[2]]))?;
                    Some((cz, mk, anchor_pt))
                })
                .collect();
            visible.sort_by(|a, b| b.0.total_cmp(&a.0));

            for (cz, mk, [anchor_sx, anchor_sy]) in visible {
                let p = mk.pos;
                let seed = (mk.drop_id as u32 % 997) as f32 * 0.017;
                let pulse = 0.88 + 0.12 * (t_sec * 2.4 + seed).sin();

                let pouch_z = (cz - 0.09).max(cam.near);
                if crate::depth::push_depth_begin(pouch_z, cam.near, cam.far, 0.06) {
                    depth_started = true;
                }

                let to_cam_x = cam_pos[0] - p[0];
                let to_cam_z = cam_pos[2] - p[2];
                let to_cam_len = to_cam_x.hypot(to_cam_z);
                let to_cam_xz = if to_cam_len > 1e-3 {
                    [to_cam_x / to_cam_len, 0.0, to_cam_z / to_cam_len]
                } else {
                    [0.0, 0.0, 1.0]
                };

                // Soft oval ground shadow directly under the golden cloth pouch.
                if let Some(sc) = project_unclipped(&m, fov, [w, h], [p[0], p[1] + 0.005, p[2]]) {
                    let mut ring = [None; 8];
                    for (i, slot) in ring.iter_mut().enumerate() {
                        let a = (i as f32) * std::f32::consts::TAU / 8.0;
                        let wp = [p[0] + a.cos() * 0.125, p[1] + 0.005, p[2] + a.sin() * 0.105];
                        *slot = project_unclipped(&m, fov, [w, h], wp);
                    }
                    for i in 0..8 {
                        if let (Some(a), Some(b)) = (ring[i], ring[(i + 1) % 8]) {
                            dl.add_triangle(sc, a, b, [0.0, 0.0, 0.0, 0.44]).filled(true).build();
                        }
                    }
                }

                // Warm golden-amber cloth loot pouch silhouette (vanilla Sekiro ground loot proportions).
                let pouch_rings: [(f32, f32, [f32; 3]); 7] = [
                    (0.006, 0.056, [0.52, 0.31, 0.08]), // grounded amber base
                    (0.044, 0.085, [0.78, 0.50, 0.14]), // lower honey-gold belly
                    (0.098, 0.087, [0.90, 0.63, 0.18]), // mid belly (paper seal)
                    (0.146, 0.066, [0.96, 0.74, 0.24]), // upper sunlit gold shoulder
                    (0.176, 0.032, [0.82, 0.56, 0.16]), // cinched bronze-gold neck
                    (0.208, 0.052, [1.00, 0.84, 0.34]), // flared bright gathered top
                    (0.238, 0.036, [1.00, 0.90, 0.48]), // radiant top edge lit by flame
                ];
                let mut proj_rings: [Option<[[f32; 2]; 4]>; 7] = [None; 7];
                for (r_idx, &(yo, hw, _)) in pouch_rings.iter().enumerate() {
                    let y = p[1] + yo;
                    let fracs = [-1.0f32, -0.45, 0.45, 1.0];
                    let mut pts = [[0.0f32; 2]; 4];
                    let mut ok = true;
                    for (j, &f) in fracs.iter().enumerate() {
                        let wp = [
                            p[0] + right_xz[0] * (hw * f),
                            y,
                            p[2] + right_xz[2] * (hw * f),
                        ];
                        if let Some(sp) = project_unclipped(&m, fov, [w, h], wp) {
                            pts[j] = sp;
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        proj_rings[r_idx] = Some(pts);
                    }
                }
                for r_idx in 0..(pouch_rings.len() - 1) {
                    if let (Some(lo), Some(hi)) = (proj_rings[r_idx], proj_rings[r_idx + 1]) {
                        let (_, _, rgb0) = pouch_rings[r_idx];
                        let (_, _, rgb1) = pouch_rings[r_idx + 1];
                        let mid = [
                            (rgb0[0] + rgb1[0]) * 0.5,
                            (rgb0[1] + rgb1[1]) * 0.5,
                            (rgb0[2] + rgb1[2]) * 0.5,
                        ];
                        let col_l = [mid[0] * 0.68, mid[1] * 0.54, mid[2] * 0.36, 0.97];
                        let col_c = [mid[0], mid[1], mid[2], 0.98];
                        let col_r = [mid[0] * 0.78, mid[1] * 0.64, mid[2] * 0.44, 0.97];
                        draw_filled_quad(&dl, lo[0], lo[1], hi[1], hi[0], col_l);
                        draw_filled_quad(&dl, lo[1], lo[2], hi[2], hi[1], col_c);
                        draw_filled_quad(&dl, lo[2], lo[3], hi[3], hi[2], col_r);
                    }
                }
                // Warm dark-bronze tie cord around the neck (y = 0.170..0.184).
                if let (Some(c0), Some(c1), Some(c2), Some(c3)) = (
                    project_unclipped(&m, fov, [w, h], [p[0] - right_xz[0] * 0.038, p[1] + 0.170, p[2] - right_xz[2] * 0.038]),
                    project_unclipped(&m, fov, [w, h], [p[0] + right_xz[0] * 0.038, p[1] + 0.170, p[2] + right_xz[2] * 0.038]),
                    project_unclipped(&m, fov, [w, h], [p[0] + right_xz[0] * 0.038, p[1] + 0.184, p[2] + right_xz[2] * 0.038]),
                    project_unclipped(&m, fov, [w, h], [p[0] - right_xz[0] * 0.038, p[1] + 0.184, p[2] - right_xz[2] * 0.038]),
                ) {
                    draw_filled_quad(&dl, c0, c1, c2, c3, [0.26, 0.15, 0.05, 0.96]);
                }
                // Warm dark-amber outer silhouette edge lines.
                let outline_col = [0.22, 0.13, 0.04, 0.78];
                for r_idx in 0..(pouch_rings.len() - 1) {
                    if let (Some(lo), Some(hi)) = (proj_rings[r_idx], proj_rings[r_idx + 1]) {
                        dl.add_line(lo[0], hi[0], outline_col).thickness(1.1).build();
                        dl.add_line(lo[3], hi[3], outline_col).thickness(1.1).build();
                    }
                }

                // Crisp circular white paper seal medallion on the front belly of the pouch.
                let seal_world = [
                    p[0] + to_cam_xz[0] * 0.025,
                    p[1] + 0.092,
                    p[2] + to_cam_xz[2] * 0.025,
                ];
                if let Some(seal_px) = project_unclipped(&m, fov, [w, h], seal_world) {
                    let seal_edge = [
                        seal_world[0] + right_xz[0] * 0.043,
                        seal_world[1],
                        seal_world[2] + right_xz[2] * 0.043,
                    ];
                    let r_seal = project_unclipped(&m, fov, [w, h], seal_edge)
                        .map(|sp| (sp[0] - seal_px[0]).hypot(sp[1] - seal_px[1]))
                        .unwrap_or(0.0);
                    if r_seal >= 1.4 {
                        let r_seal = r_seal.min(24.0);
                        dl.add_circle(seal_px, r_seal, [0.14, 0.11, 0.08, 0.96])
                            .filled(true)
                            .num_segments(18)
                            .build();
                        dl.add_circle(seal_px, r_seal * 0.84, [0.95, 0.92, 0.83, 0.97])
                            .filled(true)
                            .num_segments(18)
                            .build();
                        if r_seal >= 3.2 {
                            dl.add_circle(seal_px, r_seal * 0.28, [0.30, 0.25, 0.19, 0.60])
                                .filled(true)
                                .num_segments(10)
                                .build();
                        }
                    }
                }

                // Warm golden radiant glow blooming from the pouch mouth.
                let neck_world = [p[0], p[1] + 0.228, p[2]];
                if let Some(neck_px) = project_unclipped(&m, fov, [w, h], neck_world) {
                    let side_world = [
                        neck_world[0] + right_xz[0] * 0.13,
                        neck_world[1],
                        neck_world[2] + right_xz[2] * 0.13,
                    ];
                    let r_px = project_unclipped(&m, fov, [w, h], side_world)
                        .map(|sp| ((sp[0] - neck_px[0]).hypot(sp[1] - neck_px[1])).clamp(3.0, 26.0))
                        .unwrap_or(8.0);
                    dl.add_circle(neck_px, r_px * 1.50, [0.96, 0.74, 0.22, 0.16 * pulse])
                        .filled(true)
                        .num_segments(20)
                        .build();
                    dl.add_circle(neck_px, r_px * 0.86, [1.00, 0.85, 0.35, 0.34 * pulse])
                        .filled(true)
                        .num_segments(16)
                        .build();
                    dl.add_circle(neck_px, (r_px * 0.38).max(1.8), [1.00, 0.96, 0.74, 0.68 * pulse])
                        .filled(true)
                        .num_segments(12)
                        .build();
                }

                // Tapered upward golden flame plume + luminous central ray (wider base -> crisp tip).
                let beam_layers = [
                    (5usize, 0.20f32, 0.96f32, 0.052f32, 0.095f32, 0.22f32, [0.98f32, 0.78, 0.24]),
                    (4usize, 0.20f32, 0.88f32, 0.032f32, 0.022f32, 0.36f32, [1.00f32, 0.93, 0.58]),
                ];
                for (segs, y_bot, y_top, hw_bot, hw_top, base_alpha, rgb) in beam_layers {
                    for s in 0..segs {
                        let f0 = s as f32 / segs as f32;
                        let f1 = (s + 1) as f32 / segs as f32;
                        let sy0 = p[1] + y_bot + (y_top - y_bot) * f0;
                        let sy1 = p[1] + y_bot + (y_top - y_bot) * f1;
                        let w0 = hw_bot + (hw_top - hw_bot) * f0;
                        let w1 = hw_bot + (hw_top - hw_bot) * f1;
                        let a_mid = base_alpha * pulse * ((1.0 - f0) * (1.0 - f0) + (1.0 - f1) * (1.0 - f1)) * 0.5;
                        let bl = [p[0] - right_xz[0] * w0, sy0, p[2] - right_xz[2] * w0];
                        let br = [p[0] + right_xz[0] * w0, sy0, p[2] + right_xz[2] * w0];
                        let tr = [p[0] + right_xz[0] * w1, sy1, p[2] + right_xz[2] * w1];
                        let tl = [p[0] - right_xz[0] * w1, sy1, p[2] - right_xz[2] * w1];
                        if let (Some(q0), Some(q1), Some(q2), Some(q3)) = (
                            project_unclipped(&m, fov, [w, h], bl),
                            project_unclipped(&m, fov, [w, h], br),
                            project_unclipped(&m, fov, [w, h], tr),
                            project_unclipped(&m, fov, [w, h], tl),
                        ) {
                            draw_filled_quad(&dl, q0, q1, q2, q3, [rgb[0], rgb[1], rgb[2], a_mid]);
                        }
                    }
                }
                for s in 0..4usize {
                    let f0 = s as f32 / 4.0;
                    let f1 = (s + 1) as f32 / 4.0;
                    let y0 = p[1] + 0.20 + 0.90 * f0;
                    let y1 = p[1] + 0.20 + 0.90 * f1;
                    let a_seg = 0.68 * pulse * (1.0 - (f0 + f1) * 0.5);
                    if let (Some(rp0), Some(rp1)) = (
                        project_unclipped(&m, fov, [w, h], [p[0], y0, p[2]]),
                        project_unclipped(&m, fov, [w, h], [p[0], y1, p[2]]),
                    ) {
                        dl.add_line(rp0, rp1, [1.00, 0.96, 0.74, a_seg]).thickness(1.5).build();
                    }
                }

                // Orbiting warm cream-gold fluttering butterflies + rising golden sparkles.
                let bfly_params: [(f32, f32, f32); 3] = [
                    (0.15, 0.39, 0.88),
                    (0.21, 0.62, -0.74),
                    (0.17, 0.84, 1.06),
                ];
                for (i, &(rad, y_base, speed)) in bfly_params.iter().enumerate() {
                    let phase_ang = t_sec * speed + seed + (i as f32) * 2.094;
                    let bx = p[0] + phase_ang.cos() * rad;
                    let bz = p[2] + phase_ang.sin() * rad;
                    let by = p[1] + y_base + 0.065 * (t_sec * 1.9 + seed + i as f32).sin();
                    let bw = [bx, by, bz];
                    if let Some(bp) = project_unclipped(&m, fov, [w, h], bw) {
                        let ref_w = [
                            bx + right_xz[0] * 0.044,
                            by,
                            bz + right_xz[2] * 0.044,
                        ];
                        let scale = project_unclipped(&m, fov, [w, h], ref_w)
                            .map(|sp| (sp[0] - bp[0]).hypot(sp[1] - bp[1]))
                            .unwrap_or(0.0);
                        if scale >= 1.5 {
                            let s_px = scale.clamp(2.4, 13.5);
                            let flap = 0.25 + 0.75 * (t_sec * 10.5 + seed * 3.0 + (i as f32) * 2.1).sin().abs();
                            let wx = s_px * flap;
                            let wy = s_px * 0.68;
                            let tilt = 0.24 * (phase_ang.sin());
                            let (ct, st) = (tilt.cos(), tilt.sin());
                            let rot = |dx: f32, dy: f32| -> [f32; 2] {
                                [bp[0] + dx * ct - dy * st, bp[1] + dx * st + dy * ct]
                            };
                            dl.add_circle(bp, s_px * 0.82, [1.00, 0.88, 0.42, 0.20])
                                .filled(true)
                                .num_segments(10)
                                .build();
                            let top = rot(0.0, -wy * 0.25);
                            let bot = rot(0.0, wy * 0.45);
                            let mid = rot(0.0, 0.0);
                            // Upper left & right wings
                            dl.add_triangle(top, rot(-wx, -wy * 0.90), rot(-wx * 0.75, wy * 0.12), [1.00, 0.97, 0.84, 0.92])
                                .filled(true)
                                .build();
                            dl.add_triangle(top, rot(wx, -wy * 0.90), rot(wx * 0.75, wy * 0.12), [1.00, 0.97, 0.84, 0.92])
                                .filled(true)
                                .build();
                            // Lower left & right hindwings
                            dl.add_triangle(mid, rot(-wx * 0.62, wy * 0.10), rot(-wx * 0.42, wy * 0.78), [1.00, 0.91, 0.58, 0.74])
                                .filled(true)
                                .build();
                            dl.add_triangle(mid, rot(wx * 0.62, wy * 0.10), rot(wx * 0.42, wy * 0.78), [1.00, 0.91, 0.58, 0.74])
                                .filled(true)
                                .build();
                            dl.add_line(top, bot, [0.55, 0.40, 0.18, 0.75]).thickness(1.0).build();
                        }
                    }
                }
                for i in 0..6 {
                    let phase = (t_sec * 0.42 + i as f32 * 0.166 + seed).fract();
                    let sway = (0.05 + 0.04 * (i % 2) as f32) * (phase * std::f32::consts::TAU + i as f32 * 1.3).sin();
                    let my = p[1] + 0.20 + phase * 0.92;
                    let mote = [p[0] + right_xz[0] * sway, my, p[2] + right_xz[2] * sway];
                    if let Some(mp) = project_unclipped(&m, fov, [w, h], mote) {
                        let ma = 0.72 * (phase * std::f32::consts::PI).sin();
                        let mr = 1.3 + 0.4 * ((i % 3) as f32);
                        dl.add_circle(mp, mr, [1.00, 0.88, 0.34, ma]).filled(true).num_segments(8).build();
                    }
                }

                // Floating item name tag above the golden beam (depth-tested against world geometry).
                if mk.dist_m <= 25.0 {
                    let label_world = [p[0], p[1] + 1.18, p[2]];
                    if let Some([lsx, lsy]) = project(&m, fov, [w, h], label_world) {
                        let label_z = (view_z(label_world) - 0.04).max(cam.near);
                        if crate::depth::push_depth_begin(label_z, cam.near, cam.far, 0.06) {
                            depth_started = true;
                        }
                        let tw = ui.calc_text_size(&mk.label)[0];
                        let pad = 5.0;
                        let sx = lsx
                            .clamp(tw * 0.5 + pad + 4.0, w - tw * 0.5 - pad - 4.0);
                        let _ = anchor_sx;
                        let _ = anchor_sy;
                        let ty = (lsy - lh).clamp(6.0, h - lh - 6.0);
                        dl.add_rect([sx - tw * 0.5 - pad, ty - 2.0], [sx + tw * 0.5 + pad, ty + lh + 2.0], [0.05, 0.04, 0.02, 0.76])
                            .filled(true)
                            .rounding(4.0)
                            .build();
                        dl.add_rect([sx - tw * 0.5 - pad, ty - 2.0], [sx + tw * 0.5 + pad, ty + lh + 2.0], [0.96, 0.80, 0.36, 0.72])
                            .rounding(4.0)
                            .thickness(1.0)
                            .build();
                        dl.add_text([sx - tw * 0.5, ty], [1.00, 0.95, 0.76, 1.0], &mk.label);
                    }
                }
            }
        }
        if depth_started {
            crate::depth::push_depth_end();
        } else if !snap.markers.is_empty() {
            crate::depth::finish_frame_without_callbacks();
        }
        if let Some((name, qty)) = &snap.nearby_drop {
            let prompt = if is_pad_mode() {
                format!("[X / Square] Pick up {name} x{qty}")
            } else {
                format!("[E] Pick up {name} x{qty}")
            };
            let [tw, th] = ui.calc_text_size(&prompt);
            let cx = w * 0.5;
            let cy = h * 0.80;
            let px = 14.0;
            let py = 7.0;
            let r = [cx - tw * 0.5 - px, cy - th * 0.5 - py, cx + tw * 0.5 + px, cy + th * 0.5 + py];
            dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.05, 0.05, 0.05, 0.85]).filled(true).rounding(6.0).build();
            dl.add_rect([r[0], r[1]], [r[2], r[3]], [0.96, 0.80, 0.36, 0.85]).rounding(6.0).thickness(1.5).build();
            dl.add_text([cx - tw * 0.5, cy - th * 0.5], [1.0, 0.97, 0.86, 1.0], &prompt);
        }
    }

    /// Party HP list for `party_hp_position` left / top (head mode is drawn by `draw_tag`).
    /// "L2 R2 Page n/m" under the Co-op Settings page: two drawn trigger icons and the page number (the game has
    /// no picture of the triggers for custom pages, and the key guide row of a page is fixed).
    fn draw_page_hint(&self, ui: &Ui) {
        let Some((cur, total)) = crate::optmenu::page_hint() else { return };
        let [w, h] = ui.io().display_size;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let k = h / 1080.0;
        let dl = ui.get_foreground_draw_list();
        let (bw, bh) = (58.0 * k, 30.0 * k);
        let y = h * 0.935;
        let mut x = w * 0.58;
        for name in ["L2", "R2"] {
            let (x1, y1) = (x + bw, y + bh);
            dl.add_rect([x, y], [x1, y1], [0.08, 0.08, 0.08, 0.9]).filled(true).rounding(bh * 0.45).build();
            dl.add_rect([x, y], [x1, y1], [0.85, 0.85, 0.85, 0.95]).rounding(bh * 0.45).thickness(2.0 * k).build();
            let tw = ui.calc_text_size(name);
            dl.add_text([x + (bw - tw[0]) * 0.5, y + (bh - tw[1]) * 0.5], [1.0, 1.0, 1.0, 1.0], name);
            x = x1 + 10.0 * k;
        }
        let label = format!("Page {cur}/{total}");
        let tw = ui.calc_text_size(&label);
        dl.add_text([x + 6.0 * k, y + (bh - tw[1]) * 0.5], [1.0, 1.0, 1.0, 0.95], &label);
    }

    fn draw_party(&self, ui: &Ui, status: &Status) {
        let pos = settings::party_hp_position();
        if !settings::party_hp_bars() || status.party.is_empty() || pos == settings::PARTY_POS_HEAD {
            return;
        }
        let [w, h] = ui.io().display_size;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let content = settings::party_hp_content();
        let k = h / 1080.0 * settings::party_hp_scale() as f32 / 100.0;
        let (x, y0) = row_origin(pos, w, h);
        let (bar_w, bar_h) = (220.0 * k, (9.0 * k).max(2.0));
        let dl = ui.get_foreground_draw_list();
        let text_h = ui.calc_text_size("Ag")[1];
        let show_name = content == settings::PARTY_CONTENT_NAME_HP;
        let row_h = if show_name { 40.0 * k.max(h / 1080.0) } else { bar_h + 14.0 * k };
        for (i, m) in status.party.iter().enumerate() {
            let y = y0 + i as f32 * row_h;
            let mut by = y;
            if show_name {
                dl.add_text([x, y], [1.0, 1.0, 1.0, 0.95], &m.name);
                if let Some(t) = state_tag(m.state) {
                    let nw = ui.calc_text_size(&m.name)[0];
                    dl.add_text([x + nw + 6.0 * k, y], [0.6, 0.6, 0.6, 0.9], t);
                }
                by = y + text_h + 2.0 * k;
            }
            draw_bar(&dl, [x, by, x + bar_w, by + bar_h], m);
            // Right of the bar: "hp / max", and the state when the name is not shown.
            let mut side = String::new();
            if content == settings::PARTY_CONTENT_HP_NUMBERS {
                side = hp_numbers(m);
            }
            if !show_name && let Some(t) = state_tag(m.state) {
                if !side.is_empty() {
                    side.push(' ');
                }
                side.push_str(t);
            }
            if !side.is_empty() {
                let th = ui.calc_text_size(&side)[1];
                dl.add_text([x + bar_w + 6.0 * k, by + bar_h * 0.5 - th * 0.5], [0.9, 0.9, 0.9, 0.9], &side);
            }
        }
    }

    /// `party_hp_position = head`: the bar under the nameplate plate (`plate` = its rect).
    fn draw_head_bar(ui: &Ui, status: &Status, tag: &FriendTag, plate: [f32; 4], h: f32) {
        if !settings::party_hp_bars() || settings::party_hp_position() != settings::PARTY_POS_HEAD {
            return;
        }
        // The bar of the peer this nameplate belongs to (each peer its own).
        let Some(m) = member_of(&status.party, tag.id) else { return };
        let rect = head_bar_rect(plate, h, settings::party_hp_scale());
        let dl = ui.get_foreground_draw_list();
        draw_bar(&dl, rect, m);
        // The state under the nameplate too ("(in cutscene)" ...), when the row has one.
        if settings::party_hp_content() != settings::PARTY_CONTENT_HP_NUMBERS
            && let Some(t) = state_tag(m.state)
        {
            let th = ui.calc_text_size(t)[1];
            let k = h / 1080.0 * settings::party_hp_scale() as f32 / 100.0;
            dl.add_text([rect[2] + 5.0 * k, (rect[1] + rect[3]) * 0.5 - th * 0.5], [0.6, 0.6, 0.6, 0.9], t);
        }
        if settings::party_hp_content() == settings::PARTY_CONTENT_HP_NUMBERS {
            let text = hp_numbers(m);
            let th = ui.calc_text_size(&text)[1];
            let k = h / 1080.0 * settings::party_hp_scale() as f32 / 100.0;
            dl.add_text([rect[2] + 5.0 * k, (rect[1] + rect[3]) * 0.5 - th * 0.5], [0.9, 0.9, 0.9, 0.9], &text);
        }
    }

    /// Every nameplate: projects each head point with the render camera.
    fn draw_tags(&self, ui: &Ui, status: &Status) {
        // overhead_player_display: 0 name + ping, 1 name only, 2 nothing (arrow included).
        let mode = settings::overhead_display();
        if mode >= 2 || status.friend_tags.is_empty() {
            return;
        }
        let Some(cam) = read_camera() else { return };
        let [w, h] = ui.io().display_size;
        if w < 1.0 || h < 1.0 {
            return;
        }
        // Off-screen markers already drawn this frame (their plates), so the next peer's
        // marker that points the same way is stacked beside them instead of on top.
        let mut placed: Vec<[f32; 4]> = Vec::new();
        for tag in &status.friend_tags {
            Self::draw_tag(ui, status, tag, mode, &cam.rows, cam.fov, [w, h], &mut placed);
        }
    }

    /// One nameplate, or its edge marker when off screen.
    #[allow(clippy::too_many_arguments)]
    fn draw_tag(ui: &Ui, status: &Status, tag: &FriendTag, mode: u8, m: &[[f32; 4]; 4], fov: f32, [w, h]: [f32; 2], placed: &mut Vec<[f32; 4]>) {
        let aspect = w / h;
        // The game keeps the 16:9 horizontal FOV on narrower screens (0xF01260).
        let vfov = if aspect < 16.0 / 9.0 {
            2.0 * ((fov * 0.5).tan() * (16.0 / 9.0) / aspect).atan()
        } else {
            fov
        };
        let p = [tag.pos[0], tag.pos[1] + 1.9, tag.pos[2]];
        let d = [p[0] - m[3][0], p[1] - m[3][1], p[2] - m[3][2]];
        let dot = |r: &[f32; 4]| d[0] * r[0] + d[1] * r[1] + d[2] * r[2];
        let (x, y, z) = (dot(&m[0]), dot(&m[1]), dot(&m[2]));
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let t = (vfov * 0.5).tan();
        if z < 0.3 {
            // Behind the camera: point the edge marker the way to turn.
            let v = if x.abs() + y.abs() < 1e-3 { [0.0, 1.0] } else { [x, -y] };
            if let Some(plate) = draw_edge_marker(ui, tag, v, dist, w, h, placed) {
                Self::draw_head_bar(ui, status, tag, plate, h);
            }
            return;
        }
        let sx = (0.5 + 0.5 * x / (z * t * aspect)) * w;
        let sy = (0.5 - 0.5 * y / (z * t)) * h;
        if !(sx.is_finite() && sy.is_finite()) {
            return;
        }
        if sx < 0.0 || sx > w || sy < 0.0 || sy > h {
            if let Some(plate) = draw_edge_marker(ui, tag, [sx - w * 0.5, sy - h * 0.5], dist, w, h, placed) {
                Self::draw_head_bar(ui, status, tag, plate, h);
            }
            return;
        }
        // Visible at any distance in our map block (owner 2026-10-24, like ERSC; it faded out
        // between 100 and 150 m before). The text is drawn at the font's fixed pixel size, so
        // it never shrinks with distance; far away it only gets a little lighter, never below
        // 85 %, and shows the distance next to the name.
        let alpha = if dist <= FADE_FROM_M { 1.0 } else { (1.0 - (dist - FADE_FROM_M) / 1000.0).clamp(MIN_FAR_ALPHA, 1.0) };
        let far_name;
        let name: &str = if dist >= SHOW_DIST_FROM_M {
            far_name = format!("{}  {:.0} m", tag.name, dist);
            &far_name
        } else {
            &tag.name
        };
        let dl = ui.get_foreground_draw_list();
        let bg_col = [0.0, 0.0, 0.0, 0.6 * alpha];
        let name_col = [1.0, 0.92, 0.45, alpha];
        let ping_col = [0.8, 0.8, 0.8, 0.8 * alpha];
        let ping = match tag.ping_ms {
            Some(ms) => format!("ping {ms} ms"),
            None => "ping -".to_string(),
        };
        let line_h = ui.calc_text_size("Ag")[1];
        let name_w = ui.calc_text_size(name)[0];
        let ping_w = if mode == 0 { ui.calc_text_size(&ping)[0] } else { 0.0 };
        // Dark rounded plate behind the text keeps it readable on any background.
        let pad = 5.0;
        let half_w = name_w.max(ping_w) * 0.5 + pad;
        let below = if mode == 0 { line_h } else { 0.0 };
        dl.add_rect([sx - half_w, sy - line_h - pad], [sx + half_w, sy + below + pad], bg_col)
            .filled(true)
            .rounding(4.0)
            .build();
        // Name above the point, ping line just under it.
        dl.add_text([sx - name_w * 0.5, sy - line_h], name_col, name);
        if mode == 0 {
            dl.add_text([sx - ping_w * 0.5, sy], ping_col, &ping);
        }
        // imgui allows one foreground DrawListMut at a time and the bar takes its own
        // (live 2026-10-24 v8.2: panic "already loaded" the moment a companion appeared).
        drop(dl);
        Self::draw_head_bar(ui, status, tag, [sx - half_w, sy - line_h - pad, sx + half_w, sy + below + pad], h);
    }
}

impl ImguiRenderLoop for Overlay {
    // ImGui's built-in 13 px bitmap font looked pixelated on a friend's screen
    // (2026-10-22). Use Windows' Segoe UI, sized to the screen; Proton has no Segoe UI
    // but links its own arial.ttf into the prefix. Fall back to the built-in font if neither
    // file is there.
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        let Some((file, bytes)) =
            ["segoeui.ttf", "arial.ttf"].into_iter().find_map(|f| Some((f, std::fs::read(format!(r"{windir}\Fonts\{f}")).ok()?)))
        else {
            log::line("ui: segoeui.ttf / arial.ttf not found, using built-in font");
            return;
        };
        let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let height = ctx.io().display_size[1];
        let size = if height > 0.0 { (height / 1080.0 * 20.0).clamp(16.0, 40.0) } else { 20.0 };
        ctx.fonts().add_font(&[FontSource::TtfData {
            data,
            size_pixels: size,
            config: Some(FontConfig { oversample_h: 2, ..FontConfig::default() }),
        }]);
        log::line(&format!("ui: font {file} {size:.0} px"));
    }

    /// The mouse cursor is drawn by ImGui only while the 'Mod settings' panel is up (it is
    /// not known whether the game shows its own cursor in the System menu; if it does, this
    /// draws a second one).
    fn before_render<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        ctx.io_mut().mouse_draw_cursor =
            PANEL_VISIBLE.load(Ordering::Relaxed) || (crate::itemdrop::panel_visible() && !is_pad_mode());
    }

    /// Suppress all game window input while the modal Drop Item dialog is open (plus soak),
    /// and suppress game mouse events while the cursor hovers an open side drawer.
    fn message_filter(&self, io: &Io) -> MessageFilter {
        if crate::inputblock::blocked() {
            MessageFilter::InputAll
        } else if (PANEL_VISIBLE.load(Ordering::Relaxed) || crate::itemdrop::panel_visible()) && io.want_capture_mouse {
            MessageFilter::InputMouse
        } else {
            MessageFilter::empty()
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        let focused = crate::spectate_input::game_focused();
        let pad = if focused { crate::spectate_input::pad_state() } else { None };
        let pad_active = pad_has_activity(pad);
        let kbm_active = focused && kbm_has_activity(ui);
        INPUT_IS_PAD.store(
            detect_input_mode(INPUT_IS_PAD.load(Ordering::Relaxed), pad_active, kbm_active),
            Ordering::Relaxed,
        );

        // Debug hotkeys (settings: debug_hotkeys = 1).
        if tuning::debug_hotkeys() {
            if ui.is_key_pressed_no_repeat(Key::F8) {
                post(|c| c.wake = true);
            }
            if ui.is_key_pressed_no_repeat(Key::F9) {
                post(|c| c.go_to_friend = true);
            }
            if ui.is_key_pressed_no_repeat(Key::F11) {
                post(|c| c.mark = true);
            }
        }
        let status = SHARED
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.status.clone()))
            .unwrap_or_default();
        let drop_snap = crate::itemdrop::ui_snapshot();
        let [dw, dh] = ui.io().display_size;
        let show_hud = hud_visible();
        crate::depth::begin_overlay_frame(
            dw,
            dh,
            show_hud && drop_snap.connected && !drop_snap.markers.is_empty(),
        );

        if show_hud {
            self.draw_tags(ui, &status);
            draw_lock_mark(ui);
            self.draw_party(ui, &status);
            self.draw_item_drops(ui, &drop_snap);
        }
        self.draw_page_hint(ui);
        self.panel.draw(ui);
        self.drop_panel.draw(ui, &drop_snap, pad);

        if !settings::show_status_overlay() {
            return;
        }
        ui.window("##coop-status")
            .position([10.0, 10.0], Condition::Always)
            .no_decoration()
            .always_auto_resize(true)
            .bg_alpha(0.35)
            .no_inputs()
            .build(|| {
                ui.text(format!("CooP Sekiro v{MOD_VERSION} by xCløuD: {}", status.net));
                if let Some(n) = &status.notice {
                    ui.text_colored(YELLOW, n);
                }
            });
    }
}

/// Installs the DX11 overlay. Call once from a background thread.
pub fn install(hmodule: usize) {
    let result = Hudhook::builder()
        .with::<ImguiDx11Hooks>(Overlay::default())
        .with_hmodule(HINSTANCE(hmodule as *mut _))
        .build()
        .apply();
    match result {
        Ok(()) => {
            log::line("ui: overlay installed");
            crate::depth::install();
        }
        Err(e) => log::line(&format!("ui: overlay failed: {e:?}")),
    }
}

/// Friend off screen: name and distance at the screen edge in the friend's
/// direction (`v` = direction from the screen centre, screen axes), with a
/// small triangle pointing outwards.
/// Returns the plate rect `[x0, y0, x1, y1]` (the party HP bar hangs under it).
fn draw_edge_marker(ui: &Ui, tag: &FriendTag, v: [f32; 2], dist: f32, w: f32, h: f32, placed: &mut Vec<[f32; 4]>) -> Option<[f32; 4]> {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if !len.is_finite() || len < 1e-3 {
        return None;
    }
    let dir = [v[0] / len, v[1] / len];
    let margin = 70.0;
    let (cx, cy) = (w * 0.5, h * 0.5);
    let kx = if dir[0].abs() > 1e-4 { (cx - margin) / dir[0].abs() } else { f32::MAX };
    let ky = if dir[1].abs() > 1e-4 { (cy - margin) / dir[1].abs() } else { f32::MAX };
    let k = kx.min(ky);
    let (px, py) = (cx + dir[0] * k, cy + dir[1] * k);
    let text = format!("{}  {:.0} m", tag.name, dist);
    let tw = ui.calc_text_size(&text)[0];
    let lh = ui.calc_text_size("Ag")[1];
    let dl = ui.get_foreground_draw_list();
    let pad = 5.0;
    // On the left / right edge the plates stack vertically, on the top / bottom sideways.
    let half = [tw * 0.5 + pad, lh * 0.5 + pad];
    let [px, py] = edge_stack([px, py], half, kx < ky, placed);
    placed.push([px - half[0], py - half[1], px + half[0], py + half[1]]);
    dl.add_rect([px - tw * 0.5 - pad, py - lh * 0.5 - pad], [px + tw * 0.5 + pad, py + lh * 0.5 + pad], [0.0, 0.0, 0.0, 0.6])
        .filled(true)
        .rounding(4.0)
        .build();
    dl.add_text([px - tw * 0.5, py - lh * 0.5], [1.0, 0.92, 0.45, 1.0], &text);
    // Triangle just outside the plate, pointing towards the friend.
    let tip = [px + dir[0] * (margin - 12.0), py + dir[1] * (margin - 12.0)];
    let base = [px + dir[0] * (margin - 30.0), py + dir[1] * (margin - 30.0)];
    let side = [-dir[1] * 9.0, dir[0] * 9.0];
    dl.add_triangle(tip, [base[0] + side[0], base[1] + side[1]], [base[0] - side[0], base[1] - side[1]], [1.0, 0.92, 0.45, 0.9])
        .filled(true)
        .build();
    Some([px - tw * 0.5 - pad, py - lh * 0.5 - pad, px + tw * 0.5 + pad, py + lh * 0.5 + pad])
}

/// Head mode: the party row of the peer `id` (None: no bar for that nameplate).
fn member_of(party: &[PartyMember], id: PeerId) -> Option<&PartyMember> {
    party.iter().find(|m| m.id == id)
}

/// "(away)" / "(dead)" / "(in cutscene)" for a party member state, None when here.
fn state_tag(state: u8) -> Option<&'static str> {
    match state {
        1 => Some("(away)"),
        2 => Some("(dead)"),
        3 => Some("(in cutscene)"),
        _ => None,
    }
}

/// "hp / max" (hp never negative).
fn hp_numbers(m: &PartyMember) -> String {
    format!("{} / {}", m.hp.max(0), m.max_hp.max(0))
}

/// Filled part of the bar, 0..1. A dead member (or unknown max) is an empty bar.
fn hp_frac(m: &PartyMember) -> f32 {
    if m.max_hp > 0 && m.state != 2 { (m.hp as f32 / m.max_hp as f32).clamp(0.0, 1.0) } else { 0.0 }
}

/// Top-left of the left / top party list (`party_hp_position`), x 2 % of the width.
fn row_origin(pos: u8, w: f32, h: f32) -> (f32, f32) {
    let y = match pos {
        settings::PARTY_POS_TOP => 0.06,
        settings::PARTY_POS_LEFT => 0.30,
        _ => 0.30,
    };
    (w * 0.02, h * y)
}

/// Head-mode bar under the nameplate plate `[x0, y0, x1, y1]`: as wide as the plate (at
/// least 90 px), 5 px high, 2 px below it, centred; sizes scale with the screen height
/// (1080 = 1) and `scale_pct`.
fn head_bar_rect(plate: [f32; 4], screen_h: f32, scale_pct: u32) -> [f32; 4] {
    let k = screen_h / 1080.0 * scale_pct as f32 / 100.0;
    let width = (plate[2] - plate[0]).max(90.0 * k);
    let cx = (plate[0] + plate[2]) * 0.5;
    let top = plate[3] + 2.0 * k;
    [cx - width * 0.5, top, cx + width * 0.5, top + (5.0 * k).max(2.0)]
}

/// One HP bar in `rect`: dark back, red fill (faded when the member is in another area).
fn draw_bar(dl: &hudhook::imgui::DrawListMut<'_>, rect: [f32; 4], m: &PartyMember) {
    let [x0, y0, x1, y1] = rect;
    dl.add_rect([x0, y0], [x1, y1], [0.0, 0.0, 0.0, 0.6]).filled(true).build();
    let frac = hp_frac(m);
    if frac > 0.0 {
        let a = if m.state == 1 { 0.35 } else { 0.9 };
        dl.add_rect([x0, y0], [x0 + (x1 - x0) * frac, y1], [0.72, 0.10, 0.10, a]).filled(true).build();
    }
    dl.add_rect([x0, y0], [x1, y1], [0.05, 0.05, 0.05, 0.9]).build();
}

#[cfg(test)]
mod party_bar_tests {
    use super::*;

    fn member(hp: i32, max_hp: i32, state: u8) -> PartyMember {
        PartyMember { id: PeerId(1), name: "x".into(), hp, max_hp, state }
    }

    #[test]
    fn head_bar_is_centred_under_the_plate() {
        // 1080p, 100 %: plate 200 wide -> bar 200 wide, 5 high, 2 below.
        let r = head_bar_rect([400.0, 300.0, 600.0, 340.0], 1080.0, 100);
        assert_eq!(r, [400.0, 342.0, 600.0, 347.0]);
    }

    #[test]
    fn head_bar_has_a_minimum_width_and_scales() {
        // Narrow plate (60 px): min 90 px, centred on x = 130.
        let r = head_bar_rect([100.0, 0.0, 160.0, 20.0], 1080.0, 100);
        assert_eq!((r[0], r[2]), (85.0, 175.0));
        // 540p halves the minimum width (45) and height; 200 % doubles them back.
        let half = head_bar_rect([100.0, 0.0, 160.0, 20.0], 540.0, 100);
        assert_eq!(half[2] - half[0], 60.0);
        let narrow = head_bar_rect([100.0, 0.0, 130.0, 20.0], 540.0, 100);
        assert_eq!(narrow[2] - narrow[0], 45.0);
        let big = head_bar_rect([100.0, 0.0, 160.0, 20.0], 540.0, 200);
        assert_eq!((big[2] - big[0], big[3] - big[1]), (90.0, 5.0));
        // Never thinner than 2 px.
        let tiny = head_bar_rect([0.0, 0.0, 10.0, 10.0], 540.0, 50);
        assert_eq!(tiny[3] - tiny[1], 2.0);
    }

    #[test]
    fn row_origin_left_and_top() {
        assert_eq!(row_origin(settings::PARTY_POS_LEFT, 1000.0, 1000.0), (20.0, 300.0));
        let (x, y) = row_origin(settings::PARTY_POS_TOP, 1000.0, 1000.0);
        assert_eq!(x, 20.0);
        assert!((y - 60.0).abs() < 1e-3);
    }

    #[test]
    fn hp_fraction_and_numbers() {
        assert_eq!(hp_frac(&member(50, 200, 0)), 0.25);
        assert_eq!(hp_frac(&member(500, 200, 0)), 1.0);
        assert_eq!(hp_frac(&member(-5, 200, 0)), 0.0);
        assert_eq!(hp_frac(&member(50, 0, 0)), 0.0);
        assert_eq!(hp_frac(&member(50, 200, 2)), 0.0, "dead = empty bar");
        assert_eq!(hp_numbers(&member(-3, 200, 0)), "0 / 200");
        assert_eq!(state_tag(1), Some("(away)"));
        assert_eq!(state_tag(0), None);
        assert_eq!(state_tag(3), Some("(in cutscene)"));
    }

    #[test]
    fn head_mode_finds_each_peers_own_row() {
        let mut a = member(10, 100, 0);
        let mut b = member(70, 100, 0);
        a.id = PeerId(1);
        b.id = PeerId(3);
        let party = [a, b];
        assert_eq!(member_of(&party, PeerId(3)).map(|m| m.hp), Some(70));
        assert_eq!(member_of(&party, PeerId(1)).map(|m| m.hp), Some(10));
        // A nameplate without a row (no snapshot yet) gets no bar, never another peer's.
        assert!(member_of(&party, PeerId(2)).is_none());
    }
}

/// Where an off-screen marker plate (centre `p`, half size `half`) goes so it overlaps
/// none of the plates already `placed` this frame (`[x0, y0, x1, y1]`): moved along the
/// screen edge (`vertical`: up / down) one plate at a time, alternating sides. The first
/// marker, or one with room, stays where it is.
fn edge_stack(p: [f32; 2], half: [f32; 2], vertical: bool, placed: &[[f32; 4]]) -> [f32; 2] {
    let gap = 4.0;
    let step = if vertical { 2.0 * half[1] + gap } else { 2.0 * half[0] + gap };
    let overlaps = |c: [f32; 2]| {
        placed.iter().any(|r| c[0] - half[0] < r[2] && c[0] + half[0] > r[0] && c[1] - half[1] < r[3] && c[1] + half[1] > r[1])
    };
    for k in 0..(2 * crate::peers::MAX_PLAYERS as i32) {
        // 0, +1, -1, +2, -2, ...
        let n = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as f32;
        let c = if vertical { [p[0], p[1] + n * step] } else { [p[0] + n * step, p[1]] };
        if !overlaps(c) {
            return c;
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_hud_hides_behind_menus_cutscenes_warps_and_loads() {
        assert!(should_show_world_hud(true, false, Some(0), false));
        assert!(!should_show_world_hud(false, false, Some(0), false));
        assert!(!should_show_world_hud(true, true, Some(0), false));
        assert!(!should_show_world_hud(true, false, Some(0x40), false));
        assert!(!should_show_world_hud(true, false, Some(0x08), false));
        assert!(!should_show_world_hud(true, false, None, false));
        assert!(!should_show_world_hud(true, false, Some(0), true));
    }

    fn names(v: &[(u8, &str)]) -> Vec<(PeerId, String)> {
        v.iter().map(|(s, n)| (PeerId(*s), n.to_string())).collect()
    }

    #[test]
    fn presence_diff_names_who_joined_and_left() {
        let a = names(&[(0, "Host")]);
        let b = names(&[(0, "Host"), (2, "Bea")]);
        assert_eq!(presence_diff(&[], &a), vec![Presence::Joined(PeerId(0), "Host".into())]);
        assert_eq!(presence_diff(&a, &b), vec![Presence::Joined(PeerId(2), "Bea".into())]);
        assert_eq!(presence_diff(&b, &a), vec![Presence::Left(PeerId(2), "Bea".into())]);
        assert!(presence_diff(&b, &b).is_empty());
        // The host link drops: everyone leaves at once, in id order.
        assert_eq!(
            presence_diff(&b, &[]),
            vec![Presence::Left(PeerId(0), "Host".into()), Presence::Left(PeerId(2), "Bea".into())]
        );
        // Same slot, other name (v7.5: only "NEW joined").
        assert_eq!(presence_diff(&a, &names(&[(0, "Other")])), vec![Presence::Joined(PeerId(0), "Other".into())]);
        // A slot reused by someone else in the same frame another left.
        assert_eq!(
            presence_diff(&names(&[(1, "Ann")]), &names(&[(2, "Bea")])),
            vec![Presence::Left(PeerId(1), "Ann".into()), Presence::Joined(PeerId(2), "Bea".into())]
        );
    }

    #[test]
    fn edge_markers_stack_instead_of_overlapping() {
        let half = [50.0, 12.0];
        let mut placed = Vec::new();
        let first = edge_stack([70.0, 300.0], half, true, &placed);
        assert_eq!(first, [70.0, 300.0]);
        placed.push([first[0] - half[0], first[1] - half[1], first[0] + half[0], first[1] + half[1]]);
        let second = edge_stack([70.0, 300.0], half, true, &placed);
        assert_eq!(second, [70.0, 328.0]);
        placed.push([second[0] - half[0], second[1] - half[1], second[0] + half[0], second[1] + half[1]]);
        let third = edge_stack([70.0, 300.0], half, true, &placed);
        assert_eq!(third, [70.0, 272.0]);
        // Far apart: unchanged.
        assert_eq!(edge_stack([70.0, 600.0], half, true, &placed), [70.0, 600.0]);
        // Top edge: sideways.
        assert_eq!(edge_stack([first[0] + 10.0, 300.0], half, false, &placed[..1]), [first[0] + 10.0 + 104.0, 300.0]);
    }

    #[test]
    fn detect_input_mode_switches_cleanly_between_pad_and_kbm() {
        // Starts KBM -> stays KBM when idle -> switches on clear pad input -> back on clear KBM.
        assert!(!detect_input_mode(false, false, false));
        assert!(detect_input_mode(false, true, false));
        assert!(detect_input_mode(true, false, false));
        assert!(!detect_input_mode(true, false, true));
        // Simultaneous activity keeps previous mode stable.
        assert!(detect_input_mode(true, true, true));
        assert!(!detect_input_mode(false, true, true));
    }

    #[test]
    fn dialog_pad_nav_handles_dpad_and_both_sticks_with_repeat() {
        use crate::spectate_input::PadState;
        let mut nav = DialogPadNav::default();
        assert_eq!(nav.step(None, 0), 0);
        // D-pad right edge -> +1, then delay until 350 ms
        assert_eq!(nav.step(Some(PadState { buttons: PAD_DPAD_RIGHT, ..Default::default() }), 0), 1);
        assert_eq!(nav.step(Some(PadState { buttons: PAD_DPAD_RIGHT, ..Default::default() }), 349), 0);
        assert_eq!(nav.step(Some(PadState { buttons: PAD_DPAD_RIGHT, ..Default::default() }), 350), 1);
        // Neutral resets
        assert_eq!(nav.step(Some(PadState::default()), 360), 0);
        // Left stick up -> +5
        assert_eq!(nav.step(Some(PadState { ly: 22_000, ..Default::default() }), 370), 5);
        // Release, then Right stick down -> -5
        assert_eq!(nav.step(Some(PadState::default()), 380), 0);
        assert_eq!(nav.step(Some(PadState { ry: -22_000, ..Default::default() }), 390), -5);
    }
}
