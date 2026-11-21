//! The Tab menu (GDD 3.1 keybinds: Tab is used only to open it). Everything that used to sit on a
//! key of its own or on screen all the time lives here: help, the map, who is online, the
//! wardrobe and shop, the Capipidex, emotes, chat, quests, party, photo mode, a move or evolution on
//! offer, the first steps and the news (welcome-back card, community goal, Event Night, XP
//! catch-up).
//!
//! Keys while open: W/S (or the arrows) choose, Space or left click accept, X or right click go
//! back, Tab closes (Esc is only the pause menu). Movement is held while it is open. The vanilla player list (also on
//! Tab) is off for everyone (`lib.rs` `gui_layer_pre`); its names are the Players page here.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Help,
    Map,
    Players,
    Learn,
    Evolve,
    Wardrobe,
    Shop,
    Dex,
    Emotes,
    Chat,
    Quests,
    Party,
    Photo,
    Steps,
    News,
}

impl Row {
    fn label(self) -> &'static str {
        match self {
            Row::Help => "Help & controls",
            Row::Map => "Map",
            Row::Players => "Players online",
            Row::Learn => "Learn a new move",
            Row::Evolve => "Evolve",
            Row::Wardrobe => "Wardrobe",
            Row::Shop => "Shop",
            Row::Dex => "Capipidex",
            Row::Emotes => "Emotes",
            Row::Chat => "Chat",
            Row::Quests => "Quests",
            Row::Party => "Party",
            Row::Photo => "Photo mode",
            Row::Steps => "First steps",
            Row::News => "News",
        }
    }

    /// Rows that open a list of their own inside the menu.
    fn has_list(self) -> bool {
        matches!(self, Row::Emotes | Row::Learn | Row::Evolve)
    }

    /// Rows that only show something on the right.
    fn info_only(self) -> bool {
        matches!(self, Row::Players | Row::Steps | Row::News)
    }
}

#[derive(Default)]
struct Hub {
    open: bool,
    sel: usize,
    /// The chosen row's own list is open: which entry is picked.
    sub: Option<usize>,
}

static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);

pub fn is_open() -> bool {
    HUB.lock().unwrap().open
}

pub fn forget() {
    *HUB.lock().unwrap() = Hub::default();
}

/// Closes the menu (another one is opening, `menus`).
pub fn close() {
    *HUB.lock().unwrap() = Hub::default();
}

/// Tab: opens the menu on its first row, closing any other menu, or closes it.
pub fn toggle() {
    let open = {
        let mut h = HUB.lock().unwrap();
        *h = Hub { open: !h.open, sel: 0, sub: None };
        h.open
    };
    if open {
        menus::opening(menus::Menu::Tab);
    }
}

/// The rows to show now: a move or an evolution only while one is on offer.
fn rows() -> Vec<Row> {
    let (learn, evolve) = progress::offers();
    rows_for(learn, evolve)
}

fn rows_for(learn: bool, evolve: bool) -> Vec<Row> {
    let mut out = vec![Row::Help, Row::Map, Row::Players];
    if learn {
        out.push(Row::Learn);
    }
    if evolve {
        out.push(Row::Evolve);
    }
    out.extend([Row::Wardrobe, Row::Shop, Row::Dex, Row::Emotes, Row::Chat, Row::Quests, Row::Party, Row::Photo, Row::Steps, Row::News]);
    out
}

/// The entries of a row's own list.
fn list_of(row: Row) -> Vec<String> {
    match row {
        Row::Emotes => crate::emotes::CATALOG.emotes.iter().map(|e| e.label.clone()).collect(),
        Row::Learn => match progress::learn_choices() {
            Some((_, [q, e, r])) => vec![format!("Replace {q} (Q)"), format!("Replace {e} (E)"), format!("Replace {r} (R)"), "Don't learn it".to_string()],
            None => Vec::new(),
        },
        Row::Evolve => progress::evo_names(),
        _ => Vec::new(),
    }
}

/// Holding F at an evolve prompt with several forms opens the menu on the Evolve list.
pub fn open_evolve() {
    let list = rows();
    let Some(i) = list.iter().position(|r| *r == Row::Evolve) else { return };
    *HUB.lock().unwrap() = Hub { open: true, sel: i, sub: Some(0) };
    menus::opening(menus::Menu::Tab);
}

/// The menu's keys; true when taken. Tab opens and closes it, and nothing else listens to Tab.
pub fn key(host: &mut dyn Host, key: &str) -> bool {
    if key == "key.keyboard.tab" {
        toggle();
        return true;
    }
    let mut h = HUB.lock().unwrap();
    if !h.open {
        return false;
    }
    let list = rows();
    h.sel = h.sel.min(list.len() - 1);
    let row = list[h.sel];
    if let Some(i) = h.sub {
        let items = list_of(row);
        let n = items.len().max(1);
        match key {
            "key.keyboard.w" | "key.keyboard.up" => h.sub = Some((i + n - 1) % n),
            "key.keyboard.s" | "key.keyboard.down" => h.sub = Some((i + 1) % n),
            "key.keyboard.space" | "key.keyboard.enter" if !items.is_empty() => {
                *h = Hub::default();
                drop(h);
                pick(host, row, i.min(items.len() - 1));
            }
            "key.keyboard.x" => h.sub = None,
            _ => {}
        }
        return true;
    }
    let n = list.len();
    match key {
        "key.keyboard.w" | "key.keyboard.up" => h.sel = (h.sel + n - 1) % n,
        "key.keyboard.s" | "key.keyboard.down" => h.sel = (h.sel + 1) % n,
        "key.keyboard.space" | "key.keyboard.enter" => {
            if row.has_list() {
                h.sub = Some(0);
            } else if !row.info_only() {
                *h = Hub::default();
                drop(h);
                accept(host, row);
            }
        }
        "key.keyboard.x" => *h = Hub::default(),
        _ => {}
    }
    true
}

