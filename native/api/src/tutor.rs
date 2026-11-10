//! Evolution stones and the Move Tutor (GDD 3.8 "stone → L18 (or a stone found in PvE)", "Egg/TM/
//! tutor moves through a Move Tutor NPC at Phase HQ"; 5.1 wilds "drop stones and Move Tutor
//! tokens").
//!
//! The Tutor stands in Phase HQ. F next to it (or `/tutor`) opens its menu: the moves it can teach
//! the Pokémon you are, what each costs in tokens, and which you already own. W/S choose, 1/2/3
//! put the move on Q/E/R (buying it first if it isn't yours yet), Esc closes. A bought move also
//! shows in the Nest PC's Moves list from then on.
//!
//! Wire (channel `capipimon:play`):
//! - server → client: `tutor\t<tokens>\t<q>\t<e>\t<r>\t<id:cost:owned:level,...  or ->` (open or
//!   refresh), `tutor\t-` (close), `tutornote\t<text>` (a line under the list),
//!   `tutored\t<id,id,... or ->` (the bought moves the current Pokémon can use; sent with the PC's
//!   Moves list and after a lesson).
//! - client → server: `tutor\topen`, `tutor\tteach\t<move id>\t<slot 1-3>`, `tutor\tclose`.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/tutor.rs");

// ---------------------------------------------------------------- client

struct Row {
    mv: Move,
    cost: u32,
    owned: bool,
    level: u32,
}

struct View {
    tokens: u32,
    slots: [String; 3],
    rows: Vec<Row>,
    sel: usize,
}

#[derive(Default)]
pub struct TutorClient {
    view: Option<View>,
    note: Option<(String, Instant)>,
}

pub static TUTOR_CLIENT: LazyLock<Mutex<TutorClient>> = LazyLock::new(Default::default);

/// A `tutor`/`tutornote`/`tutored` message; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    match f {
        ["tutor", "-", ..] => TUTOR_CLIENT.lock().unwrap().view = None,
        ["tutor", tokens, q, e, r, rows, ..] => {
            let rows: Vec<Row> = rows
                .split(',')
                .filter_map(|row| {
                    let mut p = row.split(':');
                    let mv = progress::playable(p.next()?)?.clone();
                    let cost = p.next()?.parse().ok()?;
                    let owned = p.next()? == "1";
                    let level = p.next().and_then(|l| l.parse().ok()).unwrap_or(1);
                    Some(Row { mv, cost, owned, level })
                })
                .collect();
            let mut c = TUTOR_CLIENT.lock().unwrap();
            let fresh = c.view.is_none();
            let sel = c.view.as_ref().map_or(0, |v| v.sel).min(rows.len().saturating_sub(1));
            c.view = Some(View { tokens: tokens.parse().unwrap_or(0), slots: [q.to_string(), e.to_string(), r.to_string()], rows, sel });
            if fresh {
                c.note = None;
            }
        }
        ["tutornote", text, ..] => TUTOR_CLIENT.lock().unwrap().note = Some((text.to_string(), Instant::now())),
        ["tutored", ids, ..] => {
            let moves: Vec<Move> = ids.split(',').filter_map(progress::playable).cloned().collect();
            pc::add_moves(&moves);
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *TUTOR_CLIENT.lock().unwrap() = TutorClient::default();
}

pub fn is_open() -> bool {
    TUTOR_CLIENT.lock().unwrap().view.is_some()
}

/// Keys while the Tutor's menu is open (all of them are taken, so moves don't fire).
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut c = TUTOR_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_mut() else { return false };
    let len = v.rows.len().max(1);
    let mut send: Option<String> = None;
    match key {
        "key.keyboard.up" | "key.keyboard.w" => v.sel = (v.sel + len - 1) % len,
        "key.keyboard.down" | "key.keyboard.s" => v.sel = (v.sel + 1) % len,
        "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" => {
            if let Some(row) = v.rows.get(v.sel) {
                send = Some(format!("tutor\tteach\t{}\t{}", row.mv.id, &key["key.keyboard.".len()..]));
            }
        }
        "key.keyboard.escape" | "key.keyboard.backspace" | "key.keyboard.e" => {
            c.view = None;
            send = Some("tutor\tclose".to_string());
        }
        _ => {}
    }
    drop(c);
    if let Some(s) = send {
        let _ = host.net_send_to_server(CHANNEL, s.as_bytes());
    }
    true
}

