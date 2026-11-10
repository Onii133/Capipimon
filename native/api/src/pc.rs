//! The Nest PC (GDD 2.3 "switching is free at any Nest's PC terminal (once per 10 minutes, not in
//! combat)", 3.8 "the replaced move can be relearned at any Nest PC for free").
//!
//! Opened with `/pc` or F at a PC terminal, standing inside a Nest (or within 2.5 b of its
//! terminal). The client draws a Pokémon-PC style box grid of the player's unlocked species;
//! arrows/WASD move, Enter/F/Space switches, R restores the kit's moves, Esc/Backspace closes.
//!
//! Wire (channel `capipimon:play`):
//! - server → client: `pc\t<cooldown secs>\t<current species or ->\t<species,species,...>`
//!   (open or refresh), `pc\t-` (close), `pcnote\t<text>` (a line under the grid).
//! - client → server: `pc\topen`, `pc\tpick\t<species>`, `pc\trelearn`, `pc\tclose`,
//!   `pc\tmoves`, `pc\tteach\t<move id>\t<slot 1-3>`; server → client `pcslots\t<q>\t<e>\t<r>`.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/pc.rs");

// ---------------------------------------------------------------- client

struct View {
    cool_until: Instant,
    current: String,
    list: Vec<String>,
    sel: usize,
    /// The move list is showing instead of the box.
    moves: Option<MoveList>,
}

struct MoveList {
    options: Vec<Move>,
    sel: usize,
}

#[derive(Default)]
pub struct PcClient {
    view: Option<View>,
    note: Option<(String, Instant)>,
    /// The Q/E/R moves now.
    slots: [String; 3],
}

pub static PC_CLIENT: LazyLock<Mutex<PcClient>> = LazyLock::new(Default::default);

const COLS: usize = 6;
const ROWS: usize = 5;

/// A `pc`/`pcnote` message; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    let mut c = PC_CLIENT.lock().unwrap();
    match f {
        ["pc", "-", ..] => c.view = None,
        ["pc", cool, current, list, ..] => {
            let secs = cool.parse::<u64>().unwrap_or(0);
            let list: Vec<String> = list.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
            let sel = c.view.as_ref().map_or(0, |v| v.sel).min(list.len().saturating_sub(1));
            c.view = Some(View { cool_until: Instant::now() + std::time::Duration::from_secs(secs), current: current.to_string(), list, sel, moves: None });
            c.note = None;
        }
        ["pcslots", q, e, r, ..] => c.slots = [q.to_string(), e.to_string(), r.to_string()],
        ["pcnote", text, ..] => c.note = Some((text.to_string(), Instant::now())),
        _ => return false,
    }
    true
}

/// Bought Move Tutor moves (`tutor`'s `tutored` line): added to an open Moves list.
pub fn add_moves(moves: &[Move]) {
    let mut c = PC_CLIENT.lock().unwrap();
    if let Some(m) = c.view.as_mut().and_then(|v| v.moves.as_mut()) {
        for mv in moves {
            if !m.options.iter().any(|o| o.id == mv.id) {
                m.options.push(mv.clone());
            }
        }
    }
}

pub fn forget() {
    *PC_CLIENT.lock().unwrap() = PcClient::default();
}

pub fn is_open() -> bool {
    PC_CLIENT.lock().unwrap().view.is_some()
}

/// Keys while the PC is open (all of them are taken, so moves don't fire).
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut c = PC_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_mut() else { return false };
    let n = v.list.len().max(1);
    let (col, row) = (v.sel % COLS, v.sel / COLS);
    let mut send: Option<String> = None;
    if let Some(m) = v.moves.as_mut() {
        let len = m.options.len().max(1);
        match key {
            "key.keyboard.up" | "key.keyboard.w" => m.sel = (m.sel + len - 1) % len,
            "key.keyboard.down" | "key.keyboard.s" => m.sel = (m.sel + 1) % len,
            "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" => {
                if let Some(o) = m.options.get(m.sel) {
                    send = Some(format!("pc\tteach\t{}\t{}", o.id, &key["key.keyboard.".len()..]));
                }
            }
            "key.keyboard.m" | "key.keyboard.escape" | "key.keyboard.backspace" => v.moves = None,
            _ => {}
        }
        drop(c);
        if let Some(s) = send {
            let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
        }
        return true;
    }
    match key {
        "key.keyboard.m" => {
            v.moves = Some(MoveList { options: progress::teachable(&v.current, progress::client_level()), sel: 0 });
            send = Some("pc\tmoves".to_string());
        }
        "key.keyboard.left" | "key.keyboard.a" => v.sel = row * COLS + (col + COLS - 1) % COLS,
        "key.keyboard.right" | "key.keyboard.d" => v.sel = row * COLS + (col + 1) % COLS,
        "key.keyboard.up" | "key.keyboard.w" => v.sel = ((row + ROWS - 1) % ROWS) * COLS + col,
        "key.keyboard.down" | "key.keyboard.s" => v.sel = ((row + 1) % ROWS) * COLS + col,
        "key.keyboard.enter" | "key.keyboard.f" | "key.keyboard.space" | "key.keyboard.keypad.enter" => {
            if let Some(s) = v.list.get(v.sel) {
                send = Some(format!("pc\tpick\t{s}"));
            }
        }
        "key.keyboard.r" => send = Some("pc\trelearn".to_string()),
        "key.keyboard.escape" | "key.keyboard.backspace" | "key.keyboard.p" | "key.keyboard.e" => {
            c.view = None;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, b"pc\tclose");
            return true;
        }
        _ => {}
    }
    if v.sel >= n {
        v.sel = n - 1;
    }
    drop(c);
    if let Some(s) = send {
        let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
    }
    true
}

