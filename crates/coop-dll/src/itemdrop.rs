//! Player-to-player item dropping and ground pickup (`BULK_ITEM_DROP = 65`).
//!
//! Vanilla Sekiro has no inventory "Leave" action or player-loot ground bag. This module lets
//! connected players safely drop **consumables** and **prosthetic upgrade materials** from their
//! own inventory (`EquipInventoryData.normal_items_head` / `key_items_head`, filtered strictly to
//! droppable items) and lets any player in the same map area pick them up natively via
//! `MapItemMan_GrantItems` (`keyitems::grant_entries`).
//!
//! Safety guarantees:
//! - Key items, prosthetic tools, skills, Prayer Beads, Gourd Seeds, Gourds, Memories, Remnants,
//!   Spirit Emblems, Resurrect nodes, Rot Essences, Homeward Idols, and co-op items can NEVER be dropped.
//! - Before spawning any world drop, the dropper verifies inventory removal by comparing exact
//!   inventory quantities before and after (`cur_qty - drop_qty`), preventing duplication.
//! - Partial stack drops decrement the entry's `quantity` directly (keeping >= 1 intact in hash
//!   tables & quick slots); full-stack drops collapse the entry to 1 and call Sekiro 1.06's
//!   `0x67B230 LuaEventMan_RemoveItem(..., 1)` once (since it removes 1 copy per call regardless of count)
//!   so quick slots unequip cleanly and inventory hash buckets unlink cleanly.
//! - Pickups are resolved atomically by the host (`DropTable::claim`), with `claimed_recent`
//!   deduplication so two players pressing interact at the same moment never duplicate an item.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use fromsoftware_shared::FromStatic;
use sekiro::param::{EQUIP_PARAM_GOODS_ST, ParamDef};
use sekiro::sprj::{InventoryItemsData, PlayerGameData};

use crate::game::{self, Chr, module_base, read, read_bytes, read_ptr, write};
use crate::net::{Dest, Net};
use crate::peers::{PeerId, PeerSet};
use crate::{items, keyitems, log};

/// Bulk transfer kind reserved for player-dropped items (unused by all other modules).
pub const BULK_ITEM_DROP: u8 = 65;

const WIRE_VERSION: u8 = 1;
const MSG_DROP: u8 = 1;
const MSG_CLAIM_REQ: u8 = 2;
const MSG_CLAIM_OK: u8 = 3;
const MSG_SYNC_DROPS: u8 = 4;
const MSG_REMOVE_DROP: u8 = 5;

pub const MAX_DROPS: usize = 32;
pub const PICKUP_RANGE_M: f32 = 2.5;
pub const MARKER_RANGE_M: f32 = 60.0;
const MAX_CLAIMED_HISTORY: usize = 96;
const CLAIM_FLIGHT_TIMEOUT: Duration = Duration::from_millis(1500);
const INV_REFRESH_EVERY: Duration = Duration::from_millis(250);
const FEEDBACK_FOR: Duration = Duration::from_secs(4);

const CAT_GOODS: u32 = 4;
const GOODS_SHIFTED: u32 = 0x4000_0000;
/// `LuaEventMan_RemoveItem(0, 0x4000_0000, param_id, stack_count)` [SPEC-ITEM-CLEANUP §3].
const REMOVE_ITEM_RVA: usize = 0x67B230;
const GAME_DATA_MAN: usize = 0x3D5AAC0;
const GDM_PLAYER_GAME_DATA: usize = 0x08;
const PGD_EQUIP_GAME_DATA: usize = 0x278;
const INV_ITEMS: usize = std::mem::offset_of!(sekiro::sprj::EquipGameData, equip_inventory_data)
    + std::mem::offset_of!(sekiro::sprj::EquipInventoryData, items_data);
const INV_NORMAL_CAP: usize = std::mem::offset_of!(InventoryItemsData, normal_items_capacity);
const INV_NORMAL_HEAD: usize = std::mem::offset_of!(InventoryItemsData, normal_items_head);
const INV_KEY_CAP: usize = std::mem::offset_of!(InventoryItemsData, key_items_capacity);
const INV_KEY_HEAD: usize = std::mem::offset_of!(InventoryItemsData, key_items_head);
const INV_ENTRY_LEN: usize = 16;
const MAX_SLOTS: usize = 8192;

/// Prosthetic upgrade material param IDs in Sekiro (always droppable if owned in inventory).
pub const UPGRADE_MATERIAL_IDS: [u32; 9] = [
    6000, // Scrap Iron
    6010, // Scrap Magnetite
    6020, // Adamantite Scrap
    6096, // Black Gunpowder
    6192, // Yellow Gunpowder
    6210, // Fulminated Mercury
    6300, // Lump of Fat Wax
    6310, // Lump of Grave Wax
    6400, // Lapis Lazuli
];

/// Permanent non-droppable goods param IDs & ranges even if marked consumable or multi-cap.
const NON_DROPPABLE_GOODS_IDS: [u32; 7] = [
    1000, // Spirit Emblem
    1100, // Resurrective Power / Dragon Blood
    1110, // Temporary Regenerative Power / Resurrect node
    2000, // Resurrection internal node
    4000, // Prayer Bead
    4100, // Gourd Seed (legacy alias)
    4400, // Gourd Seed
];

/// Keep compilation honest on PlayerGameData layout usage.
const _: usize = std::mem::offset_of!(PlayerGameData, equip_game_data) - PGD_EQUIP_GAME_DATA;

// --- Pure item filter -----------------------------------------------------------------------

/// Minimal `EQUIP_PARAM_GOODS_ST` fields used to decide whether an item may be dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoodsDropMeta {
    pub param_id: u32,
    pub is_consume: bool,
    pub is_only_one: bool,
    pub is_supple_item: bool,
    pub is_full_supple_item: bool,
    pub is_travel_item: bool,
    pub is_bonfire_warp: bool,
    pub is_enhance: bool,
    pub supple_type: u8,
    pub max_num: i16,
    pub goods_type: u8,
    pub is_coop: bool,
}

/// Strictly allows only stackable consumables and prosthetic upgrade materials from a player's
/// inventory. Rejects key items, gourds, Homeward Idols, Spirit Emblems, Resurrect nodes,
/// Prayer Beads, Gourd Seeds, Memories, Remnants, Rot Essences, unique tools, and co-op items.
/// Note: `auto_replenish_type` is intentionally NOT checked because Sekiro's normal consumables
/// (Ungo's Sugar, Pellet, Ceramic Shard, Fistful of Ash, etc.) auto-restock from Storage Box.
pub fn is_droppable_goods(m: GoodsDropMeta) -> bool {
    if m.is_coop {
        return false;
    }
    if NON_DROPPABLE_GOODS_IDS.contains(&m.param_id)
        || (3000..=3009).contains(&m.param_id)
        || (4200..=4399).contains(&m.param_id)
        || keyitems::ROT_ESSENCES.contains(&m.param_id)
    {
        return false;
    }
    if m.is_only_one
        || m.is_supple_item
        || m.is_full_supple_item
        || m.supple_type != 0
        || m.is_travel_item
        || m.is_bonfire_warp
        || m.max_num <= 1
    {
        return false;
    }
    if UPGRADE_MATERIAL_IDS.contains(&m.param_id) {
        return true;
    }
    // goods_type: 1 = Key Item, 2 = Hidden/System
    if matches!(m.goods_type, 1 | 2) {
        return false;
    }
    if m.is_enhance || m.goods_type == 3 {
        return true;
    }
    m.is_consume && m.goods_type == 0
}

