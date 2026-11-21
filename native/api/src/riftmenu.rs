//! Lumi's Rift menu and the Rift Capsule opening (Onii's play-test, 2026-11-14, item 26).
//!
//! F on Lumi at her Matsuri stall opens the menu (the server sends it): Open capsule (with how many),
//! Start rift (pick a mode), Daily rift (each mode's seed and whether today's attempt is used) and
//! Leaderboards (each mode's daily and season top 5). W/S choose (held keys repeat), Space or Enter
//! pick, X goes back and closes.
//!
//! A capsule opening plays in front of the camera (`fx.rs` `capsule_begin`, docs/fx/
//! CAPSULE_OPENING.md): it appears, shakes and charges, bursts with a flash and a beam in its
//! rarity's colour, and a card names the prize. The server rolled the prize before it said so;
//! this side only shows it. Space, Enter or X skip to the burst after the first second, and close
//! the card after it. Opened from the menu, the menu comes back after it.
//!
//! Wire: the server half's notes (`server/rift_menu.rs`), and `capsule\t<rarity>\t<id>\t<name>\t<new or
//! shards>` (`server/capsules.rs`).
//!
//! Eimi's IV training menu (Onii, 2026-11-20) is a page of the same window: F on her sends
//! `ivmenu\t<ticket price>\t<line name>\t<6 IVs, comma separated>\t<6 Alpha names, | separated>`;
//! the player picks the IV to train (`riftm\ttrain\t<0-5>`) and fights that IV's Alpha 1v1.

use super::*;

// ---------------------------------------------------------------- the wire

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mode {
    pub id: String,
    pub name: String,
    /// The capsule multiplier as the server wrote it ("1.5").
    pub mult: String,
    pub seed: String,
    /// Today's Daily attempt is used.
    pub done: bool,
    /// Its clear time, 0 if not cleared.
    pub ticks: u64,
    pub blurb: String,
    pub daily: Vec<(String, u64)>,
    pub season: Vec<(String, u32, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    /// Common, rare, legendary.
    pub caps: [u32; 3],
    pub season: u32,
    pub modes: Vec<Mode>,
}

/// A `riftmenu` line with a view (not the `-` close).
pub fn parse_view(f: &[&str]) -> Option<View> {
    let ["riftmenu", caps, season, modes @ ..] = f else { return None };
    let c: Vec<u32> = caps.split(',').map(|n| n.parse().unwrap_or(0)).collect();
    let caps = [*c.first()?, *c.get(1)?, *c.get(2)?];
    let entries = |s: &str| -> Vec<Vec<String>> { s.split(';').filter(|e| !e.is_empty()).map(|e| e.split(':').map(str::to_string).collect()).collect() };
    let modes = modes
        .iter()
        .filter_map(|m| {
            let p: Vec<&str> = m.split('|').collect();
            if p.len() < 9 || p[0].is_empty() {
                return None;
            }
            Some(Mode {
                id: p[0].to_string(),
                name: p[1].to_string(),
                mult: p[2].to_string(),
                seed: p[3].to_string(),
                done: p[4] == "1",
                ticks: p[5].parse().unwrap_or(0),
                blurb: p[6].to_string(),
                daily: entries(p[7]).into_iter().filter_map(|e| Some((e.first()?.clone(), e.get(1)?.parse().ok()?))).collect(),
                season: entries(p[8]).into_iter().filter_map(|e| Some((e.first()?.clone(), e.get(1)?.parse().ok()?, e.get(2)?.parse().ok()?))).collect(),
            })
        })
        .collect();
    Some(View { caps, season: season.parse().unwrap_or(0), modes })
}

/// Eimi's IV training menu, from the server's `ivmenu` line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Train {
    pub price: u64,
    pub line: String,
    pub ivs: [u8; 6],
    pub alphas: Vec<String>,
}

pub fn parse_train(f: &[&str]) -> Option<Train> {
    let ["ivmenu", price, line, ivs, alphas, ..] = f else { return None };
    let v: Vec<u8> = ivs.split(',').filter_map(|n| n.parse().ok()).collect();
    let ivs: [u8; 6] = v.try_into().ok()?;
    Some(Train { price: price.parse().ok()?, line: line.to_string(), ivs, alphas: alphas.split('|').map(str::to_string).collect() })
}