/// Standing still while the PC is open (WASD move the cursor).
pub fn movement_input(event: &mut MovementInputEvent) {
    if is_open() {
        event.forward = 0.0;
        event.strafe = 0.0;
    }
}

// Colours: the classic storage-system look (blue frame, light wallpaper, white slots).
const PC_FRAME: u32 = 0xF02A_4F8C;
const PC_HEAD: u32 = 0xFF3D_7BD9;
const PC_WALL: u32 = 0xF0A8_D8F0;
const PC_WALL_DOT: u32 = 0xF094_C8E8;
const PC_SLOT: u32 = 0xE6F4_F8FC;
const PC_SLOT_EDGE: u32 = 0xFF7F_A8D0;
const PC_SEL: u32 = 0xFFFF_D23F;

/// The move list: everything this Pokémon can have on Q/E/R, nine rows at a time.
fn moves_hud(host: &mut dyn Host, b: &mut HudBatch, m: &MoveList, slots: &[String; 3], note: Option<&(String, Instant)>) {
    const SHOWN: usize = 9;
    let (w, h) = (290.0f32, 158.0f32);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, PC_FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, PC_HEAD);
    host.text(b, "Moves", [x + 8.0, y + 6.0], 0xFFFFFF, true);
    let bar = format!("Q {}   E {}   R {}", slots[0], slots[1], slots[2]);
    host.text(b, &fit(host, &bar, w - 14.0), [x + 7.0, y + 21.0], 0xFFE9A0, true);
    let start = m.sel.saturating_sub(SHOWN / 2).min(m.options.len().saturating_sub(SHOWN));
    for (i, o) in m.options.iter().enumerate().skip(start).take(SHOWN) {
        let ry = y + 33.0 + (i - start) as f32 * 11.0;
        let selected = i == m.sel;
        rect(host, b, x + 5.0, ry - 1.0, w - 10.0, 10.0, if selected { PC_SEL } else { PC_SLOT });
        rect(host, b, x + 8.0, ry + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&o.kind));
        host.text(b, &fit(host, &o.name, 130.0), [x + 15.0, ry], 0x202830, false);
        let stats = if o.support.is_some() { format!("support  {} PP", o.energy as u32) } else { format!("{} pow  {} PP", o.power as u32, o.energy as u32) };
        let sw = host.text_width(&stats);
        host.text(b, &stats, [x + w - 9.0 - sw, ry], 0x404A58, false);
    }
    guide::keyed_centred(host, b, "[W] [S] choose  put on: [1] Q  [2] E  [3] R  [M] back", x + w / 2.0, y + h - 22.0, 0xD8E4F4);
    if let Some((t, at)) = note {
        if at.elapsed().as_secs_f32() < 4.0 {
            let tw = host.text_width(t);
            host.text(b, t, [x + (w - tw) / 2.0, y + h - 11.0], 0xFFE9A0, true);
        }
    }
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let c = PC_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_ref() else { return };
    if let Some(m) = &v.moves {
        return moves_hud(host, b, m, &c.slots, c.note.as_ref());
    }
    let (cw, ch) = (34.0f32, 28.0f32);
    let (gw, gh) = (COLS as f32 * cw, ROWS as f32 * ch);
    let side = 118.0f32;
    let (w, h) = (gw + side + 18.0, gh + 44.0);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, PC_FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, PC_HEAD);
    ball(host, b, x + 11.0, y + 10.0, 4.5);
    host.text(b, "Capipimon PC  \u{2022}  BOX 1", [x + 19.0, y + 6.0], 0xFFFFFF, true);
    let cool = v.cool_until.saturating_duration_since(Instant::now()).as_secs();
    let status = if cool > 0 { format!("Switch in {}:{:02}", cool / 60, cool % 60) } else { "Switch ready".to_string() };
    let sw = host.text_width(&status);
    host.text(b, &status, [x + w - 6.0 - sw, y + 6.0], if cool > 0 { 0xFFD0A0 } else { 0xB8F5C0 }, true);
    // The box: wallpaper with a dot pattern, then 6 x 5 slots.
    let (gx, gy) = (x + 6.0, y + 21.0);
    rect(host, b, gx, gy, gw, gh, PC_WALL);
    for i in 0..(COLS * 2) {
        for j in 0..(ROWS * 2) {
            if (i + j) % 2 == 0 {
                rect(host, b, gx + i as f32 * cw / 2.0 + 7.0, gy + j as f32 * ch / 2.0 + 6.0, 2.0, 2.0, PC_WALL_DOT);
            }
        }
    }
    for i in 0..(COLS * ROWS) {
        let (sx, sy) = (gx + (i % COLS) as f32 * cw + 2.0, gy + (i / COLS) as f32 * ch + 2.0);
        let sel = i == v.sel;
        pill(host, b, sx - 1.0, sy - 1.0, cw - 2.0, ch - 2.0, if sel { PC_SEL } else { PC_SLOT_EDGE });
        pill(host, b, sx, sy, cw - 4.0, ch - 4.0, PC_SLOT);
        let Some(s) = v.list.get(i) else { continue };
        let k = kit(s);
        // A type-coloured "sprite" disc, and the name under it.
        let col = k.and_then(|k| k.types.first()).map_or(0xA0A0A0, |t| type_color(t));
        rect(host, b, sx + cw / 2.0 - 7.0, sy + 3.0, 10.0, 10.0, 0xFF00_0000 | col);
        if let Some(t2) = k.and_then(|k| k.types.get(1)) {
            rect(host, b, sx + cw / 2.0 - 2.0, sy + 3.0, 5.0, 10.0, 0xFF00_0000 | type_color(t2));
        }
        if *s == v.current {
            ball(host, b, sx + cw - 9.0, sy + 5.0, 3.0);
        }
        let label = k.map_or(s.clone(), |k| k.name.clone());
        let fitted = fit(host, &label, cw - 5.0);
        let lw = host.text_width(&fitted);
        host.text(b, &fitted, [sx + (cw - 4.0 - lw) / 2.0, sy + 15.0], 0x203048, false);
    }
    // The side panel: the selected Pokémon.
    let (px, py) = (gx + gw + 6.0, gy);
    pill(host, b, px, py, side, gh, 0xF018_2A48);
    if let Some(k) = v.list.get(v.sel).and_then(|s| kit(s)) {
        host.text(b, &fit(host, &k.name, side - 8.0), [px + 4.0, py + 4.0], 0xFFFFFF, true);
        let mut tx = px + 4.0;
        for t in &k.types {
            let label = t.to_uppercase();
            let tw = host.text_width(&label);
            pill(host, b, tx, py + 15.0, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
            host.text(b, &label, [tx + 3.0, py + 16.0], 0xFFFFFF, true);
            tx += tw + 9.0;
        }
        let bars = [("HP", k.base[0], 0xFF4C_D964u32), ("Atk", k.base[1].max(k.base[3]), 0xFFFF_8A3D), ("Def", k.base[2].max(k.base[4]), 0xFFE0_C040), ("Spe", k.base[5], 0xFF3F_A9F5)];
        for (i, (label, val, col)) in bars.iter().enumerate() {
            let by = py + 30.0 + i as f32 * 10.0;
            host.text(b, label, [px + 4.0, by], 0xC0C8D8, true);
            pill(host, b, px + 26.0, by + 1.0, side - 32.0, 6.0, 0xFF26_2626);
            rect(host, b, px + 27.0, by + 2.0, (side - 34.0) * (val / 150.0).clamp(0.0, 1.0), 4.0, *col);
        }
        for (i, m) in k.moves.iter().take(4).enumerate() {
            let my = py + 74.0 + i as f32 * 9.0;
            rect(host, b, px + 4.0, my + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&m.kind));
            host.text(b, &fit(host, &m.name, side - 14.0), [px + 11.0, my], 0xFFFFFF, true);
        }
        let tag = if v.list.get(v.sel) == Some(&v.current) { "In use" } else { "[Enter] Switch" };
        guide::keyed_centred(host, b, tag, px + side / 2.0, py + gh - 11.0, 0xFFE9A0);
    }
    guide::keyed_centred(host, b, "[WASD] Move  [Enter] Switch  [M] Moves  [R] Reset moves  [Esc] Close", x + w / 2.0, y + h - 19.0, 0xD8E4F4);
    if v.list.len() <= 1 {
        let lines = ["Only one Pokémon so far.", "KO wilds to research more."];
        let tw = lines.iter().map(|l| host.text_width(l)).fold(0.0, f32::max);
        let (cx, ty) = (gx + gw / 2.0, gy + gh / 2.0);
        rect(host, b, cx - tw / 2.0 - 6.0, ty - 3.0, tw + 12.0, 26.0, 0xE018_2A4A);
        for (i, l) in lines.iter().enumerate() {
            let lw = host.text_width(l);
            host.text(b, l, [cx - lw / 2.0, ty + 1.0 + i as f32 * 11.0], 0xFFFFFF, true);
        }
    }
    if let Some((t, at)) = &c.note {
        if at.elapsed().as_secs_f32() < 4.0 {
            let tw = host.text_width(t);
            host.text(b, t, [x + (w - tw) / 2.0, y + h - 9.0], 0xFFE9A0, true);
        }
    }
}