fn meta_from_row(param_id: u32, row: &EQUIP_PARAM_GOODS_ST) -> GoodsDropMeta {
    GoodsDropMeta {
        param_id,
        is_consume: row.is_consume(),
        is_only_one: row.is_only_one(),
        is_supple_item: row.is_supple_item(),
        is_full_supple_item: row.is_full_supple_item(),
        is_travel_item: row.is_travel_item(),
        is_bonfire_warp: row.is_bonfire_warp_item(),
        is_enhance: row.is_enhance(),
        supple_type: row.supple_type(),
        max_num: row.max_num(),
        goods_type: row.goods_type(),
        is_coop: items::is_coop_goods(param_id),
    }
}

fn goods_meta(param_id: u32) -> Option<GoodsDropMeta> {
    let repo = unsafe { sekiro::sprj::SoloParamRepository::instance() }.ok()?;
    let file = repo.params().find(|p| p.struct_name() == EQUIP_PARAM_GOODS_ST::NAME).map(|p| &*p.data)?;
    let row = unsafe { file.get_row_by_id::<EQUIP_PARAM_GOODS_ST>(param_id) }?;
    Some(meta_from_row(param_id, row))
}

static DROPPABLE_CATALOG: Mutex<Option<BTreeMap<u32, String>>> = Mutex::new(None);

fn with_droppable_catalog<R>(f: impl FnOnce(&BTreeMap<u32, String>) -> R) -> Option<R> {
    let mut guard = DROPPABLE_CATALOG.lock().ok()?;
    if guard.is_none() {
        let repo = unsafe { sekiro::sprj::SoloParamRepository::instance() }.ok()?;
        let file = repo
            .params()
            .find(|p| p.struct_name() == EQUIP_PARAM_GOODS_ST::NAME)
            .map(|p| &*p.data)?;
        let mut map = BTreeMap::new();
        for (id, row) in unsafe { file.rows::<EQUIP_PARAM_GOODS_ST>() } {
            let meta = meta_from_row(id, row);
            if is_droppable_goods(meta)
                && let Some(name) = items::goods_name(id)
            {
                map.insert(id, name);
            }
        }
        if map.is_empty() {
            return None;
        }
        *guard = Some(map);
    }
    guard.as_ref().map(f)
}

fn droppable_name(full_id: u32) -> Option<String> {
    if (full_id >> 28) != CAT_GOODS {
        return None;
    }
    let param_id = full_id & 0x0FFF_FFFF;
    if items::is_coop_goods(param_id) {
        return None;
    }
    with_droppable_catalog(|cat| cat.get(&param_id).cloned()).flatten()
}

// --- Pure DropTable + Wire protocol --------------------------------------------------------

/// A single item bundle currently lying in a shared world.
#[derive(Clone, Debug, PartialEq)]
pub struct WorldDrop {
    pub drop_id: u64,
    pub block: i32,
    pub pos: [f32; 3],
    pub full_id: u32,
    pub qty: u32,
    pub dropper_slot: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WireMsg {
    Drop(WorldDrop),
    ClaimReq { drop_id: u64 },
    ClaimOk { drop_id: u64, winner_slot: u8 },
    SyncDrops(Vec<WorldDrop>),
    RemoveDrop { drop_id: u64 },
}

fn write_drop(out: &mut Vec<u8>, d: &WorldDrop) {
    out.extend_from_slice(&d.drop_id.to_le_bytes());
    out.extend_from_slice(&d.block.to_le_bytes());
    for v in d.pos {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&d.full_id.to_le_bytes());
    out.extend_from_slice(&d.qty.to_le_bytes());
    out.push(d.dropper_slot);
}

fn read_drop(b: &[u8], at: &mut usize) -> Option<WorldDrop> {
    let take = |n: usize, at: &mut usize| -> Option<&[u8]> {
        let end = at.checked_add(n)?;
        let s = b.get(*at..end)?;
        *at = end;
        Some(s)
    };
    let u64v = u64::from_le_bytes(take(8, at)?.try_into().ok()?);
    let block = i32::from_le_bytes(take(4, at)?.try_into().ok()?);
    let mut pos = [0.0f32; 3];
    for c in &mut pos {
        *c = f32::from_le_bytes(take(4, at)?.try_into().ok()?);
    }
    let full_id = u32::from_le_bytes(take(4, at)?.try_into().ok()?);
    let qty = u32::from_le_bytes(take(4, at)?.try_into().ok()?);
    let dropper_slot = take(1, at)?[0];
    let sane_coords = pos.iter().all(|v| v.is_finite() && v.abs() < 100_000.0);
    if !sane_coords || (full_id >> 28) != CAT_GOODS || qty == 0 || qty > 9999 {
        return None;
    }
    Some(WorldDrop { drop_id: u64v, block, pos, full_id, qty, dropper_slot })
}

pub fn encode_msg(m: &WireMsg) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.push(WIRE_VERSION);
    match m {
        WireMsg::Drop(d) => {
            out.push(MSG_DROP);
            write_drop(&mut out, d);
        }
        WireMsg::ClaimReq { drop_id } => {
            out.push(MSG_CLAIM_REQ);
            out.extend_from_slice(&drop_id.to_le_bytes());
        }
        WireMsg::ClaimOk { drop_id, winner_slot } => {
            out.push(MSG_CLAIM_OK);
            out.extend_from_slice(&drop_id.to_le_bytes());
            out.push(*winner_slot);
        }
        WireMsg::SyncDrops(list) => {
            out.push(MSG_SYNC_DROPS);
            let n = list.len().min(MAX_DROPS) as u16;
            out.extend_from_slice(&n.to_le_bytes());
            for d in list.iter().take(MAX_DROPS) {
                write_drop(&mut out, d);
            }
        }
        WireMsg::RemoveDrop { drop_id } => {
            out.push(MSG_REMOVE_DROP);
            out.extend_from_slice(&drop_id.to_le_bytes());
        }
    }
    out
}

