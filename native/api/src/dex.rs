//! The Capipidex (roadmap "Season 1: collection"): one entry per playable species with four marks.
//!
//! - **Seen**: a Pokémon of that species (a player or a wild one) came close to you.
//! - **Played**: you played as it.
//! - **Evolved**: you evolved it, or evolved into it. Species with no playable evolution don't
//!   have this mark.
//! - **Mastered**: you earned a level 50's worth of XP while playing its line.
//!
//! The entries are in National Pokédex order (`data/dex.json`, from `tools/dex_numbers.py`) and
//! are paged by type and by generation. `/dex` opens the book. The server keeps the marks and
//! gives the rewards; this file is the entry list both sides share and what the client draws.
//!
//! Wire (channel `capipimon:play`), server -> client, only to a client whose `feat` line has `dex`:
//! - `dex\tclear`: forget the marks (a full list follows).
//! - `dex\tmarks\t<species>:<marks, one hex digit>:<line mastery, 0-100>,...`: entries with a mark.
//! - `dex\trewards\t<done 0/1>|<have>|<need>|<what to do>|<prize>\t...`: the reward list.
//! - `dex\topen\t<species the server has played>\t<species in the dex>`: show the book.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/dex.rs");

pub const SEEN: u8 = 1;
pub const PLAYED: u8 = 2;
pub const EVOLVED: u8 = 4;
pub const MASTERED: u8 = 8;

const NUMBERS: &str = include_str!("../data/dex.json");

pub struct Entry {
    pub species: String,
    /// National Pokédex number.
    pub number: u32,
    pub name: String,
    pub types: Vec<String>,
    /// It evolves from or into another entry, so it has the Evolved mark.
    pub evolves: bool,
}

impl Entry {
    /// The marks a finished entry has.
    pub fn full(&self) -> u8 {
        if self.evolves { SEEN | PLAYED | EVOLVED | MASTERED } else { SEEN | PLAYED | MASTERED }
    }
}

/// Every entry, in Pokédex order: the species that have a number, a kit and a model.
pub fn entries() -> &'static [Entry] {
    static ALL: LazyLock<Vec<Entry>> = LazyLock::new(|| {
        let numbers: HashMap<String, u32> = serde_json::from_str(NUMBERS).unwrap_or_default();
        let listed = |s: &str| numbers.contains_key(s) && kit(s).is_some();
        let mut evolves: HashSet<String> = HashSet::new();
        for s in numbers.keys().filter(|s| listed(s)) {
            for to in progress::evolutions(s).into_iter().filter(|to| listed(to)) {
                evolves.insert(s.clone());
                evolves.insert(to);
            }
        }
        let mut all: Vec<Entry> = numbers
            .iter()
            .filter_map(|(s, n)| {
                let k = kit(s)?;
                Some(Entry { species: s.clone(), number: *n, name: k.name.clone(), types: k.types.clone(), evolves: evolves.contains(s) })
            })
            .collect();
        all.sort_by(|a, b| (a.number, &a.species).cmp(&(b.number, &b.species)));
        all
    });
    &ALL
}

/// Where `species` is in `entries`.
pub fn index(species: &str) -> Option<usize> {
    static BY_NAME: LazyLock<HashMap<&'static str, usize>> = LazyLock::new(|| entries().iter().enumerate().map(|(i, e)| (e.species.as_str(), i)).collect());
    BY_NAME.get(species).copied()
}

/// The generations: region name and its last Pokédex number.
pub const REGIONS: [(&str, u32); 9] =
    [("Kanto", 151), ("Johto", 251), ("Hoenn", 386), ("Sinnoh", 493), ("Unova", 649), ("Kalos", 721), ("Alola", 809), ("Galar", 905), ("Paldea", 1025)];

pub const TYPES: [&str; 18] =
    ["normal", "fire", "water", "electric", "grass", "ice", "fighting", "poison", "ground", "flying", "psychic", "bug", "rock", "ghost", "dragon", "dark", "steel", "fairy"];