/// The click hook fires every tick a button is held: one press is one step.
static LAST_CLICK: Mutex<Option<Instant>> = Mutex::new(None);
const CLICK_GAP: f32 = 0.3;

pub fn fresh_click() -> bool {
    let mut last = LAST_CLICK.lock().unwrap();
    let fresh = last.is_none_or(|at| at.elapsed().as_secs_f32() >= CLICK_GAP);
    *last = Some(Instant::now());
    fresh
}

/// Left click while the menu is open accepts, like Space. True when taken.
pub fn click(host: &mut dyn Host) -> bool {
    is_open() && (!fresh_click() || key(host, "key.keyboard.space"))
}

/// Right click while the menu is open goes back, like X. True when taken.
pub fn right_click(host: &mut dyn Host) -> bool {
    is_open() && (!fresh_click() || key(host, "key.keyboard.x"))
}

/// An entry of a row's own list was picked (the menu has closed).
fn pick(host: &mut dyn Host, row: Row, i: usize) {
    match row {
        Row::Emotes => {
            if let Some(e) = crate::emotes::CATALOG.emotes.get(i) {
                let _ = host.net_send_to_server(CHANNEL, format!("emote\t{}", e.id).as_bytes());
            }
        }
        Row::Learn => progress::pick_learn(host, i),
        Row::Evolve => progress::pick_evolve(host, i),
        _ => {}
    }
}

/// A row with nothing more to show was accepted (the menu has closed).
fn accept(host: &mut dyn Host, row: Row) {
    let chat = |host: &mut dyn Host, c: &str| {
        let _ = host.send_chat(c);
    };
    match row {
        Row::Help => guide::open_help(),
        Row::Map => worldmap::open(host),
        Row::Wardrobe => {
            menus::asked(menus::Menu::Wardrobe);
            chat(host, "/wardrobe");
        }
        Row::Shop => {
            cosmetics::draw::open_on_shop();
            menus::asked(menus::Menu::Wardrobe);
            chat(host, "/wardrobe");
        }
        Row::Dex => {
            menus::asked(menus::Menu::Dex);
            chat(host, "/dex");
        }
        // T and Enter no longer open the chat (`lib.rs`); the game half opens it (`frame.rs`).
        Row::Chat => host.set_setting(crate::MOD_ID, &format!("chat-{}", std::process::id()), "open"),
        Row::Quests => chat(host, "/quests"),
        Row::Party => chat(host, "/party"),
        Row::Photo => {
            photo::command(host, "/photo");
        }
        _ => {}
    }
}

const PANEL: u32 = 0xF018_1A26;
const HEAD: u32 = 0xFFB8_2A1A;
const PICK: u32 = 0xFF2A_4F8C;
const DIM: u32 = 0xC0C8D8;
const ROW_H: f32 = 12.0;

/// The small "Tab" reminder in the top right (Onii's mockup): a menu icon over the word TAB.
pub fn reminder(host: &mut dyn Host, event: &mut HudEvent<'_>) {
    let (w, h) = (28.0f32, 22.0f32);
    let (x, y) = event.corners.place(Corner::TopRight, w, h);
    let b = &mut *event.batch;
    pill(host, b, x, y, w, h, 0xB018_1A26);
    for i in 0..3 {
        rect(host, b, x + 4.0, y + 3.0 + i as f32 * 2.5, w - 8.0, 1.0, 0xFFE8_ECF4);
    }
    let tw = host.text_width("TAB");
    host.text(b, "TAB", [x + (w - tw) / 2.0, y + 11.5], 0xFFFFFF, true);
}