const IV_NAMES: [&str; 6] = ["HP", "Attack", "Defense", "Sp. Atk", "Sp. Def", "Speed"];

/// What a capsule gave, from the server's `capsule` line.
#[derive(Clone, Debug, PartialEq)]
pub struct Prize {
    /// 0 common, 1 rare, 2 legendary.
    pub tier: u32,
    pub name: String,
    /// None: new. Some: a duplicate, paid in shards.
    pub shards: Option<u32>,
    /// The cosmetic's id (empty from an "Open x10" line).
    pub id: String,
}

/// Up to this many capsules open at once ("Open x10", Onii 2026-11-20).
pub const MANY: u32 = 10;

/// The server's `capsules` line for an "Open x10": `capsules\t<rarity>|<name>|<new or shards>\t...`.
pub fn parse_many(f: &[&str]) -> Option<Vec<Prize>> {
    let ["capsules", rest @ ..] = f else { return None };
    let out: Vec<Prize> = rest
        .iter()
        .filter_map(|e| {
            let mut p = e.splitn(3, '|');
            let (r, n, w) = (p.next()?, p.next()?, p.next()?);
            parse_reveal(&["capsule", r, "", n, w])
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

pub fn parse_reveal(f: &[&str]) -> Option<Prize> {
    let ["capsule", rarity, id, name, what, ..] = f else { return None };
    let tier = match *rarity {
        "common" => 0,
        "rare" => 1,
        "legendary" => 2,
        _ => return None,
    };
    let shards = if *what == "new" { None } else { Some(what.parse().ok()?) };
    Some(Prize { tier, name: name.to_string(), shards, id: id.to_string() })
}

fn tier_rarity(tier: u32) -> cosmetics::Rarity {
    match tier {
        0 => cosmetics::Rarity::Common,
        1 => cosmetics::Rarity::Rare,
        _ => cosmetics::Rarity::Legendary,
    }
}

const TIER_WORD: [&str; 3] = ["Common", "Rare", "Legendary"];

/// m:ss.t, like the server's boards.
pub fn clock(ticks: u64) -> String {
    format!("{}:{:02}.{}", ticks / 1200, (ticks / 20) % 60, (ticks % 20) / 2)
}

// ---------------------------------------------------------------- the menu

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Main,
    Start,
    Daily,
    Boards,
    /// One mode's boards.
    Board(usize),
    /// Eimi's IV training: pick the IV.
    Train,
}

const MAIN: [&str; 5] = ["Open capsule", "Open x10", "Start rift", "Daily rift", "Leaderboards"];

struct Ui {
    view: View,
    train: Option<Train>,
    page: Page,
    /// The chosen row on the main page, and on a page of modes.
    main: usize,
    sel: usize,
}

#[derive(Default)]
struct State {
    ui: Option<Ui>,
    note: Option<(String, Instant)>,
    /// Open capsule was picked here, at this time: its opening brings the menu back after.
    asked_capsule: Option<Instant>,
    /// A capsule opening on screen, and whether the menu comes back after it.
    prize: Option<(Prize, bool)>,
    /// An "Open x10": every prize, for the card (the opening plays the best one's rarity).
    many: Vec<Prize>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `riftmenu`, `riftnote` or `capsule` line; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    match f {
        ["riftmenu", "-", ..] => lock().ui = None,
        // A late refresh of a menu this client just closed for another one.
        ["riftmenu", ..] if !is_open() && !menus::server_may_open(menus::Menu::Rift) => {}
        ["riftmenu", ..] => {
            let Some(view) = parse_view(f) else { return true };
            let mut s = lock();
            match s.ui.as_mut() {
                Some(ui) => ui.view = view,
                None => {
                    s.ui = Some(Ui { view, train: None, page: Page::Main, main: 0, sel: 0 });
                    s.note = None;
                    drop(s);
                    menus::opening(menus::Menu::Rift);
                }
            }
        }
        ["riftnote", text, ..] => lock().note = Some((text.to_string(), Instant::now())),
        ["ivmenu", ..] => {
            let Some(t) = parse_train(f) else { return true };
            if !is_open() && !menus::server_may_open(menus::Menu::Rift) {
                return true;
            }
            let mut s = lock();
            let fresh = s.ui.is_none();
            s.ui = Some(Ui { view: View::default(), train: Some(t), page: Page::Train, main: 0, sel: 0 });
            s.note = None;
            drop(s);
            if fresh {
                menus::opening(menus::Menu::Rift);
            }
        }
        ["capsule" | "capsules", ..] => {
            let (prize, many) = if f[0] == "capsules" {
                let Some(all) = parse_many(f) else { return true };
                let Some(best) = all.iter().max_by_key(|p| p.tier).cloned() else { return true };
                (best, all)
            } else {
                let Some(prize) = parse_reveal(f) else { return true };
                (prize, Vec::new())
            };
            let tier = prize.tier;
            {
                let mut s = lock();
                let again = s.asked_capsule.take().is_some_and(|at| at.elapsed().as_secs_f32() < 5.0);
                s.prize = Some((prize, again));
                s.many = many;
            }
            // A trail prize's own particles circle the burst; an x10 shows the list instead.
            let trail = lock().prize.as_ref().map(|(p, _)| p.id.clone()).filter(|id| id.starts_with("trail.") && f[0] == "capsule");
            crate::fx::capsule_begin_with(tier, cosmetics::rarity_color(tier_rarity(tier)), trail);
            menus::opening(menus::Menu::Capsule);
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *lock() = State::default();
    crate::fx::capsule_stop();
}

pub fn is_open() -> bool {
    lock().ui.is_some()
}

/// Closes the menu (another one is opening); true when it was open.
pub fn shut() -> bool {
    lock().ui.take().is_some()
}

/// A capsule opening is on screen.
pub fn revealing() -> bool {
    lock().prize.is_some()
}

/// Ends the opening (another menu is opening over it).
pub fn stop_reveal() {
    lock().prize = None;
    crate::fx::capsule_stop();
}

/// Each frame: an opening that has played out goes, and the menu it came from comes back.
pub fn tick(host: &mut dyn Host) {
    let done = {
        let mut s = lock();
        if s.prize.is_some() && crate::fx::capsule_time().is_none() { s.prize.take() } else { None }
    };
    if let Some((_, again)) = done {
        if again {
            menus::asked(menus::Menu::Rift);
            let _ = host.net_send_to_server(CHANNEL, b"riftm\topen");
        }
    }
}

fn rows(ui: &Ui) -> usize {
    match ui.page {
        Page::Train => 6,
        Page::Main => MAIN.len(),
        Page::Start | Page::Daily | Page::Boards => ui.view.modes.len().max(1),
        Page::Board(_) => 1,
    }
}

/// The chosen row of the page showing.
fn cursor(ui: &mut Ui) -> &mut usize {
    if ui.page == Page::Main { &mut ui.main } else { &mut ui.sel }
}

/// Keys while the menu or an opening is up (all of them are taken).
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    if revealing() {
        if matches!(key, "key.keyboard.space" | "key.keyboard.enter" | "key.keyboard.x") {
            crate::fx::capsule_skip();
            tick(host);
        }
        return true;
    }
    let mut s = lock();
    let Some(ui) = s.ui.as_mut() else { return false };
    let n = rows(ui);
    let mut send: Option<String> = None;
    let mut note: Option<&str> = None;
    match key {
        "key.keyboard.up" | "key.keyboard.w" => {
            let i = cursor(ui);
            *i = (*i + n - 1) % n;
        }
        "key.keyboard.down" | "key.keyboard.s" => {
            let i = cursor(ui);
            *i = (*i + 1) % n;
        }
        "key.keyboard.space" | "key.keyboard.enter" => {
            let mode = ui.view.modes.get(ui.sel.min(ui.view.modes.len().saturating_sub(1))).map(|m| m.id.clone());
            match (ui.page, ui.main) {
                (Page::Main, m @ (0 | 1)) => {
                    if ui.view.caps.iter().sum::<u32>() == 0 {
                        note = Some("No capsules yet. Rifts drop them.");
                    } else if m == 0 {
                        send = Some("riftm\tcaps".to_string());
                    } else {
                        send = Some(format!("riftm\tcaps\t{MANY}"));
                    }
                }
                (Page::Main, 2) => (ui.page, ui.sel) = (Page::Start, 0),
                (Page::Main, 3) => (ui.page, ui.sel) = (Page::Daily, 0),
                (Page::Main, _) => (ui.page, ui.sel) = (Page::Boards, 0),
                (Page::Start, _) => send = mode.map(|m| format!("riftm\tstart\t{m}")),
                (Page::Daily, _) => {
                    let used = ui.view.modes.get(ui.sel).is_some_and(|m| m.done);
                    if used {
                        note = Some("Today's run is used. Back tomorrow.");
                    } else {
                        send = mode.map(|m| format!("riftm\tdaily\t{m}"));
                    }
                }
                (Page::Boards, _) if !ui.view.modes.is_empty() => ui.page = Page::Board(ui.sel),
                (Page::Train, _) => {
                    let maxed = ui.train.as_ref().is_some_and(|t| t.ivs.get(ui.sel).is_some_and(|v| *v >= 31));
                    if maxed {
                        note = Some("That IV is already maxed.");
                    } else {
                        send = Some(format!("riftm\ttrain\t{}", ui.sel));
                    }
                }
                _ => {}
            }
        }
        "key.keyboard.x" | "key.keyboard.backspace" => match ui.page {
            Page::Main | Page::Train => {
                s.ui = None;
                send = Some("riftm\tclose".to_string());
            }
            Page::Board(i) => (ui.page, ui.sel) = (Page::Boards, i),
            _ => ui.page = Page::Main,
        },
        _ => {}
    }
    if send.as_deref().is_some_and(|l| l.starts_with("riftm\tcaps")) {
        s.asked_capsule = Some(Instant::now());
    }
    if let Some(t) = note {
        s.note = Some((t.to_string(), Instant::now()));
    }
    drop(s);
    if let Some(line) = send {
        let _ = host.net_send_to_server(CHANNEL, line.as_bytes());
    }
    true
}

// ---------------------------------------------------------------- drawing

// Colours: a rift's violet, so it never reads as the PC (blue) or the Tutor (dojo brown).
const FRAME: u32 = 0xF02A_1E48;
const HEAD: u32 = 0xFF6A_4BC4;
const SLOT: u32 = 0xE6EE_E8FA;
const SEL: u32 = 0xFFFF_D23F;
const DIM: u32 = 0x6A6478;

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    reveal_hud(host, b);
    let s = lock();
    let Some(ui) = s.ui.as_ref() else { return };
    let v = &ui.view;
    // Five rows fit above the info line; each extra row (Eimi's six IVs) makes the panel taller.
    let (w, h) = (240.0f32, 116.0f32 + rows(ui).saturating_sub(5) as f32 * 12.0);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, FRAME);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, HEAD);
    let title = match ui.page {
        Page::Main => "Wild Rifts".to_string(),
        Page::Start => "Start rift".to_string(),
        Page::Daily => "Daily rift".to_string(),
        Page::Boards => "Leaderboards".to_string(),
        Page::Board(i) => v.modes.get(i).map_or_else(String::new, |m| m.name.clone()),
        Page::Train => "Eimi's IV training".to_string(),
    };
    host.text(b, &title, [x + 8.0, y + 6.0], 0xFFFFFF, true);
    let row = |host: &mut dyn Host, b: &mut HudBatch, i: usize, chosen: bool, left: &str, right: &str, right_rgb: u32| {
        let ry = y + 22.0 + i as f32 * 12.0;
        rect(host, b, x + 5.0, ry - 1.0, w - 10.0, 11.0, if chosen { SEL } else { SLOT });
        host.text(b, &fit(host, left, w - 80.0), [x + 9.0, ry + 0.5], 0x202830, false);
        let rw = host.text_width(right);
        host.text(b, right, [x + w - 9.0 - rw, ry + 0.5], right_rgb, false);
    };
    let mut info = String::new();
    match ui.page {
        Page::Main => {
            let total: u32 = v.caps.iter().sum();
            for (i, label) in MAIN.iter().enumerate() {
                let right = match i {
                    0 => format!("x{total}"),
                    1 => format!("x{}", total.min(MANY)),
                    _ => String::new(),
                };
                row(host, b, i, i == ui.main, label, &right, if total > 0 { 0x6A4BC4 } else { DIM });
            }
            if ui.main <= 1 {
                info = [(v.caps[0], "common"), (v.caps[1], "rare"), (v.caps[2], "legendary")].iter().filter(|(n, _)| *n > 0).map(|(n, w)| format!("{n} {w}")).collect::<Vec<_>>().join(", ");
            }
        }
        Page::Start | Page::Daily | Page::Boards => {
            for (i, m) in v.modes.iter().enumerate() {
                let (right, rgb) = match ui.page {
                    Page::Start => (format!("x{} capsules", m.mult), 0x6A4BC4),
                    Page::Daily if m.done && m.ticks > 0 => (format!("done {}", clock(m.ticks)), 0x2E7D32),
                    Page::Daily if m.done => ("done".to_string(), DIM),
                    Page::Daily => ("open".to_string(), 0x2E7D32),
                    _ => (m.daily.first().map_or_else(String::new, |(n, t)| format!("{n} {}", clock(*t))), DIM),
                };
                row(host, b, i, i == ui.sel, &m.name, &right, rgb);
            }
            if let Some(m) = v.modes.get(ui.sel) {
                info = match ui.page {
                    Page::Daily => format!("Seed {}", m.seed),
                    Page::Boards => String::new(),
                    _ => m.blurb.clone(),
                };
            }
        }
        Page::Train => {
            if let Some(t) = &ui.train {
                for (i, name) in IV_NAMES.iter().enumerate() {
                    let iv = t.ivs[i];
                    let (right, rgb) = if iv >= 31 { ("31 max".to_string(), DIM) } else { (format!("{iv} / 31"), 0x6A4BC4) };
                    row(host, b, i, i == ui.sel, name, &right, rgb);
                }
                let alpha = t.alphas.get(ui.sel).cloned().unwrap_or_default();
                info = format!("{}: 1v1 vs {alpha}  \u{20B1}{}", t.line, t.price);
            }
        }
        Page::Board(i) => {
            if let Some(m) = v.modes.get(i) {
                let col = |host: &mut dyn Host, b: &mut HudBatch, cx: f32, head: &str, lines: Vec<String>| {
                    host.text(b, head, [cx, y + 22.0], 0xFFE9A0, true);
                    if lines.is_empty() {
                        host.text(b, "No clears yet.", [cx, y + 34.0], 0xB8B0C8, true);
                    }
                    for (n, l) in lines.iter().enumerate() {
                        host.text(b, &fit(host, l, w / 2.0 - 12.0), [cx, y + 34.0 + n as f32 * 10.0], 0xFFFFFF, true);
                    }
                };
                let today: Vec<String> = m.daily.iter().enumerate().map(|(n, (who, t))| format!("{}. {who} {}", n + 1, clock(*t))).collect();
                let season: Vec<String> = m.season.iter().enumerate().map(|(n, (who, c, _))| format!("{}. {who} {c}", n + 1)).collect();
                col(host, b, x + 9.0, "Today", today);
                col(host, b, x + w / 2.0 + 4.0, &format!("Season {}", v.season), season);
            }
        }
    }
    if !info.is_empty() {
        let t = fit(host, &info, w - 14.0);
        let tw = host.text_width(&t);
        host.text(b, &t, [x + (w - tw) / 2.0, y + h - 33.0], 0xE0D4FF, true);
    }
    let keys = match ui.page {
        Page::Main => "[W] [S] choose  [Space] pick  [X] close",
        Page::Train => "[W] [S] choose  [Space] train  [X] close",
        Page::Board(_) => "[X] back",
        _ => "[W] [S] choose  [Space] pick  [X] back",
    };
    guide::keyed_centred(host, b, keys, x + w / 2.0, y + h - 22.0, 0xE8E0FF);
    if let Some((t, at)) = &s.note {
        if at.elapsed().as_secs_f32() < 5.0 {
            let fitted = fit(host, t, w - 10.0);
            let lw = host.text_width(&fitted);
            host.text(b, &fitted, [x + (w - lw) / 2.0, y + h - 11.0], 0xFFE9A0, true);
        }
    }
}