pub fn decode_msg(b: &[u8]) -> Option<WireMsg> {
    if b.len() < 2 || b[0] != WIRE_VERSION {
        return None;
    }
    let kind = b[1];
    let mut at = 2usize;
    let take = |n: usize, at: &mut usize| -> Option<&[u8]> {
        let end = at.checked_add(n)?;
        let s = b.get(*at..end)?;
        *at = end;
        Some(s)
    };
    let msg = match kind {
        MSG_DROP => WireMsg::Drop(read_drop(b, &mut at)?),
        MSG_CLAIM_REQ => WireMsg::ClaimReq {
            drop_id: u64::from_le_bytes(take(8, &mut at)?.try_into().ok()?),
        },
        MSG_CLAIM_OK => WireMsg::ClaimOk {
            drop_id: u64::from_le_bytes(take(8, &mut at)?.try_into().ok()?),
            winner_slot: take(1, &mut at)?[0],
        },
        MSG_SYNC_DROPS => {
            let n = u16::from_le_bytes(take(2, &mut at)?.try_into().ok()?) as usize;
            if n > MAX_DROPS {
                return None;
            }
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                list.push(read_drop(b, &mut at)?);
            }
            WireMsg::SyncDrops(list)
        }
        MSG_REMOVE_DROP => WireMsg::RemoveDrop {
            drop_id: u64::from_le_bytes(take(8, &mut at)?.try_into().ok()?),
        },
        _ => return None,
    };
    (at == b.len()).then_some(msg)
}

/// Pure drop table maintaining live ground drops and preventing duplicate claims.
#[derive(Default, Debug)]
pub struct DropTable {
    pub drops: Vec<WorldDrop>,
    claimed_recent: VecDeque<u64>,
}

impl DropTable {
    pub fn can_accept_drop(&self) -> bool {
        self.drops.len() < MAX_DROPS
    }

    pub fn insert(&mut self, d: WorldDrop) -> bool {
        if self.claimed_recent.contains(&d.drop_id) || self.drops.iter().any(|x| x.drop_id == d.drop_id) {
            return false;
        }
        if self.drops.len() >= MAX_DROPS {
            self.drops.remove(0);
        }
        self.drops.push(d);
        true
    }

    pub fn replace_all(&mut self, incoming: Vec<WorldDrop>) {
        self.drops.clear();
        for d in incoming.into_iter().take(MAX_DROPS) {
            if !self.claimed_recent.contains(&d.drop_id) && !self.drops.iter().any(|x| x.drop_id == d.drop_id) {
                self.drops.push(d);
            }
        }
    }

    /// Atomically removes and returns the drop if it exists (preventing duplicate claims).
    pub fn claim(&mut self, drop_id: u64) -> Option<WorldDrop> {
        self.record_claimed(drop_id);
        let idx = self.drops.iter().position(|d| d.drop_id == drop_id)?;
        Some(self.drops.remove(idx))
    }

    pub fn remove_only(&mut self, drop_id: u64) -> Option<WorldDrop> {
        let idx = self.drops.iter().position(|d| d.drop_id == drop_id)?;
        Some(self.drops.remove(idx))
    }

    pub fn record_claimed(&mut self, drop_id: u64) {
        if !self.claimed_recent.contains(&drop_id) {
            if self.claimed_recent.len() >= MAX_CLAIMED_HISTORY {
                self.claimed_recent.pop_front();
            }
            self.claimed_recent.push_back(drop_id);
        }
    }