/// The menu itself, over everything; true when it drew.
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) -> bool {
    let (sel, sub) = {
        let h = HUB.lock().unwrap();
        if !h.open {
            return false;
        }
        (h.sel, h.sub)
    };
    let list = rows();
    let sel = sel.min(list.len() - 1);
    let row = list[sel];
    let (sw, sh) = (b.width, b.height);
    rect(host, b, 0.0, 0.0, sw, sh, 0x9010_1820);
    let w = 380.0f32.min(sw - 8.0);
    let h = (36.0 + list.len() as f32 * ROW_H + 16.0).min(sh - 8.0);
    let (x, y) = ((sw - w) / 2.0, (sh - h) / 2.0);
    pill(host, b, x, y, w, h, PANEL);
    pill(host, b, x, y, w, 14.0, HEAD);
    emblem(host, b, x + 8.0, y + 7.0, 4.5);
    host.text(b, "Menu", [x + 16.0, y + 3.0], 0xFFFFFF, true);
    let lw = 118.0f32;
    for (i, r) in list.iter().enumerate() {
        let ly = y + 20.0 + i as f32 * ROW_H;
        if i == sel {
            pill(host, b, x + 4.0, ly - 2.0, lw, ROW_H, if sub.is_some() { 0xFF3A_3D4D } else { PICK });
        }
        host.text(b, r.label(), [x + 9.0, ly], if i == sel { 0xFFFFFF } else { DIM }, true);
    }
    let (px, pw) = (x + lw + 14.0, w - lw - 20.0);
    let bottom = y + h - 16.0;
    let mut py = y + 20.0;
    host.text(b, row.label(), [px, py], 0xFFE9A0, true);
    py += 15.0;
    let line = |host: &mut dyn Host, b: &mut HudBatch, py: &mut f32, t: &str, rgb: u32| {
        for l in guide::wrap(&*host, t, pw) {
            if *py + 10.0 <= bottom {
                guide::keyed(host, b, &l, [px, *py], rgb);
                *py += 10.0;
            }
        }
    };
    match row {
        Row::Players => {
            let mut names: Vec<String> = host.client_profiles().into_iter().map(|p| p.name).collect();
            names.sort_by_key(|n| n.to_lowercase());
            line(host, b, &mut py, &format!("{} online", names.len()), 0xFFFFFF);
            for pair in names.chunks(2) {
                line(host, b, &mut py, &pair.join("    "), DIM);
            }
        }
        Row::Emotes | Row::Learn | Row::Evolve => {
            for (i, t) in list_of(row).iter().enumerate() {
                let picked = sub == Some(i);
                if picked {
                    pill(host, b, px - 3.0, py - 2.0, pw, 11.0, PICK);
                }
                line(host, b, &mut py, t, if picked || sub.is_none() { 0xFFFFFF } else { DIM });
            }
        }
        Row::Steps => {
            let (done, goal) = guide::first_steps(&*host);
            for (i, s) in guide::STEPS.iter().enumerate() {
                let mark = if done[i] { "\u{2714}" } else { "\u{2022}" };
                line(host, b, &mut py, &format!("{mark} {}", s.title()), if done[i] { 0x9CF27A } else { 0xFFFFFF });
            }
            if let Some((progress, title, detail, way)) = goal {
                py += 4.0;
                line(host, b, &mut py, &format!("Next ({progress}): {title}"), 0xFFE9A0);
                line(host, b, &mut py, &detail, DIM);
                if let Some(way) = way {
                    line(host, b, &mut py, &way, 0x8FD3FF);
                }
            }
        }
        Row::News => {
            let mut any = false;
            if let Some(t) = event::timer() {
                line(host, b, &mut py, &t, 0xFFD24A);
                any = true;
            }
            if let Some(t) = launch_ui::goal_text() {
                line(host, b, &mut py, &t, 0xFFFFFF);
                any = true;
            }
            let (mult, median) = progress::boost();
            if median > 0 {
                let t = if mult > 1.01 { format!("Server level {median}: catching up, XP x{mult:.1}") } else { format!("Server level {median}") };
                line(host, b, &mut py, &t, 0x9CF27A);
                any = true;
            }
            let news = launch_ui::news(pw, |t| host.text_width(t));
            for (t, rgb, gap) in news {
                py += gap;
                line(host, b, &mut py, &t, rgb);
                any = true;
            }
            if !any {
                line(host, b, &mut py, "Nothing new right now.", DIM);
            }
        }
        _ => {}
    }
    let foot = if sub.is_some() {
        "[W] [S] choose  [Space] pick  [X] back"
    } else if row.info_only() {
        "[W] [S] choose  [X] close"
    } else {
        "[W] [S] choose  [Space] open  [X] close"
    };
    guide::keyed_centred(host, b, foot, x + w / 2.0, y + h - 12.0, 0xD8E4F4);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_only_show_while_on_offer() {
        let plain = rows_for(false, false);
        assert!(!plain.contains(&Row::Learn) && !plain.contains(&Row::Evolve));
        let both = rows_for(true, true);
        assert_eq!(&both[..5], &[Row::Help, Row::Map, Row::Players, Row::Learn, Row::Evolve]);
        assert_eq!(both.len(), plain.len() + 2);
    }

    #[test]
    fn everything_onii_listed_has_a_row() {
        // Onii 2026-11-12: help, map, player count, wardrobe, shop and the rest.
        let r = rows_for(false, false);
        for want in [Row::Help, Row::Map, Row::Players, Row::Wardrobe, Row::Shop, Row::Dex, Row::Emotes, Row::Chat, Row::News] {
            assert!(r.contains(&want), "{want:?}");
        }
    }

    #[test]
    fn the_emote_list_is_the_catalogue() {
        assert_eq!(list_of(Row::Emotes).len(), crate::emotes::CATALOG.emotes.len());
        assert!(list_of(Row::Players).is_empty());
    }
}