/// The opening's card: the rarity, the prize's name and whether it's new, after the burst. Without
/// the effects pass (Low FX, or the pass switched itself off) a small capsule shakes on screen and
/// a flash bursts it.
fn reveal_hud(host: &mut dyn Host, b: &mut HudBatch) {
    let Some((prize, _)) = lock().prize.clone() else { return };
    let Some(t) = crate::fx::capsule_time() else { return };
    let rgb = cosmetics::rarity_color(tier_rarity(prize.tier));
    let (cx, cy) = (b.width / 2.0, b.height / 2.0);
    let burst = crate::fx::CAPSULE_BURST;
    if crate::fx::capsule_flat() {
        if t < burst {
            let ch = ((t - 0.45) / (burst - 0.45)).clamp(0.0, 1.0);
            let dx = (t * (9.0 + 26.0 * ch)).sin() * (1.0 + 4.0 * ch * ch);
            let (w, h) = (16.0, 22.0);
            rect(host, b, cx - w / 2.0 + dx, cy - h / 2.0, w, h / 2.0, 0xFF00_0000 | rgb);
            rect(host, b, cx - w / 2.0 + dx, cy, w, h / 2.0, 0xFFE8_E8EE);
            rect(host, b, cx - w / 2.0 + dx, cy - 1.0, w, 2.0, 0xFF1A_1820);
        } else if t < burst + 0.4 {
            let a = ((1.0 - (t - burst) / 0.4) * 200.0) as u32;
            rect(host, b, 0.0, 0.0, b.width, b.height, (a << 24) | 0xFFFFFF);
        }
    }
    if t < 1.0 {
        return;
    }
    if t < burst {
        guide::keyed_centred(host, b, "[Space] skip", cx, b.height - 60.0, 0xD8D0E8);
        return;
    }
    let k = t - burst;
    if k < 0.35 {
        return;
    }
    let many = lock().many.clone();
    if !many.is_empty() {
        return many_card(host, b, &many, cx);
    }
    // The card, in the lower third so the beam stays clear.
    let word = TIER_WORD[prize.tier.min(2) as usize];
    let sub = match prize.shards {
        None => "New!".to_string(),
        Some(n) => format!("Owned: +{n} shards"),
    };
    let w = (host.text_width(&prize.name).max(host.text_width(&sub)) + 28.0).max(120.0);
    let (x, y) = (cx - w / 2.0, b.height * 0.68);
    pill(host, b, x, y, w, 40.0, 0xE014_1020);
    rect(host, b, x + 2.0, y + 2.0, w - 4.0, 2.0, 0xFF00_0000 | rgb);
    let ww = host.text_width(word);
    host.text(b, word, [cx - ww / 2.0, y + 7.0], rgb, true);
    let nw = host.text_width(&prize.name);
    host.text(b, &prize.name, [cx - nw / 2.0, y + 18.0], 0xFFFFFF, true);
    let sw = host.text_width(&sub);
    host.text(b, &sub, [cx - sw / 2.0, y + 29.0], if prize.shards.is_none() { 0xFFE9A0 } else { 0xB8B0C8 }, true);
}