    /// Nearest drop in `block` within `PICKUP_RANGE_M` of `pos`.
    pub fn nearest_in_range(&self, block: i32, pos: [f32; 3]) -> Option<(&WorldDrop, f32)> {
        self.drops
            .iter()
            .filter(|d| d.block == block)
            .filter_map(|d| {
                let dist = game::distance(pos, d.pos);
                (dist <= PICKUP_RANGE_M).then_some((d, dist))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    pub fn clear(&mut self) {
        self.drops.clear();
        self.claimed_recent.clear();
    }
}

// --- Shared UI snapshot & requests (render thread <-> game thread) -------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DroppableItem {
    pub full_id: u32,
    pub name: String,
    pub qty: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DropMarker {
    pub drop_id: u64,
    pub pos: [f32; 3],
    pub label: String,
    pub dist_m: f32,
}

#[derive(Clone, Default)]
pub struct UiSnapshot {
    pub connected: bool,
    pub in_inventory_menu: bool,
    pub hovered_droppable: Option<DroppableItem>,
    pub items: Vec<DroppableItem>,
    pub markers: Vec<DropMarker>,
    pub nearby_drop: Option<(String, u32)>,
    pub feedback: Option<String>,
}

static UI_SNAPSHOT: Mutex<Option<UiSnapshot>> = Mutex::new(None);
static DROP_REQUEST: Mutex<Option<(u32, u32)>> = Mutex::new(None);
static DROP_PANEL_VISIBLE: AtomicBool = AtomicBool::new(false);
static HOVERED_GOODS_ID: AtomicU32 = AtomicU32::new(0);

/// Called from `items::caption_detour` when Sekiro fetches the detail caption of a goods item.
pub fn note_hovered_goods(param_id: u32) {
    HOVERED_GOODS_ID.store(param_id, Ordering::Relaxed);
}

fn clear_hovered_goods() {
    HOVERED_GOODS_ID.store(0, Ordering::Relaxed);
}

pub fn resolve_hovered_droppable(
    in_inventory_menu: bool,
    hovered_param_id: u32,
    items: &[DroppableItem],
) -> Option<DroppableItem> {
    if !in_inventory_menu || hovered_param_id == 0 {
        return None;
    }
    items
        .iter()
        .find(|it| (it.full_id & 0x0FFF_FFFF) == hovered_param_id)
        .cloned()
}

/// Keeps multiple drops at the same feet position slightly separated so each box & glow is distinct.
pub fn spread_drop_pos(existing: &[WorldDrop], block: i32, base: [f32; 3], seq: u32) -> [f32; 3] {
    const MIN_SEP_M: f32 = 0.35;
    const RING_R_M: f32 = 0.42;
    let clear = |p: [f32; 3]| {
        !existing
            .iter()
            .filter(|d| d.block == block)
            .any(|d| game::distance(p, d.pos) < MIN_SEP_M)
    };
    if clear(base) {
        return base;
    }
    let start = (seq as usize) % 8;
    for step in 0..8 {
        let idx = (start + step) % 8;
        let ang = (idx as f32) * std::f32::consts::TAU / 8.0;
        let cand = [base[0] + RING_R_M * ang.cos(), base[1], base[2] + RING_R_M * ang.sin()];
        if clear(cand) {
            return cand;
        }
    }
    let ang = (start as f32) * std::f32::consts::TAU / 8.0;
    [base[0] + RING_R_M * ang.cos(), base[1], base[2] + RING_R_M * ang.sin()]
}

pub fn ui_snapshot() -> UiSnapshot {
    UI_SNAPSHOT
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

pub fn post_drop_request(full_id: u32, qty: u32) {
    if qty == 0 {
        return;
    }
    if let Ok(mut g) = DROP_REQUEST.lock() {
        *g = Some((full_id, qty));
    }
}

fn take_drop_request() -> Option<(u32, u32)> {
    DROP_REQUEST.lock().ok().and_then(|mut g| g.take())
}

pub fn set_panel_visible(v: bool) {
    DROP_PANEL_VISIBLE.store(v, Ordering::Relaxed);
}

pub fn panel_visible() -> bool {
    DROP_PANEL_VISIBLE.load(Ordering::Relaxed)
}

fn publish_ui(snap: UiSnapshot) {
    if let Ok(mut g) = UI_SNAPSHOT.lock() {
        *g = Some(snap);
    }
}

// --- Verified inventory reads & safe removal -----------------------------------------------

/// Returns `(entries, normal_goods_count, key_goods_count)` across both `normal_items_head` and
/// `key_items_head`. Eligibility for dropping is strictly enforced by `is_droppable_goods`.
fn scan_goods_inventory_entries() -> Option<(Vec<(usize, u32, u32)>, usize, usize)> {
    let pgd = read_ptr(module_base() + GAME_DATA_MAN).and_then(|g| read_ptr(g + GDM_PLAYER_GAME_DATA))?;
    let inv = pgd + PGD_EQUIP_GAME_DATA + INV_ITEMS;
    let mut out = Vec::new();
    let mut normal_n = 0usize;
    let mut key_n = 0usize;
    for (is_normal, head_off, cap_off) in [
        (true, INV_NORMAL_HEAD, INV_NORMAL_CAP),
        (false, INV_KEY_HEAD, INV_KEY_CAP),
    ] {
        let n = read::<u32>(inv + cap_off)? as usize;
        if n == 0 {
            continue;
        }
        if n > MAX_SLOTS {
            return None;
        }
        let head = read_ptr(inv + head_off)?;
        let mut buf = vec![0u8; n * INV_ENTRY_LEN];
        if !read_bytes(head, &mut buf) {
            return None;
        }
        for (i, e) in buf.chunks_exact(INV_ENTRY_LEN).enumerate() {
            let id = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            let qty = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);
            if (id >> 28) == CAT_GOODS && qty > 0 && qty < 1_000_000 {
                out.push((head + i * INV_ENTRY_LEN, id, qty));
                if is_normal {
                    normal_n += 1;
                } else {
                    key_n += 1;
                }
            }
        }
    }
    Some((out, normal_n, key_n))
}

fn total_goods_qty(full_id: u32) -> Option<u32> {
    let (entries, _, _) = scan_goods_inventory_entries()?;
    Some(entries.into_iter().filter(|&(_, id, _)| id == full_id).map(|(_, _, q)| q).sum())
}

fn refresh_droppable_inventory(logged_diag: &mut bool) -> Vec<DroppableItem> {
    let Some((entries, normal_n, key_n)) = scan_goods_inventory_entries() else {
        return Vec::new();
    };
    let mut by_id: BTreeMap<u32, u32> = BTreeMap::new();
    for (_, id, qty) in entries {
        *by_id.entry(id).or_insert(0) += qty;
    }
    let Some((mut list, cat_len)) = with_droppable_catalog(|cat| {
        let items: Vec<DroppableItem> = by_id
            .iter()
            .filter_map(|(&full_id, &qty)| {
                let param_id = full_id & 0x0FFF_FFFF;
                if items::is_coop_goods(param_id) {
                    return None;
                }
                let name = cat.get(&param_id)?.clone();
                Some(DroppableItem { full_id, name, qty })
            })
            .collect();
        (items, cat.len())
    }) else {
        return Vec::new();
    };
    list.sort_by(|a, b| a.name.cmp(&b.name));
    if !*logged_diag && (normal_n + key_n > 0) {
        *logged_diag = true;
        log::line(&format!(
            "itemdrop: catalog {cat_len} droppable goods; inventory normal={normal_n} key={key_n} droppable={}",
            list.len()
        ));
        if list.is_empty() {
            let mut samples = Vec::new();
            for (&full_id, &qty) in by_id.iter().take(8) {
                let pid = full_id & 0x0FFF_FFFF;
                if let Some(m) = goods_meta(pid) {
                    samples.push(format!(
                        "{pid}(q={qty},t={},c={},sup={}/{},max={})",
                        m.goods_type, m.is_consume as u8, m.is_supple_item as u8, m.supple_type, m.max_num
                    ));
                }
            }
            log::line(&format!("itemdrop: sample goods flags: [{}]", samples.join(", ")));
        }
    }
    list
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalStep {
    /// Partial stack removal: write `new_qty` (`>= 1`) directly into the inventory entry.
    PartialWrite(u32),
    /// Full stack removal: collapse to `1` first if `entry_qty > 1`, then call `LuaEventMan_RemoveItem(..., 1)`
    /// once to cleanly unequip any quick slot and unlink the inventory hash entry.
    FullStack { write_one_first: bool },
}

pub fn plan_removal_step(entry_qty: u32, need: u32) -> RemovalStep {
    if need < entry_qty {
        RemovalStep::PartialWrite(entry_qty - need)
    } else {
        RemovalStep::FullStack {
            write_one_first: entry_qty > 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Exact(u32),
    Partial { removed: u32, remaining: u32 },
    NoneRemoved,
    OverRemoved(u32),
}

/// Pure reconciliation between expected drop count and measured before/after inventory totals.
pub fn reconcile_removed_qty(before: u32, after: u32, requested: u32) -> RemoveOutcome {
    let removed = before.saturating_sub(after);
    if removed == 0 {
        RemoveOutcome::NoneRemoved
    } else if removed == requested {
        RemoveOutcome::Exact(removed)
    } else if removed < requested {
        RemoveOutcome::Partial {
            removed,
            remaining: requested - removed,
        }
    } else {
        RemoveOutcome::OverRemoved(removed)
    }
}

/// Safely removes `drop_qty` copies of `full_id` from inventory and returns
/// `(name, actual_removed, remaining_total)`.
/// Partial stack writes (`>= 1` left) update quantity in-place; full-stack removals collapse an
/// entry down to `1` and then call `0x67B230 LuaEventMan_RemoveItem(..., 1)` once so quick-slots
/// unequip and hash buckets unlink cleanly regardless of stack size (`3`, `4`, `99`, etc.).
fn remove_goods(full_id: u32, drop_qty: u32) -> Result<(String, u32, u32), &'static str> {
    if (full_id >> 28) != CAT_GOODS || drop_qty == 0 {
        return Err("Invalid item.");
    }
    let name = droppable_name(full_id).ok_or("This item cannot be dropped.")?;
    let (entries, _, _) = scan_goods_inventory_entries().ok_or("Inventory unreadable.")?;
    let before_total: u32 = entries.iter().filter(|&&(_, id, _)| id == full_id).map(|&(_, _, q)| q).sum();
    if before_total < drop_qty {
        return Err("Not enough items in inventory.");
    }
    let target_total = before_total - drop_qty;
    let param_id = full_id & 0x0FFF_FFFF;
    let remove_fn: extern "C" fn(usize, u32, u32, i32) -> u8 =
        unsafe { std::mem::transmute(module_base() + REMOVE_ITEM_RVA) };

    for _ in 0..16 {
        let Some((cur_entries, _, _)) = scan_goods_inventory_entries() else {
            return Err("Inventory unreadable.");
        };
        let mut cur_total = 0u32;
        let mut first_match: Option<(usize, u32)> = None;
        for (addr, id, q) in cur_entries {
            if id == full_id {
                cur_total = cur_total.saturating_add(q);
                if first_match.is_none() {
                    first_match = Some((addr, q));
                }
            }
        }
        if cur_total <= target_total {
            break;
        }
        let need = cur_total - target_total;
        let Some((entry_addr, entry_qty)) = first_match else {
            break;
        };
        match plan_removal_step(entry_qty, need) {
            RemovalStep::PartialWrite(new_qty) => {
                if !write::<u32>(entry_addr + 8, new_qty) {
                    return Err("Failed to update inventory quantity.");
                }
            }
            RemovalStep::FullStack { write_one_first } => {
                if write_one_first && !write::<u32>(entry_addr + 8, 1) {
                    return Err("Failed to update inventory quantity.");
                }
                let _ = remove_fn(0, GOODS_SHIFTED, param_id, 1);
            }
        }
        let next_total = total_goods_qty(full_id).ok_or("Inventory verification failed.")?;
        if next_total >= cur_total {
            break;
        }
    }

    let after_total = total_goods_qty(full_id).ok_or("Inventory verification failed.")?;
    match reconcile_removed_qty(before_total, after_total, drop_qty) {
        RemoveOutcome::Exact(n) => Ok((name, n, after_total)),
        RemoveOutcome::Partial { removed, .. } => {
            log::line(&format!(
                "itemdrop: partial remove {full_id:#x} (requested {drop_qty}, removed {removed}): spawning x{removed}"
            ));
            Ok((name, removed, after_total))
        }
        RemoveOutcome::OverRemoved(actual) => {
            log::line(&format!(
                "itemdrop: over-removed {full_id:#x} (requested {drop_qty}, removed {actual}): spawning x{actual}"
            ));
            Ok((name, actual, after_total))
        }
        RemoveOutcome::NoneRemoved => {
            log::line(&format!(
                "itemdrop: ERROR removing {full_id:#x} x{drop_qty}: before {before_total}, after {after_total}"
            ));
            Err("Inventory check mismatch; drop cancelled.")
        }
    }
}

// --- Interact key edge detection -----------------------------------------------------------

#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(vk: i32) -> i16;
}

const VK_E: i32 = 0x45;
const PAD_X: u16 = 0x4000;

fn key_down(vk: i32) -> bool {
    crate::inputblock::with_mod_poll(|| (unsafe { GetAsyncKeyState(vk) } as u16) & 0x8000 != 0)
}

// --- Runtime controller --------------------------------------------------------------------

pub struct ItemDrop {
    table: DropTable,
    names: BTreeMap<u64, String>,
    seq: u32,
    session_nonce: u16,
    was_connected: bool,
    synced_peers: BTreeMap<PeerId, u64>,
    claim_in_flight: Option<(u64, Instant)>,
    pending_grants: VecDeque<(u32, u32, String)>,
    inv_at: Option<Instant>,
    cached_items: Vec<DroppableItem>,
    feedback: Option<(String, Instant)>,
    e_was_down: bool,
    pad_x_was_down: bool,
    logged_diag: bool,
    last_hovered: u32,
}

impl Default for ItemDrop {
    fn default() -> Self {
        Self {
            table: DropTable::default(),
            names: BTreeMap::new(),
            seq: 0,
            session_nonce: 0,
            was_connected: false,
            synced_peers: BTreeMap::new(),
            claim_in_flight: None,
            pending_grants: VecDeque::new(),
            inv_at: None,
            cached_items: Vec::new(),
            feedback: None,
            e_was_down: false,
            pad_x_was_down: false,
            logged_diag: false,
            last_hovered: 0,
        }
    }
}

impl ItemDrop {
    pub fn on_loading(&mut self) {
        self.inv_at = None;
        self.cached_items.clear();
        self.e_was_down = false;
        self.pad_x_was_down = false;
        self.last_hovered = 0;
        clear_hovered_goods();
        publish_ui(UiSnapshot::default());
    }

    pub fn on_world_reset(&mut self) {
        self.on_loading();
    }

    pub fn on_disconnected(&mut self) {
        if self.was_connected {
            self.was_connected = false;
            self.table.clear();
            self.names.clear();
            self.synced_peers.clear();
            self.claim_in_flight = None;
            self.pending_grants.clear();
            self.cached_items.clear();
            self.feedback = None;
            self.e_was_down = false;
            self.pad_x_was_down = false;
            self.logged_diag = false;
            self.last_hovered = 0;
        }
        clear_hovered_goods();
        publish_ui(UiSnapshot::default());
    }

    fn set_feedback(&mut self, msg: String, now: Instant) {
        self.feedback = Some((msg, now));
    }

    fn name_for(&mut self, d: &WorldDrop) -> String {
        if let Some(n) = self.names.get(&d.drop_id) {
            return n.clone();
        }
        let n = items::goods_name(d.full_id & 0x0FFF_FFFF).unwrap_or_else(|| format!("Item #{}", d.full_id & 0x0FFF_FFFF));
        self.names.insert(d.drop_id, n.clone());
        n
    }

    fn broadcast_except(&self, net: &mut Net, peers: &PeerSet, skip: Option<PeerId>, data: &[u8]) {
        for p in peers.iter() {
            if Some(p.id) != skip {
                let _ = net.send_bulk_to(Dest::Peer(p.id), BULK_ITEM_DROP, data);
            }
        }
    }

    fn handle_inbound(&mut self, net: &mut Net, peers: &PeerSet) {
        while let Some((from, raw)) = net.take_bulk_from(BULK_ITEM_DROP) {
            let Some(msg) = decode_msg(&raw) else { continue };
            match msg {
                WireMsg::Drop(mut d) => {
                    if net.is_host() {
                        if droppable_name(d.full_id).is_none() {
                            log::line(&format!(
                                "itemdrop: ignored non-droppable item {:#x} from peer {}",
                                d.full_id, from.slot()
                            ));
                            continue;
                        }
                        d.dropper_slot = from.slot();
                    } else if !from.is_host() {
                        continue;
                    }
                    if self.table.insert(d.clone()) {
                        let name = self.name_for(&d);
                        log::line(&format!(
                            "itemdrop: peer {} dropped {name} x{} (id {:#x}, block {})",
                            d.dropper_slot, d.qty, d.drop_id, d.block
                        ));
                        if net.is_host() {
                            let bytes = encode_msg(&WireMsg::Drop(d));
                            self.broadcast_except(net, peers, Some(from), &bytes);
                        }
                    }
                }
                WireMsg::ClaimReq { drop_id } => {
                    if !net.is_host() {
                        continue;
                    }
                    if let Some(d) = self.table.claim(drop_id) {
                        self.names.remove(&drop_id);
                        let bytes = encode_msg(&WireMsg::ClaimOk { drop_id, winner_slot: from.slot() });
                        let _ = net.send_bulk(BULK_ITEM_DROP, &bytes);
                        log::line(&format!(
                            "itemdrop: peer {} claimed {:#x} (x{})",
                            from.slot(), d.full_id, d.qty
                        ));
                    } else {
                        let bytes = encode_msg(&WireMsg::RemoveDrop { drop_id });
                        let _ = net.send_bulk_to(Dest::Peer(from), BULK_ITEM_DROP, &bytes);
                    }
                }
                WireMsg::ClaimOk { drop_id, winner_slot } => {
                    if !from.is_host() {
                        continue;
                    }
                    self.table.record_claimed(drop_id);
                    if self.claim_in_flight.is_some_and(|(id, _)| id == drop_id) {
                        self.claim_in_flight = None;
                    }
                    let removed = self.table.remove_only(drop_id);
                    let cached_name = self.names.remove(&drop_id);
                    if winner_slot == peers.me().slot()
                        && let Some(d) = removed
                    {
                        let name = cached_name
                            .or_else(|| items::goods_name(d.full_id & 0x0FFF_FFFF))
                            .unwrap_or_else(|| "Item".into());
                        self.pending_grants.push_back((d.full_id, d.qty, name));
                    }
                }
                WireMsg::SyncDrops(list) => {
                    if !from.is_host() {
                        continue;
                    }
                    self.table.replace_all(list);
                    self.names.clear();
                }
                WireMsg::RemoveDrop { drop_id } => {
                    if !from.is_host() {
                        continue;
                    }
                    self.table.record_claimed(drop_id);
                    if self.claim_in_flight.is_some_and(|(id, _)| id == drop_id) {
                        self.claim_in_flight = None;
                    }
                    self.table.remove_only(drop_id);
                    self.names.remove(&drop_id);
                }
            }
        }
    }

    fn flush_grants(&mut self, in_world_for: Duration, now: Instant) {
        if self.pending_grants.is_empty() || game::load_pending() || game::safe_for_reward(in_world_for).is_err() {
            return;
        }
        while let Some((full_id, qty, name)) = self.pending_grants.pop_front() {
            if keyitems::grant_entries(&[[full_id, qty, u32::MAX, 0]]) {
                log::line(&format!("itemdrop: picked up {name} x{qty}"));
                self.set_feedback(format!("Picked up {name} x{qty}"), now);
                self.inv_at = None;
            } else {
                self.pending_grants.push_front((full_id, qty, name));
                break;
            }
        }
    }

    pub fn tick(
        &mut self,
        net: &mut Net,
        peers: &PeerSet,
        player: Chr,
        here: Option<i32>,
        in_world_for: Duration,
        system_menu_open: bool,
    ) {
        let now = Instant::now();
        let host_authority = net.is_host() || net.is_hosting_open();
        let has_link = net.connected();
        let my_slot = if host_authority { PeerId::HOST.slot() } else { peers.me().slot() };
        if !self.was_connected {
            self.was_connected = true;
            self.session_nonce = self.session_nonce.wrapping_add(1);
            self.seq = 0;
        }

        // Process inbound messages before sync/drop/pickup.
        if has_link {
            self.handle_inbound(net, peers);
        }

        // Host: sync active drops to newly connected / rejoined peers (clear tracking when
        // no peer is linked so the next joiner always receives current floor drops).
        if host_authority && has_link {
            let links = peers.link_epochs();
            self.synced_peers.retain(|id, _| links.iter().any(|(l, _)| l == id));
            for (id, epoch) in links {
                if self.synced_peers.get(&id) != Some(&epoch) && !self.table.drops.is_empty() {
                    let bytes = encode_msg(&WireMsg::SyncDrops(self.table.drops.clone()));
                    if net.send_bulk_to(Dest::Peer(id), BULK_ITEM_DROP, &bytes) {
                        self.synced_peers.insert(id, epoch);
                    }
                } else if self.table.drops.is_empty() {
                    self.synced_peers.insert(id, epoch);
                }
            }
        } else if !has_link {
            self.synced_peers.clear();
        }

        // Complete any pending grants cleanly in-world.
        self.flush_grants(in_world_for, now);

        // Process queued UI drop requests safely on the game thread.
        if let Some((full_id, req_qty)) = take_drop_request() {
            if in_world_for < Duration::from_secs(1) || game::load_pending() || player.hp().is_none_or(|h| h <= 0) {
                self.set_feedback("Cannot drop items right now.".into(), now);
            } else if !self.table.can_accept_drop() {
                self.set_feedback(format!("Ground drop limit reached ({MAX_DROPS})."), now);
            } else if let (Some(block), Some(pos)) = (here, player.position().filter(|p| game::sane_pos(*p))) {
                match remove_goods(full_id, req_qty) {
                    Ok((name, actual_qty, remaining_total)) => {
                        self.seq = self.seq.wrapping_add(1);
                        let drop_id = ((my_slot as u64) << 56)
                            | ((self.session_nonce as u64) << 40)
                            | (self.seq as u64);
                        let drop_pos = spread_drop_pos(&self.table.drops, block, pos, self.seq);
                        let drop = WorldDrop {
                            drop_id,
                            block,
                            pos: drop_pos,
                            full_id,
                            qty: actual_qty,
                            dropper_slot: my_slot,
                        };
                        self.names.insert(drop_id, name.clone());
                        self.table.insert(drop.clone());
                        if has_link {
                            let bytes = encode_msg(&WireMsg::Drop(drop));
                            if host_authority {
                                let _ = net.send_bulk(BULK_ITEM_DROP, &bytes);
                            } else {
                                let _ = net.send_bulk_to(Dest::Host, BULK_ITEM_DROP, &bytes);
                            }
                        }
                        self.inv_at = None;
                        log::line(&format!(
                            "itemdrop: dropped {name} x{actual_qty} (remaining {remaining_total}, id {drop_id:#x}, block {block})"
                        ));
                        self.set_feedback(format!("Dropped {name} x{actual_qty} (remaining: {remaining_total})"), now);
                    }
                    Err(e) => self.set_feedback(e.to_owned(), now),
                }
            } else {
                self.set_feedback("World position unavailable.".into(), now);
            }
        }

        // Expire old feedback & timed-out in-flight claim.
        if self.feedback.as_ref().is_some_and(|(_, t)| now.duration_since(*t) >= FEEDBACK_FOR) {
            self.feedback = None;
        }
        if self.claim_in_flight.is_some_and(|(_, t)| now.duration_since(t) >= CLAIM_FLIGHT_TIMEOUT) {
            self.claim_in_flight = None;
        }

        let menu_mask = game::menu_mask();
        let in_inventory_menu = is_inventory_menu_open(menu_mask);
        if !in_inventory_menu {
            clear_hovered_goods();
            self.last_hovered = 0;
        }
        let hovered_id = if in_inventory_menu { HOVERED_GOODS_ID.load(Ordering::Relaxed) } else { 0 };
        if in_inventory_menu && hovered_id != self.last_hovered {
            self.last_hovered = hovered_id;
            self.inv_at = None;
        }

        // Refresh droppable inventory cache periodically (~4 Hz) or immediately when panel/hover opens.
        let refresh_due = self.inv_at.is_none_or(|t| now.duration_since(t) >= INV_REFRESH_EVERY);
        if (panel_visible() || system_menu_open || in_inventory_menu || self.inv_at.is_none()) && refresh_due {
            self.inv_at = Some(now);
            self.cached_items = refresh_droppable_inventory(&mut self.logged_diag);
        }
        let hovered_droppable = resolve_hovered_droppable(in_inventory_menu, hovered_id, &self.cached_items);

        // Build nearby ground markers & interact prompt.
        let mut markers = Vec::new();
        let mut nearby_drop = None;
        let can_pickup_now = in_world_for >= Duration::from_secs(1)
            && !game::load_pending()
            && player.hp().is_some_and(|h| h > 0)
            && menu_mask == Some(0)
            && game::talk_idle()
            && !crate::cutscene::active()
            && !crate::ui::panel_visible()
            && !panel_visible()
            && !crate::inputblock::blocked();

        let e_down = crate::spectate_input::game_focused() && key_down(VK_E);
        let pad_x_down = crate::spectate_input::game_focused()
            && crate::spectate_input::pad_buttons_down().is_some_and(|b| b & PAD_X != 0);
        let pickup_pressed = (e_down && !self.e_was_down) || (pad_x_down && !self.pad_x_was_down);
        self.e_was_down = e_down;
        self.pad_x_was_down = pad_x_down;

        let show_world_markers = menu_mask == Some(0) && !crate::cutscene::active();
        if let (Some(block), Some(pp)) = (here, player.position().filter(|p| game::sane_pos(*p))) {
            if show_world_markers {
                let active_list: Vec<WorldDrop> = self.table.drops.iter().filter(|d| d.block == block).cloned().collect();
                for d in active_list {
                    let dist = game::distance(pp, d.pos);
                    if dist <= MARKER_RANGE_M {
                        let name = self.name_for(&d);
                        markers.push(DropMarker {
                            drop_id: d.drop_id,
                            pos: d.pos,
                            label: format!("{name} x{}", d.qty),
                            dist_m: dist,
                        });
                    }
                }
            }
            if can_pickup_now
                && self.claim_in_flight.is_none()
                && let Some((nearest, _)) = self.table.nearest_in_range(block, pp)
            {
                let drop_id = nearest.drop_id;
                let qty = nearest.qty;
                let full_id = nearest.full_id;
                let nearest_clone = nearest.clone();
                let name = self.name_for(&nearest_clone);
                nearby_drop = Some((name.clone(), qty));
                if pickup_pressed {
                    if host_authority {
                        if self.table.claim(drop_id).is_some() {
                            self.names.remove(&drop_id);
                            if has_link {
                                let bytes = encode_msg(&WireMsg::ClaimOk { drop_id, winner_slot: 0 });
                                let _ = net.send_bulk(BULK_ITEM_DROP, &bytes);
                            }
                            self.pending_grants.push_back((full_id, qty, name));
                            self.flush_grants(in_world_for, now);
                        }
                    } else {
                        self.claim_in_flight = Some((drop_id, now));
                        let bytes = encode_msg(&WireMsg::ClaimReq { drop_id });
                        let _ = net.send_bulk_to(Dest::Host, BULK_ITEM_DROP, &bytes);
                    }
                }
            }
        }

        publish_ui(UiSnapshot {
            connected: true,
            in_inventory_menu,
            hovered_droppable,
            items: self.cached_items.clone(),
            markers,
            nearby_drop,
            feedback: self.feedback.as_ref().map(|(s, _)| s.clone()),
        });
    }
}

/// Returns `true` when Sekiro's Equipment/Inventory menu is open (`0x40` set and `0x08` clear).
/// Intentionally ignores `quitblock::system_menu_open` so Inventory item dropping stays available
/// throughout co-op sessions (including after a partner connects).
pub(crate) fn is_inventory_menu_open(menu_mask: Option<u32>) -> bool {
    menu_mask.is_some_and(|m| (m & 0x40) != 0 && (m & 0x08) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_menu_open_ignores_system_list_latch() {
        assert!(!is_inventory_menu_open(None));
        assert!(!is_inventory_menu_open(Some(0)));
        assert!(is_inventory_menu_open(Some(0x40)));
        assert!(!is_inventory_menu_open(Some(0x48)));
        assert!(!is_inventory_menu_open(Some(0x08)));
    }

    fn base_meta(param_id: u32) -> GoodsDropMeta {
        GoodsDropMeta {
            param_id,
            is_consume: true,
            is_only_one: false,
            is_supple_item: false,
            is_full_supple_item: false,
            is_travel_item: false,
            is_bonfire_warp: false,
            is_enhance: false,
            supple_type: 0,
            max_num: 10,
            goods_type: 0,
            is_coop: false,
        }
    }

    #[test]
    fn droppable_filter_allows_only_consumables_and_upgrade_materials() {
        // Consumables (Pellet, Ungo's Sugar, Ceramic Shard, Fistful of Ash, Confetti — auto_replenish is ignored)
        assert!(is_droppable_goods(GoodsDropMeta { max_num: 3, ..base_meta(3020) }));
        assert!(is_droppable_goods(GoodsDropMeta { max_num: 3, ..base_meta(3210) }));
        // Upgrade materials (Scrap Iron, Fulminated Mercury, Lapis Lazuli)
        for id in UPGRADE_MATERIAL_IDS {
            assert!(
                is_droppable_goods(GoodsDropMeta { is_consume: false, is_enhance: true, max_num: 99, ..base_meta(id) }),
                "upgrade material {id} must be droppable"
            );
        }
        // Key items, Healing Gourds (3000/3001) & colored gourds, Homeward Idol, Spirit Emblem, Prayer Bead,
        // Gourd Seed (4100/4400), Memory, Resurrection (goods_type 7), Rot Essence, Co-op item, unique max_num=1
        assert!(!is_droppable_goods(GoodsDropMeta { goods_type: 1, is_consume: false, max_num: 1, ..base_meta(2470) }));
        assert!(!is_droppable_goods(base_meta(3000)));
        assert!(!is_droppable_goods(base_meta(3001)));
        assert!(!is_droppable_goods(GoodsDropMeta { is_supple_item: true, ..base_meta(201) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_full_supple_item: true, ..base_meta(202) }));
        assert!(!is_droppable_goods(GoodsDropMeta { supple_type: 1, ..base_meta(203) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_bonfire_warp: true, is_only_one: true, max_num: 1, ..base_meta(120) }));
        assert!(!is_droppable_goods(GoodsDropMeta { max_num: 999, ..base_meta(1000) }));
        assert!(!is_droppable_goods(GoodsDropMeta { goods_type: 7, max_num: 3, ..base_meta(2000) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_consume: false, max_num: 40, ..base_meta(4000) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_consume: false, max_num: 9, ..base_meta(4100) }));
        assert!(!is_droppable_goods(GoodsDropMeta { goods_type: 1, is_consume: false, max_num: 99, ..base_meta(4400) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_consume: false, max_num: 99, ..base_meta(4201) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_consume: false, max_num: 99, ..base_meta(9505) }));
        assert!(!is_droppable_goods(GoodsDropMeta { is_coop: true, is_consume: false, ..base_meta(9001) }));
        assert!(!is_droppable_goods(GoodsDropMeta { max_num: 1, ..base_meta(3100) }), "unique max_num=1 excluded");
    }

    #[test]
    fn resolve_hovered_droppable_checks_menu_and_owned_list() {
        let items = vec![
            DroppableItem { full_id: GOODS_SHIFTED | 3210, name: "Ungo's Sugar".into(), qty: 2 },
            DroppableItem { full_id: GOODS_SHIFTED | 3410, name: "Ceramic Shard".into(), qty: 8 },
        ];
        assert_eq!(resolve_hovered_droppable(false, 3210, &items), None);
        assert_eq!(resolve_hovered_droppable(true, 0, &items), None);
        assert_eq!(resolve_hovered_droppable(true, 3000, &items), None);
        assert_eq!(
            resolve_hovered_droppable(true, 3210, &items).as_ref().map(|i| (i.name.as_str(), i.qty)),
            Some(("Ungo's Sugar", 2))
        );
    }

    #[test]
    fn spread_drop_pos_offsets_overlapping_drops() {
        let base = [10.0, 2.0, -5.0];
        assert_eq!(spread_drop_pos(&[], 1100, base, 1), base);
        let existing = vec![WorldDrop {
            drop_id: 1,
            block: 1100,
            pos: base,
            full_id: GOODS_SHIFTED | 3210,
            qty: 1,
            dropper_slot: 0,
        }];
        let next = spread_drop_pos(&existing, 1100, base, 1);
        assert!(game::distance(base, next) >= 0.35);
        assert_eq!(next[1], base[1]);
        // Different block does not push the drop aside.
        assert_eq!(spread_drop_pos(&existing, 1200, base, 1), base);
    }

    #[test]
    fn wire_messages_round_trip_and_reject_garbage() {
        let d = WorldDrop { drop_id: 0x0100_0002_0000_0003, block: 1100, pos: [1.5, 2.0, -3.5], full_id: GOODS_SHIFTED | 3020, qty: 4, dropper_slot: 1 };
        let cases = vec![
            WireMsg::Drop(d.clone()),
            WireMsg::ClaimReq { drop_id: d.drop_id },
            WireMsg::ClaimOk { drop_id: d.drop_id, winner_slot: 2 },
            WireMsg::SyncDrops(vec![d.clone()]),
            WireMsg::RemoveDrop { drop_id: d.drop_id },
        ];
        for m in cases {
            let b = encode_msg(&m);
            assert_eq!(decode_msg(&b), Some(m));
            assert_eq!(decode_msg(&b[..b.len() - 1]), None);
        }
        assert_eq!(decode_msg(&[99, 1]), None);
        let bad_qty = WorldDrop { qty: 0, ..d.clone() };
        assert_eq!(decode_msg(&encode_msg(&WireMsg::Drop(bad_qty))), None);
        let bad_cat = WorldDrop { full_id: 0x1000_0001, ..d };
        assert_eq!(decode_msg(&encode_msg(&WireMsg::Drop(bad_cat))), None);
    }

    #[test]
    fn drop_table_prevents_double_pickup_and_enforces_range() {
        let mut t = DropTable::default();
        let d1 = WorldDrop { drop_id: 42, block: 1100, pos: [10.0, 0.0, 10.0], full_id: GOODS_SHIFTED | 3020, qty: 3, dropper_slot: 0 };
        assert!(t.insert(d1.clone()));
        assert!(!t.insert(d1.clone()), "duplicate ignored");
        assert!(t.nearest_in_range(1100, [11.5, 0.0, 10.0]).is_some());
        assert!(t.nearest_in_range(1100, [14.0, 0.0, 10.0]).is_none(), "out of 2.5m range");
        assert!(t.nearest_in_range(1200, [10.0, 0.0, 10.0]).is_none(), "different block");

        // First claim wins atomically; second claimer gets None (zero duplication).
        assert_eq!(t.claim(42).map(|d| d.qty), Some(3));
        assert!(t.claim(42).is_none());
        // Late re-insert of already-claimed drop_id is blocked.
        assert!(!t.insert(d1));
    }

    #[test]
    fn open_host_self_pickup_and_late_joiner_sync_preserve_only_unclaimed_drops() {
        let mut t = DropTable::default();
        let d_keep = WorldDrop {
            drop_id: ((PeerId::HOST.slot() as u64) << 56) | 1,
            block: 1100,
            pos: [1.0, 2.0, 3.0],
            full_id: GOODS_SHIFTED | 3210,
            qty: 2,
            dropper_slot: PeerId::HOST.slot(),
        };
        let d_self = WorldDrop {
            drop_id: ((PeerId::HOST.slot() as u64) << 56) | 2,
            block: 1100,
            pos: [1.5, 2.0, 3.0],
            full_id: GOODS_SHIFTED | 3020,
            qty: 1,
            dropper_slot: PeerId::HOST.slot(),
        };
        assert!(t.insert(d_keep.clone()));
        assert!(t.insert(d_self.clone()));

        // Solo host picks up their second drop cleanly before any peer connects.
        assert_eq!(t.claim(d_self.drop_id).map(|d| d.qty), Some(1));
        assert_eq!(t.drops.len(), 1);
        assert_eq!(t.drops[0], d_keep);

        // Late joiner receives SyncDrops and populates the remaining drop cleanly.
        let sync_bytes = encode_msg(&WireMsg::SyncDrops(t.drops.clone()));
        let Some(WireMsg::SyncDrops(synced)) = decode_msg(&sync_bytes) else {
            panic!("expected SyncDrops");
        };
        let mut client = DropTable::default();
        client.replace_all(synced);
        assert_eq!(client.drops, vec![d_keep]);
    }

    #[test]
    fn reconcile_removed_qty_handles_exact_partial_none_and_over() {
        assert_eq!(reconcile_removed_qty(3, 0, 3), RemoveOutcome::Exact(3));
        assert_eq!(reconcile_removed_qty(3, 2, 3), RemoveOutcome::Partial { removed: 1, remaining: 2 });
        assert_eq!(reconcile_removed_qty(3, 3, 3), RemoveOutcome::NoneRemoved);
        assert_eq!(reconcile_removed_qty(3, 0, 2), RemoveOutcome::OverRemoved(3));
    }

    #[test]
    fn plan_removal_step_collapses_full_stacks_before_engine_remove() {
        assert_eq!(plan_removal_step(3, 2), RemovalStep::PartialWrite(1));
        assert_eq!(plan_removal_step(3, 3), RemovalStep::FullStack { write_one_first: true });
        assert_eq!(plan_removal_step(4, 4), RemovalStep::FullStack { write_one_first: true });
        assert_eq!(plan_removal_step(99, 99), RemovalStep::FullStack { write_one_first: true });
        assert_eq!(plan_removal_step(1, 1), RemovalStep::FullStack { write_one_first: false });
    }
}