/// Standing still while the menu is open (W/S move the cursor).
pub fn movement_input(event: &mut MovementInputEvent) {
    if is_open() {
        event.forward = 0.0;
        event.strafe = 0.0;
    }
}

// Colours: a warm dojo board, so it never reads as the (blue) PC.
const FRAME: u32 = 0xF05A_3420;
const HEAD: u32 = 0xFFB8_6A2C;
const SLOT: u32 = 0xE6FC_F4E6;
const SLOT_OWNED: u32 = 0xE6DC_F2D8;
const SEL: u32 = 0xFFFF_D23F;

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    const SHOWN: usize = 9;
    let c = TUTOR_CLIENT.lock().unwrap();
    let Some(v) = c.view.as_ref() else { return };
    let (w, h) = (300.0f32, 158.0f32);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, HEAD);
    host.text(b, "Move Tutor", [x + 8.0, y + 6.0], 0xFFFFFF, true);
    let tokens = format!("{} token{}", v.tokens, if v.tokens == 1 { "" } else { "s" });
    let tw = host.text_width(&tokens);
    host.text(b, &tokens, [x + w - 8.0 - tw, y + 6.0], 0xFFF2B0, true);
    let bar = format!("Q {}   E {}   R {}", v.slots[0], v.slots[1], v.slots[2]);
    host.text(b, &fit(host, &bar, w - 14.0), [x + 7.0, y + 21.0], 0xFFE9A0, true);
    if v.rows.is_empty() {
        let t = "The Tutor has nothing new for this Pokémon.";
        let lw = host.text_width(t);
        host.text(b, t, [x + (w - lw) / 2.0, y + 70.0], 0xFFFFFF, true);
    }
    let level = progress::client_level();
    let start = v.sel.saturating_sub(SHOWN / 2).min(v.rows.len().saturating_sub(SHOWN));
    for (i, row) in v.rows.iter().enumerate().skip(start).take(SHOWN) {
        let ry = y + 33.0 + (i - start) as f32 * 11.0;
        let o = &row.mv;
        rect(host, b, x + 5.0, ry - 1.0, w - 10.0, 10.0, if i == v.sel { SEL } else if row.owned { SLOT_OWNED } else { SLOT });
        rect(host, b, x + 8.0, ry + 2.0, 4.0, 4.0, 0xFF00_0000 | type_color(&o.kind));
        host.text(b, &fit(host, &o.name, 104.0), [x + 15.0, ry], 0x202830, false);
        let stats = if o.support.is_some() { format!("support  {} PP", o.energy as u32) } else { format!("{} pow  {} PP", o.power as u32, o.energy as u32) };
        host.text(b, &stats, [x + 124.0, ry], 0x404A58, false);
        let (price, col) = if row.owned {
            ("owned".to_string(), 0x2E7D32)
        } else if level < row.level {
            (format!("level {}", row.level), 0x9A4A3A)
        } else {
            (format!("{} token{}", row.cost, if row.cost == 1 { "" } else { "s" }), if v.tokens >= row.cost { 0x202830 } else { 0x9A4A3A })
        };
        let pw = host.text_width(&price);
        host.text(b, &price, [x + w - 9.0 - pw, ry], col, false);
    }
    guide::keyed_centred(host, b, "[W] [S] choose  learn onto: [1] Q  [2] E  [3] R  [Esc] close", x + w / 2.0, y + h - 22.0, 0xF4E4D0);
    if let Some((t, at)) = &c.note {
        if at.elapsed().as_secs_f32() < 5.0 {
            let fitted = fit(host, t, w - 10.0);
            let lw = host.text_width(&fitted);
            host.text(b, &fitted, [x + (w - lw) / 2.0, y + h - 11.0], 0xFFE9A0, true);
        }
    }
}