/// Page 0 is every entry, then one page per type, then one per generation.
pub const PAGES: usize = 1 + TYPES.len() + REGIONS.len();

/// Which generation a Pokédex number belongs to (an index into `REGIONS`).
pub fn region(number: u32) -> usize {
    REGIONS.iter().position(|(_, last)| number <= *last).unwrap_or(REGIONS.len() - 1)
}

/// "fire" as "Fire".
pub fn capital(word: &str) -> String {
    let mut c = word.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

pub fn page_name(page: usize) -> String {
    match page {
        0 => "All".to_string(),
        p if p <= TYPES.len() => capital(TYPES[p - 1]),
        p => REGIONS[(p - 1 - TYPES.len()).min(REGIONS.len() - 1)].0.to_string(),
    }
}

pub fn on_page(e: &Entry, page: usize) -> bool {
    match page {
        0 => true,
        p if p <= TYPES.len() => e.types.iter().any(|t| t == TYPES[p - 1]),
        p => region(e.number) == p - 1 - TYPES.len(),
    }
}

// ---------------------------------------------------------------- client

struct Row {
    done: bool,
    have: u32,
    need: u32,
    label: String,
    prize: String,
}

#[derive(Default)]
struct View {
    /// Species -> (marks, line mastery in percent).
    marks: HashMap<String, (u8, u8)>,
    open: bool,
    page: usize,
    sel: usize,
    rewards: Vec<Row>,
    show_rewards: bool,
    scroll: usize,
    /// How many species the whole server has played, of how many.
    server: (u32, u32),
}

static VIEW: LazyLock<Mutex<View>> = LazyLock::new(Default::default);

const COLS: usize = 7;
const ROWS: usize = 5;
const REWARD_ROWS: usize = 12;

fn row(text: &str) -> Option<Row> {
    let mut p = text.splitn(5, '|');
    let (done, have, need, label, prize) = (p.next()?, p.next()?, p.next()?, p.next()?, p.next()?);
    Some(Row { done: done == "1", have: have.parse().ok()?, need: need.parse().ok()?, label: label.chars().take(60).collect(), prize: prize.chars().take(40).collect() })
}

/// A `dex` message; true when it was one.
pub fn heard(f: &[&str]) -> bool {
    if f.first() != Some(&"dex") {
        return false;
    }
    let mut v = VIEW.lock().unwrap();
    match f {
        ["dex", "clear", ..] => v.marks.clear(),
        ["dex", "marks", list, ..] => {
            for item in list.split(',') {
                let mut p = item.split(':');
                let (Some(species), Some(bits)) = (p.next(), p.next().and_then(|b| u8::from_str_radix(b, 16).ok())) else { continue };
                // Only entries this client knows: a made-up name can't grow the map.
                if index(species).is_some() {
                    let pct = p.next().and_then(|n| n.parse::<u8>().ok()).unwrap_or(0).min(100);
                    v.marks.insert(species.to_string(), (bits & 15, pct));
                }
            }
        }
        ["dex", "rewards", rows @ ..] => v.rewards = rows.iter().take(80).filter_map(|r| row(r)).collect(),
        ["dex", "open", played, total, ..] => {
            v.server = (played.parse().unwrap_or(0), total.parse().unwrap_or(0));
            v.open = true;
        }
        _ => {}
    }
    true
}

pub fn forget() {
    *VIEW.lock().unwrap() = View::default();
}

pub fn is_open() -> bool {
    VIEW.lock().unwrap().open
}

/// Standing still while the book is open (WASD move the cursor).
pub fn movement_input(event: &mut MovementInputEvent) {
    if is_open() {
        event.forward = 0.0;
        event.strafe = 0.0;
    }
}

/// The entries on `page`, as indexes into `entries`.
fn listed(page: usize) -> Vec<usize> {
    entries().iter().enumerate().filter(|(_, e)| on_page(e, page)).map(|(i, _)| i).collect()
}

/// The first page of the next group: all, types, generations.
fn next_group(page: usize) -> usize {
    match page {
        0 => 1,
        p if p <= TYPES.len() => 1 + TYPES.len(),
        _ => 0,
    }
}

/// Where the cursor goes for a move key on a list of `n`; none for other keys.
fn moved(key: &str, sel: usize, n: usize) -> Option<usize> {
    let last = n.saturating_sub(1);
    Some(match key {
        "key.keyboard.left" | "key.keyboard.a" => sel.saturating_sub(1),
        "key.keyboard.right" | "key.keyboard.d" => (sel + 1).min(last),
        "key.keyboard.up" | "key.keyboard.w" => if sel >= COLS { sel - COLS } else { sel },
        "key.keyboard.down" | "key.keyboard.s" => if sel + COLS <= last { sel + COLS } else { sel },
        "key.keyboard.page.up" => sel.saturating_sub(COLS * ROWS),
        "key.keyboard.page.down" => (sel + COLS * ROWS).min(last),
        _ => return None,
    })
}

/// Keys while the book is open (all of them are taken, so moves don't fire).
pub fn key(key: &str) -> bool {
    let mut v = VIEW.lock().unwrap();
    if !v.open {
        return false;
    }
    match key {
        "key.keyboard.escape" | "key.keyboard.backspace" | "key.keyboard.x" => v.open = false,
        "key.keyboard.r" => {
            v.show_rewards = !v.show_rewards;
            v.scroll = 0;
        }
        _ if v.show_rewards => {
            let last = v.rewards.len().saturating_sub(REWARD_ROWS);
            match key {
                "key.keyboard.up" | "key.keyboard.w" => v.scroll = v.scroll.saturating_sub(1),
                "key.keyboard.down" | "key.keyboard.s" => v.scroll = (v.scroll + 1).min(last),
                "key.keyboard.page.up" => v.scroll = v.scroll.saturating_sub(REWARD_ROWS),
                "key.keyboard.page.down" => v.scroll = (v.scroll + REWARD_ROWS).min(last),
                _ => {}
            }
        }
        "key.keyboard.q" => (v.page, v.sel) = ((v.page + PAGES - 1) % PAGES, 0),
        "key.keyboard.e" => (v.page, v.sel) = ((v.page + 1) % PAGES, 0),
        "key.keyboard.t" => (v.page, v.sel) = (next_group(v.page), 0),
        _ => {
            let n = listed(v.page).len();
            if let Some(sel) = moved(key, v.sel, n) {
                v.sel = sel;
            }
        }
    }
    true
}

// Colours: a red book with cream pages.
const BOOK: u32 = 0xF0A8_2430;
const BOOK_HEAD: u32 = 0xFFD8_3A44;
const PAGE: u32 = 0xF0F2_E8D0;
const SLOT: u32 = 0xE6FF_FCF2;
const SLOT_EDGE: u32 = 0xFFC9_B48C;
const SLOT_SEL: u32 = 0xFFFF_D23F;
const SIDE: u32 = 0xF03A_1418;
const UNSEEN: u32 = 0xFF9A_9488;
const MARKS: [(u8, &str, u32); 4] = [(SEEN, "Seen", 0xFF3F_A9F5), (PLAYED, "Played", 0xFF4C_D964), (EVOLVED, "Evolved", 0xFFFF_8A3D), (MASTERED, "Mastered", 0xFFFF_D23F)];
const PIP_OFF: u32 = 0xFFB8_B0A0;

/// How many entries of `list` have each mark, and how many can have Evolved.
fn tally(v: &View, list: &[usize]) -> ([u32; 4], u32) {
    let all = entries();
    let mut have = [0u32; 4];
    let mut evolving = 0;
    for i in list {
        let e = &all[*i];
        evolving += e.evolves as u32;
        let bits = v.marks.get(&e.species).map_or(0, |m| m.0);
        for (n, (bit, _, _)) in MARKS.iter().enumerate() {
            have[n] += (bits & bit != 0) as u32;
        }
    }
    (have, evolving)
}

fn rewards_hud(host: &mut dyn Host, b: &mut HudBatch, v: &View, x: f32, y: f32, w: f32, h: f32) {
    rect(host, b, x, y, w, h, PAGE);
    if v.rewards.is_empty() {
        let t = "No rewards listed.";
        let tw = host.text_width(t);
        host.text(b, t, [x + (w - tw) / 2.0, y + h / 2.0 - 4.0], 0x403830, false);
    }
    for (i, r) in v.rewards.iter().skip(v.scroll).take(REWARD_ROWS).enumerate() {
        let ry = y + 3.0 + i as f32 * 11.0;
        rect(host, b, x + 3.0, ry, w - 6.0, 10.0, if r.done { 0xE6DC_F2D0 } else { SLOT });
        rect(host, b, x + 6.0, ry + 3.0, 4.0, 4.0, if r.done { 0xFF4C_D964 } else { PIP_OFF });
        let count = format!("{}/{}", r.have.min(r.need), r.need);
        let cw = host.text_width(&count);
        host.text(b, &count, [x + w - 6.0 - cw, ry + 1.0], 0x403830, false);
        let prize_w = 112.0;
        let px = x + w - 12.0 - 44.0 - prize_w;
        host.text(b, &fit(host, &r.label, px - x - 18.0), [x + 13.0, ry + 1.0], 0x201810, false);
        host.text(b, &fit(host, &r.prize, prize_w), [px, ry + 1.0], if r.done { 0x1E7A32 } else { 0x8A4A10 }, false);
    }
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let v = VIEW.lock().unwrap();
    if !v.open {
        return;
    }
    let all = entries();
    let list = listed(v.page);
    let (cw, ch) = (36.0f32, 28.0f32);
    let (gw, gh) = (COLS as f32 * cw, ROWS as f32 * ch);
    let side = 112.0f32;
    let (w, h) = (gw + side + 18.0, gh + 44.0);
    let (x, y) = ((b.width - w) / 2.0, (b.height - h) / 2.0);
    pill(host, b, x, y, w, h, BOOK);
    pill(host, b, x + 3.0, y + 3.0, w - 6.0, 14.0, BOOK_HEAD);
    ball(host, b, x + 11.0, y + 10.0, 4.5);
    let (have, evolving) = tally(&v, &list);
    let head = if v.show_rewards { "Capipidex  \u{2022}  Rewards".to_string() } else { format!("Capipidex  \u{2022}  {}  ({}/{} seen)", page_name(v.page), have[0], list.len()) };
    host.text(b, &head, [x + 19.0, y + 6.0], 0xFFFFFF, true);
    let together = format!("Server: {}/{}", v.server.0, v.server.1);
    let tw = host.text_width(&together);
    host.text(b, &together, [x + w - 6.0 - tw, y + 6.0], 0xFFE9A0, true);
    let (gx, gy) = (x + 6.0, y + 21.0);
    if v.show_rewards {
        rewards_hud(host, b, &v, gx, gy, w - 12.0, gh);
        guide::keyed_centred(host, b, "[W] [S] Scroll  [R] Entries  [X] Close", x + w / 2.0, y + h - 16.0, 0xFFE8D8);
        return;
    }
    rect(host, b, gx, gy, gw, gh, PAGE);
    let rows = list.len().div_ceil(COLS);
    let first = (v.sel / COLS).saturating_sub(ROWS / 2).min(rows.saturating_sub(ROWS));
    for i in 0..(COLS * ROWS) {
        let at = first * COLS + i;
        let Some(e) = list.get(at).map(|n| &all[*n]) else { continue };
        let (sx, sy) = (gx + (i % COLS) as f32 * cw + 2.0, gy + (i / COLS) as f32 * ch + 2.0);
        pill(host, b, sx - 1.0, sy - 1.0, cw - 2.0, ch - 2.0, if at == v.sel { SLOT_SEL } else { SLOT_EDGE });
        pill(host, b, sx, sy, cw - 4.0, ch - 4.0, SLOT);
        let bits = v.marks.get(&e.species).map_or(0, |m| m.0);
        let seen = bits & SEEN != 0;
        // A type-coloured disc once seen, grey before; the marks as pips down the left edge.
        let col = if seen { 0xFF00_0000 | e.types.first().map_or(0xA0A0A0, |t| type_color(t)) } else { UNSEEN };
        rect(host, b, sx + cw / 2.0 - 7.0, sy + 3.0, 10.0, 10.0, col);
        if let Some(t2) = e.types.get(1).filter(|_| seen) {
            rect(host, b, sx + cw / 2.0 - 2.0, sy + 3.0, 5.0, 10.0, 0xFF00_0000 | type_color(t2));
        }
        for (n, (bit, _, colour)) in MARKS.iter().enumerate() {
            if *bit == EVOLVED && !e.evolves {
                continue;
            }
            rect(host, b, sx + 2.0, sy + 3.0 + n as f32 * 3.0, 3.0, 2.0, if bits & bit != 0 { *colour } else { PIP_OFF });
        }
        let label = if seen { e.name.clone() } else { format!("No. {}", e.number) };
        let fitted = fit(host, &label, cw - 5.0);
        let lw = host.text_width(&fitted);
        host.text(b, &fitted, [sx + (cw - 4.0 - lw) / 2.0, sy + 15.0], if seen { 0x302018 } else { 0x8A8478 }, false);
    }
    // The side panel: the selected entry, then this page's totals.
    let (px, py) = (gx + gw + 6.0, gy);
    pill(host, b, px, py, side, gh, SIDE);
    if let Some(e) = list.get(v.sel).map(|n| &all[*n]) {
        let (bits, pct) = v.marks.get(&e.species).copied().unwrap_or((0, 0));
        let seen = bits & SEEN != 0;
        host.text(b, &format!("No. {}", e.number), [px + 4.0, py + 4.0], 0xE0B8B0, true);
        host.text(b, &fit(host, if seen { &e.name } else { "???" }, side - 8.0), [px + 4.0, py + 14.0], 0xFFFFFF, true);
        let mut tx = px + 4.0;
        for t in e.types.iter().filter(|_| seen) {
            let label = t.to_uppercase();
            let tw = host.text_width(&label);
            pill(host, b, tx, py + 25.0, tw + 6.0, 10.0, 0xFF00_0000 | type_color(t));
            host.text(b, &label, [tx + 3.0, py + 26.0], 0xFFFFFF, true);
            tx += tw + 9.0;
        }
        for (n, (bit, name, colour)) in MARKS.iter().enumerate() {
            let my = py + 39.0 + n as f32 * 9.0;
            let applies = *bit != EVOLVED || e.evolves;
            let got = bits & bit != 0;
            rect(host, b, px + 5.0, my + 2.0, 4.0, 4.0, if got { *colour } else { 0xFF5A_3A3A });
            let text = if applies { name.to_string() } else { "Doesn't evolve".to_string() };
            host.text(b, &text, [px + 12.0, my], if got { 0xFFFFFF } else { 0xB89890 }, true);
        }
        // Line mastery: XP earned as this line, towards a level 50's worth.
        let by = py + 77.0;
        pill(host, b, px + 4.0, by, side - 8.0, 6.0, 0xFF26_2626);
        rect(host, b, px + 5.0, by + 1.0, (side - 10.0) * (pct as f32 / 100.0).clamp(0.0, 1.0), 4.0, MARKS[3].2);
        host.text(b, &format!("Line mastery {pct}%"), [px + 4.0, by + 8.0], 0xE0B8B0, true);
    }
    host.text(b, &fit(host, &format!("{} page", page_name(v.page)), side - 8.0), [px + 4.0, py + 96.0], 0xFFE9A0, true);
    for (n, (bit, name, _)) in MARKS.iter().enumerate() {
        let of = if *bit == EVOLVED { evolving } else { list.len() as u32 };
        host.text(b, &format!("{name} {}/{of}", have[n]), [px + 4.0, py + 106.0 + n as f32 * 9.0], 0xFFFFFF, true);
    }
    guide::keyed_centred(host, b, "[Q] [E] Page  [T] Types / Regions  [WASD] Move  [R] Rewards  [X] Close", x + w / 2.0, y + h - 16.0, 0xFFE8D8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_playable_species_has_one_entry_in_pokedex_order() {
        let all = entries();
        assert_eq!(all.len(), 989);
        assert!(all.windows(2).all(|w| w[0].number < w[1].number), "numbers rise and never repeat");
        assert_eq!(all[0].species, "bulbasaur");
        assert_eq!(index("pikachu").map(|i| all[i].number), Some(25));
        assert_eq!(index("missingno"), None);
    }

    #[test]
    fn pages_cover_the_dex() {
        let all = entries();
        for page in 0..PAGES {
            assert!(all.iter().any(|e| on_page(e, page)), "page {} is empty", page_name(page));
        }
        // Every entry is on exactly one generation page and on a page for each of its types.
        let gens = 1 + TYPES.len()..PAGES;
        for e in all {
            assert_eq!(gens.clone().filter(|p| on_page(e, *p)).count(), 1, "{}", e.species);
            assert_eq!((1..=TYPES.len()).filter(|p| on_page(e, *p)).count(), e.types.len(), "{}", e.species);
        }
        assert_eq!(page_name(2), "Fire");
        assert_eq!(page_name(1 + TYPES.len()), "Kanto");
        assert_eq!(page_name(PAGES - 1), "Paldea");
    }

    #[test]
    fn generations_split_at_the_right_numbers() {
        assert_eq!((region(1), region(151), region(152), region(251), region(252)), (0, 0, 1, 1, 2));
        assert_eq!((region(905), region(906), region(1025)), (7, 8, 8));
    }

    #[test]
    fn only_species_in_a_family_have_the_evolved_mark() {
        let all = entries();
        let of = |s: &str| &all[index(s).unwrap()];
        assert!(of("charmander").evolves && of("charmeleon").evolves && of("charizard").evolves);
        assert!(!of("absol").evolves);
        assert_eq!(of("absol").full(), SEEN | PLAYED | MASTERED);
        assert_eq!(of("eevee").full(), 15);
    }

    #[test]
    fn the_cursor_stays_on_the_list() {
        assert_eq!(moved("key.keyboard.a", 0, 20), Some(0));
        assert_eq!(moved("key.keyboard.d", 19, 20), Some(19));
        assert_eq!(moved("key.keyboard.s", 14, 20), Some(14), "no entry below: stay");
        assert_eq!(moved("key.keyboard.s", 3, 20), Some(10));
        assert_eq!(moved("key.keyboard.w", 3, 20), Some(3));
        assert_eq!(moved("key.keyboard.d", 0, 0), Some(0));
        assert_eq!(moved("key.keyboard.z", 0, 20), None);
        assert_eq!((next_group(0), next_group(3), next_group(20)), (1, 19, 0));
    }

    #[test]
    fn messages_fill_the_book() {
        forget();
        assert!(!heard(&["xp", "1"]));
        assert!(heard(&["dex", "marks", "pikachu:3:40,charizard:f:100,notapokemon:1:0,bad"]));
        assert!(heard(&["dex", "rewards", "1|50|50|See 50 Pokémon|Title: Sightseer", "0|3|10|Play as 10 Pokémon|Title: Shapeshifter", "junk"]));
        assert!(!is_open());
        assert!(heard(&["dex", "open", "12", "989"]));
        {
            let v = VIEW.lock().unwrap();
            assert_eq!(v.marks.get("pikachu"), Some(&(3, 40)));
            assert_eq!(v.marks.get("charizard"), Some(&(15, 100)));
            assert_eq!(v.marks.len(), 2);
            assert_eq!(v.rewards.len(), 2);
            assert!(v.rewards[0].done && v.rewards[1].prize == "Title: Shapeshifter");
            assert_eq!(v.server, (12, 989));
        }
        assert!(is_open() && key("key.keyboard.x") && !is_open());
        assert!(!key("key.keyboard.x"), "a closed book takes no keys");
        forget();
    }
}