/// An "Open x10"'s card: every prize, best first, one line each in its rarity's colour.
fn many_card(host: &mut dyn Host, b: &mut HudBatch, many: &[Prize], cx: f32) {
    let mut list: Vec<&Prize> = many.iter().collect();
    list.sort_by_key(|p| std::cmp::Reverse(p.tier));
    let line = |p: &Prize| match p.shards {
        None => format!("{}  New!", p.name),
        Some(n) => format!("{}  +{n} shards", p.name),
    };
    let head = format!("{} capsules", many.len());
    let w = list.iter().map(|p| host.text_width(&line(p))).fold(host.text_width(&head), f32::max) + 28.0;
    let h = 18.0 + list.len() as f32 * 10.0 + 6.0;
    let (x, y) = (cx - w / 2.0, (b.height - h - 30.0).min(b.height * 0.55));
    pill(host, b, x, y, w, h, 0xE014_1020);
    let hw = host.text_width(&head);
    host.text(b, &head, [cx - hw / 2.0, y + 6.0], 0xFFE9A0, true);
    for (n, p) in list.iter().enumerate() {
        let rgb = cosmetics::rarity_color(tier_rarity(p.tier));
        let t = line(p);
        let tw = host.text_width(&t);
        host.text(b, &t, [cx - tw / 2.0, y + 18.0 + n as f32 * 10.0], rgb, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what this client sends.
    #[derive(Default)]
    struct Sent(Vec<String>);

    impl Host for Sent {
        fn net_send_to_server(&mut self, _: &str, data: &[u8]) -> Result<(), String> {
            self.0.push(String::from_utf8_lossy(data).into_owned());
            Ok(())
        }
    }

    const VIEW: &str = "riftmenu\t2,1,0\t3\tnormal|Normal|1|0A1B2C3D|1|900|The usual rift.|Lia:900|Lia:4:880\ttype|Type Advantage|1.5|11223344|0|0|Every foe hits super effective.||\talpha|All Alphas|2|55667788|0|0|Every foe is an Alpha.||";

    fn show(line: &str) {
        let f: Vec<&str> = line.split('\t').collect();
        assert!(heard(&f));
    }

    fn press(host: &mut Sent, keys: &str) {
        for k in keys.split_whitespace() {
            assert!(key(host, &format!("key.keyboard.{k}")), "{k} is taken while the menu is up");
        }
    }

    fn reset() {
        forget();
        menus::asked_any();
    }

    #[test]
    fn the_menu_reads_the_server_line() {
        let v = parse_view(&VIEW.split('\t').collect::<Vec<_>>()).unwrap();
        assert_eq!((v.caps, v.season, v.modes.len()), ([2, 1, 0], 3, 3));
        assert_eq!(v.modes[0].daily, [("Lia".to_string(), 900)]);
        assert_eq!(v.modes[0].season, [("Lia".to_string(), 4, 880)]);
        assert!(v.modes[0].done && !v.modes[1].done);
        assert_eq!((v.modes[1].mult.as_str(), v.modes[1].seed.as_str()), ("1.5", "11223344"));
        assert!(parse_view(&["riftmenu", "x"]).is_none());
        assert_eq!(clock(1250), "1:02.5");
    }

    #[test]
    fn each_row_sends_its_own_request() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        menus::asked(menus::Menu::Rift);
        show(VIEW);
        assert_eq!(menus::open_now(), Some(menus::Menu::Rift));
        // Start rift, the third mode: All Alphas.
        press(&mut host, "s s space s s space");
        assert_eq!(host.0, ["riftm\tstart\talpha"]);
        // Back to the main page, Daily rift: Normal's is used today, Type Advantage's isn't.
        press(&mut host, "x s space space");
        assert_eq!(host.0.len(), 1, "a used Daily isn't asked for");
        assert!(lock().note.as_ref().is_some_and(|(t, _)| t.contains("used")));
        press(&mut host, "s space");
        assert_eq!(host.0[1], "riftm\tdaily\ttype");
        // Leaderboards: a mode's boards, and back.
        press(&mut host, "x s space space");
        assert!(matches!(lock().ui.as_ref().map(|u| u.page), Some(Page::Board(0))));
        press(&mut host, "x x");
        // Open capsule (the main page keeps its own row: Leaderboards), then close.
        press(&mut host, "s space");
        assert_eq!(host.0[2], "riftm\tcaps");
        press(&mut host, "x");
        assert_eq!(host.0[3], "riftm\tclose");
        assert!(!is_open());
        assert!(!key(&mut host, "key.keyboard.s"), "closed: keys go back to the game");
        reset();
    }

    #[test]
    fn open_x10_asks_for_ten_and_shows_every_prize() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        menus::asked(menus::Menu::Rift);
        show(VIEW);
        press(&mut host, "s space");
        assert_eq!(host.0, ["riftm\tcaps\t10"]);
        show("capsules\tcommon|Leaf Trail|new\trare|Frost Trail|20\tbad");
        let s = lock();
        assert_eq!(s.many.len(), 2, "a bad entry is skipped");
        assert_eq!(s.prize.as_ref().map(|(p, _)| p.tier), Some(1), "the opening plays the best one");
        drop(s);
        crate::fx::capsule_stop();
        reset();
    }

    #[test]
    fn eimi_lets_you_pick_the_iv_to_train() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        menus::asked_any();
        show("ivmenu\t250\tBuneary\t10,31,4,0,12,20\tSnorlax or Blissey|Machamp or Conkeldurr|Steelix or Aggron|Alakazam or Gengar|Florges or Umbreon|Ninjask or Jolteon");
        assert!(is_open());
        assert_eq!(lock().ui.as_ref().map(|u| u.page), Some(Page::Train));
        press(&mut host, "s space");
        assert!(host.0.is_empty(), "Attack is maxed: nothing asked");
        assert!(lock().note.as_ref().is_some_and(|(t, _)| t.contains("maxed")));
        press(&mut host, "s s s s space");
        assert_eq!(host.0, ["riftm\ttrain\t5"]);
        press(&mut host, "x");
        assert_eq!(host.0[1], "riftm\tclose");
        assert!(parse_train(&["ivmenu", "1", "B", "1,2,3", "x"]).is_none(), "six IVs or nothing");
        reset();
    }

    #[test]
    fn no_capsules_asks_nothing() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        menus::asked(menus::Menu::Rift);
        show(&VIEW.replacen("2,1,0", "0,0,0", 1));
        press(&mut host, "space");
        assert!(host.0.is_empty());
        assert!(lock().note.is_some());
        reset();
    }

    #[test]
    fn an_opening_replaces_the_menu_and_brings_it_back() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        menus::asked(menus::Menu::Rift);
        show(VIEW);
        press(&mut host, "space");
        assert_eq!(host.0, ["riftm\tcaps"]);
        show("capsule\trare\ttrail.comet\tComet Trail\tnew");
        assert_eq!(menus::open_now(), Some(menus::Menu::Capsule), "the menu made way");
        assert!(!is_open());
        assert!(crate::fx::capsule_time().is_some());
        // Too early to skip: nothing moves.
        press(&mut host, "space");
        assert!(crate::fx::capsule_time().unwrap() < 1.0);
        // The opening ends (as `fx::frame` ends it at CAPSULE_END): the menu is asked for again.
        crate::fx::capsule_stop();
        tick(&mut host);
        assert!(!revealing());
        assert_eq!(host.0.last().map(String::as_str), Some("riftm\topen"));
        // Its answer opens it, though it was displaced a moment ago.
        show(VIEW);
        assert!(is_open());
        reset();
    }

    /// Review: Tab (the hub menu) used to reach `hub::key` first and end the opening at once,
    /// inside the first second no key may skip.
    #[test]
    fn tab_cannot_cut_an_opening_short() {
        let _g = menus::test_lock();
        reset();
        super::super::hub::forget();
        let mut host = Sent::default();
        show("capsule\tlegendary\ttrail.comet\tComet Trail\tnew");
        assert!(revealing());
        for k in ["tab", "m", "b", "f"] {
            assert!(super::super::key(&mut host, &format!("key.keyboard.{k}")), "{k} is taken during the opening");
            assert!(revealing(), "{k} left the opening playing");
        }
        assert!(!super::super::hub::is_open(), "the hub menu stayed shut");
        assert!(crate::fx::capsule_time().is_some_and(|t| t < 1.0));
        assert!(host.0.is_empty(), "no F use, nothing sent");
        super::super::hub::forget();
        reset();
    }

    #[test]
    fn a_typed_capsule_open_does_not_bring_the_menu() {
        let _g = menus::test_lock();
        reset();
        let mut host = Sent::default();
        show("capsule\tcommon\ttrail.t\tT\t5");
        assert!(revealing());
        crate::fx::capsule_stop();
        tick(&mut host);
        assert!(host.0.is_empty(), "/capsule open came from chat, not the menu");
        reset();
    }

    #[test]
    fn skips_wait_a_second_then_jump_to_the_burst_then_close() {
        use crate::fx::{CAPSULE_BURST, capsule_skip_to};
        assert_eq!(capsule_skip_to(0.5), Some(0.5), "not in the first second");
        assert_eq!(capsule_skip_to(1.2), Some(CAPSULE_BURST));
        assert_eq!(capsule_skip_to(CAPSULE_BURST + 0.1), Some(CAPSULE_BURST + 0.1), "the same press doesn't close it too");
        assert_eq!(capsule_skip_to(CAPSULE_BURST + 1.0), None, "then a press closes the card");
    }
}
