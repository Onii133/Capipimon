//! The world map (M, a keybind exception like B for emotes; or Tab menu > Map): quadrants and weather areas, the player, the Nests, gyms and raid dens
//! they've walked near (saved per player), and what's going on now (hordes, raid sign-ups).
//!
//! Wire: client -> server `map\topen|close`. Server -> client, while open:
//! `mapdata\t<x0,z0,x1,z1>\t<kind,title,x0,z0,x1,z1;...>\t<kind,name,x,z;...>\t<kind,name,x,z;...>`
//! and `mapme\t<x>\t<z>\t<yaw>`. Only what this player has found is ever sent.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/worldmap.rs");

// ---------------------------------------------------------------- client

#[derive(Clone, Debug, PartialEq)]
struct Area {
    kind: char,
    title: String,
    b: [f32; 4],
}

#[derive(Clone, Debug, PartialEq)]
struct Pin {
    kind: char,
    name: String,
    at: [f32; 2],
}

#[derive(Clone, Debug, Default, PartialEq)]
struct View {
    bounds: [f32; 4],
    areas: Vec<Area>,
    pins: Vec<Pin>,
    events: Vec<Pin>,
    /// Healing spots: `P` pond, `R` rest spot, `B` berry grove. Always known, unlike the pins.
    heals: Vec<Pin>,
    me: Option<[f32; 3]>,
}

static VIEW: LazyLock<Mutex<Option<View>>> = LazyLock::new(Default::default);
/// The map was asked for and its data hasn't come yet, or it was closed and late data must not reopen it.
static WANTED: LazyLock<Mutex<bool>> = LazyLock::new(Default::default);

fn numbers<const N: usize>(parts: &[&str]) -> Option<[f32; N]> {
    let mut out = [0.0; N];
    if parts.len() != N {
        return None;
    }
    for (o, p) in out.iter_mut().zip(parts) {
        // NaN or inf would reach f32::clamp as a bound and panic, so a message holding one is dropped.
        *o = p.parse::<f32>().ok().filter(|v| v.is_finite())?;
    }
    Some(out)
}

fn pins(text: &str) -> Vec<Pin> {
    text.split(';')
        .filter_map(|r| {
            let f: Vec<&str> = r.split(',').collect();
            let [kind, name, x, z] = f.as_slice() else { return None };
            Some(Pin { kind: kind.chars().next()?, name: name.to_string(), at: numbers::<2>(&[x, z])? })
        })
        .collect()
}

fn decode(bounds: &str, zones: &str, places: &str, events: &str) -> Option<View> {
    let b = numbers::<4>(&bounds.split(',').collect::<Vec<_>>())?;
    let areas = zones
        .split(';')
        .filter_map(|r| {
            let f: Vec<&str> = r.split(',').collect();
            let [kind, title, rest @ ..] = f.as_slice() else { return None };
            Some(Area { kind: kind.chars().next()?, title: title.to_string(), b: numbers::<4>(rest)? })
        })
        .collect();
    Some(View { bounds: b, areas, pins: pins(places), events: pins(events), heals: Vec::new(), me: None })
}

pub fn heard(f: &[&str]) -> bool {
    match f {
        ["mapdata", bounds, zones, places, events, more @ ..] => {
            if *WANTED.lock().unwrap() {
                if let Some(mut v) = decode(bounds, zones, places, events) {
                    v.heals = more.first().map_or_else(Vec::new, |h| pins(h));
                    let mut now = VIEW.lock().unwrap();
                    v.me = now.as_ref().and_then(|o| o.me);
                    *now = Some(v);
                }
            }
            true
        }
        ["mapme", x, z, yaw, ..] => {
            if let (Some(v), Some(me)) = (VIEW.lock().unwrap().as_mut(), numbers::<3>(&[x, z, yaw])) {
                v.me = Some(me);
            }
            true
        }
        _ => false,
    }
}

pub fn forget() {
    *VIEW.lock().unwrap() = None;
    *WANTED.lock().unwrap() = false;
}

/// Whether the map is up. Not while only asked for: a server that never answers mustn't leave B, M
/// and right click blocked.
pub fn is_open() -> bool {
    VIEW.lock().unwrap().is_some()
}

/// Opens the map (M, or the Tab menu's Map row), closing any other menu (`menus`).
pub fn open(host: &mut dyn Host) {
    if ask() {
        let _ = host.net_send_to_server(CHANNEL, b"map\topen");
    }
}

/// The client side of `open`: true when the server must be asked for the map.
pub(crate) fn ask() -> bool {
    if VIEW.lock().unwrap().is_some() || *WANTED.lock().unwrap() {
        return false;
    }
    *WANTED.lock().unwrap() = true;
    menus::opening(menus::Menu::Map);
    true
}

/// Asked for, its data not here yet. Not `is_open` (keys aren't blocked), but another menu
/// opening still closes it, or the data would open it over that menu.
pub fn pending() -> bool {
    *WANTED.lock().unwrap() && VIEW.lock().unwrap().is_none()
}

/// Closes the map if it's up or on its way.
pub fn close(host: &mut dyn Host) {
    if shut() {
        let _ = host.net_send_to_server(CHANNEL, b"map\tclose");
    }
}

/// Closes the map without telling the server; true when the server needs a `map close`.
pub fn shut() -> bool {
    let was = VIEW.lock().unwrap().take().is_some();
    let wanted = std::mem::take(&mut *WANTED.lock().unwrap());
    was || wanted
}

/// M opens the map when no other screen is up (`free`). While it is up X or M closes it (Esc is
/// only the pause menu). Nothing else happens while it is up: no walking, moves or anything else (`menus`).
pub fn key(host: &mut dyn Host, key: &str, free: bool) -> bool {
    let up = VIEW.lock().unwrap().is_some();
    if !up {
        if key == "key.keyboard.m" && free {
            open(host);
            return true;
        }
        return false;
    }
    if matches!(key, "key.keyboard.x" | "key.keyboard.m") {
        close(host);
        return true;
    }
    !matches!(key, "key.keyboard.w" | "key.keyboard.a" | "key.keyboard.s" | "key.keyboard.d" | "key.keyboard.space" | "key.keyboard.left.shift" | "key.keyboard.tab")
}

fn area_colour(kind: char) -> u32 {
    match kind {
        'a' => 0xFF5E7F3F,
        'b' => 0xFF3F5A4E,
        'c' => 0xFF3A6E96,
        'd' => 0xFF7D6650,
        'H' => 0xFF8A8F99,
        'S' => 0x90F0B040,
        'W' => 0x904878C8,
        _ => 0x90D8ECFF,
    }
}

fn pin_colour(kind: char) -> u32 {
    match kind {
        'N' => 0xFF5CE07A,
        'G' => 0xFFFFA030,
        'R' => 0xFFE85050,
        'X' => 0xFFB070FF,
        'H' => 0xFFFF50E0,
        'P' => 0xFF58C8FF,
        'T' => 0xFF7CE0C0,
        'B' => 0xFFFF7FBF,
        'O' => 0xFFB6FF3C,
        _ => 0xFFFFE060,
    }
}

/// The legend's key rows: (marker, words, size). `T` is a rest spot's colour on the map (its wire
/// letter `R` is a raid's). Gyms, raid dens and what's on now are named in the legend's numbered rows.
const LEGEND: [(char, &str, f32); 4] = [
    ('P', "Pond: heal", 4.0),
    ('T', "Rest spot: heal", 4.0),
    ('B', "Berries: heal", 4.0),
    ('N', "Nest (PC)", 4.0),
];
const LEGEND_WIDTH: f32 = 104.0;
const ROW: f32 = 11.0;

/// Only environments are named on the map itself: the quadrants (`a`-`d`) and the weather areas
/// (`S`, `W`, `F`, like Frostcap Hollow). Everything else (the Arena, gyms, raid dens, what's on now)
/// is a marker with its name in the legend.
fn named_on_map(kind: char) -> bool {
    matches!(kind, 'a'..='d' | 'S' | 'W' | 'F')
}

/// A numbered marker on the map and its row in the legend.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    number: usize,
    kind: char,
    name: String,
    at: [f32; 2],
    live: bool,
}

/// The legend's numbered rows: places found (gyms, raid dens) first, then what's on now (the next
/// goal, a horde, a raid sign-up). Nests are left out: they share one key row.
fn entries(v: &View) -> Vec<Entry> {
    let places = v.pins.iter().filter(|p| p.kind != 'N').map(|p| (p, false));
    let live = v.events.iter().map(|p| (p, true));
    places
        .chain(live)
        .enumerate()
        .map(|(i, (p, live))| Entry { number: i + 1, kind: p.kind, name: p.name.clone(), at: p.at, live })
        .collect()
}

/// `t` cut down with an ellipsis so it fits `width`.
fn fit(host: &mut dyn Host, t: &str, width: f32) -> String {
    if host.text_width(t) <= width {
        return t.to_string();
    }
    let mut s: String = t.to_string();
    while !s.is_empty() && host.text_width(&format!("{s}\u{2026}")) > width {
        s.pop();
    }
    format!("{}\u{2026}", s.trim_end())
}

/// A coloured box with its number inside.
fn numbered(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, n: usize, argb: u32, glow: bool) {
    let t = n.to_string();
    let tw = host.text_width(&t);
    let w = (tw + 3.0).max(9.0);
    let h = 10.0;
    if glow {
        rect(host, b, x - w / 2.0 - 2.0, y - h / 2.0 - 2.0, w + 4.0, h + 4.0, 0xFFFFFFFF);
    }
    rect(host, b, x - w / 2.0 - 1.0, y - h / 2.0 - 1.0, w + 2.0, h + 2.0, 0xFF101010);
    rect(host, b, x - w / 2.0, y - h / 2.0, w, h, argb);
    host.text(b, &t, [x - tw / 2.0 + 0.5, y - 3.5], 0x101010, false);
}

/// The top-down picture of the event map (`content/.../textures/gui/worldmap_wide_<n>.png`, made by
/// `tools/mapgen/render_map.py`, one pixel per block), in bands north to south, and the world box
/// each covers: x0, z0, x1, z1 with the max exclusive, as `tools/mapgen/worldmap.json` says.
/// Horizon packs a content GUI texture only up to 1024x512 and silently leaves a bigger one out, so
/// the +-448 map (v2.12) is two 896x448 bands; one 896x896 picture never showed (2026-11-15).
/// `gui/worldmap` is still the +-256 middle, for clients from before v2.12.
const ART_TILES: [(&str, [f32; 4]); 2] = [
    ("capipimon:gui/worldmap_wide_0", [-448.0, -448.0, 448.0, 0.0]),
    ("capipimon:gui/worldmap_wide_1", [-448.0, 0.0, 448.0, 448.0]),
];
/// The biggest content GUI texture Horizon packs (hud.rs); the art tests hold every one to it.
#[cfg(test)]
const GUI_MAX: (u32, u32) = (1024, 512);

/// The part of the picture `art` (a world box) inside the map's world box `bounds`, as (source
/// rect in the image's pixels, world box it shows); None when they don't overlap.
fn art_crop(art: [f32; 4], bounds: [f32; 4]) -> Option<([f32; 4], [f32; 4])> {
    let [a0, b0, a1, b1] = art;
    let (x0, z0, x1, z1) = (bounds[0].max(a0), bounds[1].max(b0), bounds[2].min(a1), bounds[3].min(b1));
    (x1 > x0 && z1 > z0).then(|| ([x0 - a0, z0 - b0, x1 - x0, z1 - z0], [x0, z0, x1, z1]))
}

/// A heal pin's colour key.
fn heal_key(kind: char) -> char {
    if kind == 'R' { 'T' } else { kind }
}

fn heal_name(kind: char) -> &'static str {
    match kind {
        'P' => "Pond",
        'R' => "Rest",
        _ => "Berries",
    }
}

/// "Pond 34 m NW" for the nearest pin of `list`, if there is one.
fn nearest_line(list: &[Pin], me: [f32; 3], name: impl Fn(&Pin) -> String) -> Option<String> {
    let at = [f64::from(me[0]), 0.0, f64::from(me[1])];
    let spots: Vec<[f64; 3]> = list.iter().map(|p| [f64::from(p.at[0]), 0.0, f64::from(p.at[1])]).collect();
    let (i, d) = guide::nearest(&spots, at)?;
    Some(format!("{} {d:.0} m {}", name(&list[i]), guide::compass(at, spots[i])).trim_start().to_string())
}

/// A box on screen: x, y (top left), width, height.
type Rect = [f32; 4];

/// A map name's height on screen, shadow included.
const LABEL_H: f32 = 9.0;
/// Space kept between a name and anything else.
const LABEL_GAP: f32 = 1.0;
/// A name broken over two lines (only where one line has no room: small GUIs, 2026-11-15).
const LABEL_H2: f32 = LABEL_H * 2.0 + 1.0;

/// A name to place: its width on one line and on two (0: one word, no second line), the centre of
/// where it would like to go, and its area's box on screen, which it stays beside.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Want {
    w: f32,
    w2: f32,
    at: [f32; 2],
    near: Rect,
}

/// A name's two lines: split at the space nearest its middle. None for one word.
fn two_lines(t: &str) -> Option<(&str, &str)> {
    let mid = t.len() / 2;
    let at = t.match_indices(' ').map(|(i, _)| i).min_by_key(|i| i.abs_diff(mid))?;
    Some((&t[..at], &t[at + 1..]))
}

/// How far apart two boxes are (0 when they touch or overlap).
fn gap_between(a: Rect, b: Rect) -> f32 {
    let dx = (b[0] - (a[0] + a[2])).max(a[0] - (b[0] + b[2])).max(0.0);
    let dy = (b[1] - (a[1] + a[3])).max(a[1] - (b[1] + b[3])).max(0.0);
    dx.hypot(dy)
}

/// The last layout `hud` made, and what it was made from: the search is too slow for every frame.
type Layout = (Vec<Want>, Vec<Rect>, Rect, Vec<Rect>);
static LAYOUT: Mutex<Option<Layout>> = Mutex::new(None);

fn square(x: f32, y: f32, size: f32) -> Rect {
    [x - size / 2.0, y - size / 2.0, size, size]
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

fn overlap_area(a: Rect, b: Rect) -> f32 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    w.max(0.0) * h.max(0.0)
}

/// The names in the order they get first pick: the weather areas (small circles among the markers,
/// with few clear spots), then the quadrants, which have the open ground past their outer edge.
fn label_order(areas: &[Area]) -> Vec<&Area> {
    let mut out: Vec<&Area> = areas.iter().filter(|a| named_on_map(a.kind) && !matches!(a.kind, 'a'..='d')).collect();
    out.extend(areas.iter().filter(|a| matches!(a.kind, 'a'..='d')));
    out
}

/// Where an area's name would like to sit, for an area of `kind` at `r` on screen: a quadrant's just
/// past its outer edge (north quadrants above, south below), away from the plaza where the markers
/// crowd; a weather area's in its middle.
fn label_spot(kind: char, r: [f32; 4]) -> [f32; 2] {
    let cx = (r[0] + r[2]) / 2.0;
    let off = LABEL_GAP + LABEL_H / 2.0;
    match kind {
        'a' | 'b' => [cx, r[1] - off],
        'c' | 'd' => [cx, r[3] + off],
        _ => [cx, (r[1] + r[3]) / 2.0],
    }
}

/// Lays the names out, spots in `wants` order. Every name sits inside `area`, over its own area's x
/// span, off every `blocked` box and every other name by `LABEL_GAP`, as close to its area as can
/// be: a search over all the names together, first allowing each a row from its area, then two,
/// then three. If none of those has a layout (or the search runs long), `place_in_order` does.
fn place_labels(wants: &[Want], blocked: &[Rect], area: Rect) -> Vec<Rect> {
    let cands: Vec<Vec<(f32, Rect)>> = wants.iter().map(|w| label_spots(w, blocked, area)).collect();
    for rows in 1..=3 {
        let lim = rows as f32 * (LABEL_H + LABEL_GAP);
        let doms: Vec<(usize, Vec<Rect>)> = cands.iter().enumerate().map(|(i, c)| (i, c.iter().filter(|(_, r)| gap_between(*r, wants[i].near) <= lim).map(|(_, r)| *r).collect())).collect();
        let mut nodes = 0;
        if let Some(spots) = search(doms, Vec::new(), &mut nodes) {
            let mut out = vec![[0.0; 4]; wants.len()];
            for (i, r) in spots {
                out[i] = r;
            }
            return out;
        }
    }
    place_in_order(wants, blocked, area)
}

/// Where `want` can go clear of `blocked`, best first (closest to its area, then to its spot): a
/// grid through the map, plus the spots just past each marker's edges, which a grid can step over.
fn label_spots(want: &Want, blocked: &[Rect], area: Rect) -> Vec<(f32, Rect)> {
    let mut out: Vec<(f32, Rect)> = Vec::new();
    // One line first; two (a bit worse) where the name has a space.
    let shapes = [(want.w, LABEL_H, 0.0), (want.w2, LABEL_H2, 6.0)];
    for (w, h, worse) in shapes.into_iter().filter(|s| s.0 > 0.0) {
        let x_lo = (want.near[0] - w + 1.0).max(area[0] + 1.0);
        let x_hi = (want.near[0] + want.near[2] - 1.0).min(area[0] + area[2] - w - 1.0);
        let (y_lo, y_hi) = (area[1] + 1.0, area[1] + area[3] - h - 1.0);
        let grid = |lo: f32, hi: f32, step: f32| -> Vec<f32> {
            let mut v: Vec<f32> = (0..).map(|k| lo + k as f32 * step).take_while(|s| *s <= hi).collect();
            v.push(hi);
            v
        };
        let mut xs = grid(x_lo, x_hi, 2.0);
        let mut ys = grid(y_lo, y_hi, 1.5);
        xs.push((want.at[0] - w / 2.0).clamp(x_lo, x_hi.max(x_lo)));
        ys.push((want.at[1] - h / 2.0).clamp(y_lo, y_hi.max(y_lo)));
        for m in blocked {
            xs.extend([m[0] + m[2] + LABEL_GAP, m[0] - LABEL_GAP - w].into_iter().filter(|x| (x_lo..=x_hi).contains(x)));
            ys.extend([m[1] + m[3] + LABEL_GAP, m[1] - LABEL_GAP - h].into_iter().filter(|y| (y_lo..=y_hi).contains(y)));
        }
        let padded = |r: Rect| [r[0] - LABEL_GAP, r[1] - LABEL_GAP, r[2] + LABEL_GAP * 2.0, r[3] + LABEL_GAP * 2.0];
        for &x in &xs {
            for &y in &ys {
                let r = [x, y, w, h];
                if blocked.iter().any(|m| overlaps(padded(r), *m)) {
                    continue;
                }
                let off = (x + w / 2.0 - want.at[0]).hypot(y + h / 2.0 - want.at[1]);
                out.push((gap_between(r, want.near) + off / 4.0 + worse, r));
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Picks one spot per name, none running into another: the name with the fewest spots left goes
/// next, and each pick strikes the spots it takes from the others. None once it has tried
/// `SEARCH_NODES` picks, so a crowded map can't stall a frame.
fn search(doms: Vec<(usize, Vec<Rect>)>, chosen: Vec<(usize, Rect)>, nodes: &mut u32) -> Option<Vec<(usize, Rect)>> {
    const SEARCH_NODES: u32 = 20_000;
    let Some(k) = (0..doms.len()).min_by_key(|&k| doms[k].1.len()) else { return Some(chosen) };
    for r in &doms[k].1 {
        *nodes += 1;
        if *nodes > SEARCH_NODES {
            return None;
        }
        let p = [r[0] - LABEL_GAP, r[1] - LABEL_GAP, r[2] + LABEL_GAP * 2.0, r[3] + LABEL_GAP * 2.0];
        let rest: Vec<(usize, Vec<Rect>)> = doms.iter().enumerate().filter(|(j, _)| *j != k).map(|(_, (i, d))| (*i, d.iter().filter(|q| !overlaps(p, **q)).copied().collect())).collect();
        if rest.iter().any(|(_, d)| d.is_empty()) {
            continue;
        }
        let mut next = chosen.clone();
        next.push((doms[k].0, *r));
        if let Some(done) = search(rest, next, nodes) {
            return Some(done);
        }
        if *nodes > SEARCH_NODES {
            return None;
        }
    }
    None
}

/// One pass in order. Each name takes the clear spot (inside `area`, off the names placed before it
/// and every `blocked` box, by `LABEL_GAP`) closest to its own area, then to the spot it wants,
/// always over its area's x span. With no clear spot left it takes the least covered one.
fn place_in_order(wants: &[Want], blocked: &[Rect], area: Rect) -> Vec<Rect> {
    let mut placed: Vec<Rect> = Vec::new();
    for want in wants {
        let (w, h) = (want.w, LABEL_H);
        let x_lo = (want.near[0] - w + 1.0).max(area[0] + 1.0);
        let x_hi = (want.near[0] + want.near[2] - 1.0).min(area[0] + area[2] - w - 1.0);
        let (y_lo, y_hi) = (area[1] + 1.0, area[1] + area[3] - h - 1.0);
        // A grid through the wanted spot: 2 px across, 1.5 px down.
        let steps = |from: f32, lo: f32, hi: f32, step: f32| -> Vec<f32> {
            let from = from.clamp(lo, hi.max(lo));
            let mut v = vec![from];
            let mut k = 1.0;
            while from - k * step >= lo || from + k * step <= hi {
                v.extend([from - k * step, from + k * step].into_iter().filter(|s| (lo..=hi).contains(s)));
                k += 1.0;
            }
            v
        };
        let mut tries: Vec<(f32, Rect)> = Vec::new();
        for x in steps(want.at[0] - w / 2.0, x_lo, x_hi, 2.0) {
            for &y in &steps(want.at[1] - h / 2.0, y_lo, y_hi, 1.5) {
                let r = [x, y, w, h];
                let off = (x + w / 2.0 - want.at[0]).hypot(y + h / 2.0 - want.at[1]);
                tries.push((gap_between(r, want.near) + off / 4.0, r));
            }
        }
        tries.sort_by(|a, b| a.0.total_cmp(&b.0));
        let padded = |r: Rect| [r[0] - LABEL_GAP, r[1] - LABEL_GAP, r[2] + LABEL_GAP * 2.0, r[3] + LABEL_GAP * 2.0];
        let cost = |r: Rect| -> f32 { placed.iter().chain(blocked).map(|o| overlap_area(padded(r), *o)).sum() };
        let mut best: Option<(f32, Rect)> = None;
        for (_, r) in tries {
            let c = cost(r);
            if c == 0.0 {
                best = Some((0.0, r));
                break;
            }
            if best.is_none_or(|(bc, _)| c < bc) {
                best = Some((c, r));
            }
        }
        placed.push(best.map_or([want.at[0] - w / 2.0, want.at[1] - h / 2.0, w, h], |(_, r)| r));
    }
    placed
}

/// `place_labels`, remembered while nothing it reads changes.
fn place_labels_cached(wants: Vec<Want>, blocked: Vec<Rect>, area: Rect) -> Vec<Rect> {
    let mut last = LAYOUT.lock().unwrap();
    if let Some((w, b, a, spots)) = last.as_ref() {
        if *w == wants && *b == blocked && *a == area {
            return spots.clone();
        }
    }
    let spots = place_labels(&wants, &blocked, area);
    *last = Some((wants, blocked, area, spots.clone()));
    spots
}

/// Heal marks' and Nests' sizes on the map.
const HEAL_MARK: f32 = 3.0;
const NEST_MARK: f32 = 4.0;

/// What the names are laid out from, for the map drawn in the square `sq`: the names in order, what
/// each wants, and the boxes every marker takes (its outline included), which `hud` draws the same.
fn name_layout<'a>(v: &'a View, sq: Rect, text_width: &mut dyn FnMut(&str) -> f32) -> (Vec<&'a Area>, Vec<Want>, Vec<Rect>) {
    let [bx0, bz0, bx1, bz1] = v.bounds;
    let (sx, sz) = (sq[2] / (bx1 - bx0), sq[3] / (bz1 - bz0));
    let px = |x: f32| sq[0] + (x.clamp(bx0, bx1) - bx0) * sx;
    let pz = |z: f32| sq[1] + (z.clamp(bz0, bz1) - bz0) * sz;
    let mut blocked: Vec<Rect> = v.heals.iter().map(|p| square(px(p.at[0]), pz(p.at[1]), HEAL_MARK + 2.0)).collect();
    blocked.extend(v.pins.iter().filter(|p| p.kind == 'N').map(|p| square(px(p.at[0]), pz(p.at[1]), NEST_MARK + 2.0)));
    for e in entries(v) {
        let w = (text_width(&e.number.to_string()) + 3.0).max(9.0) + 4.0;
        blocked.push([px(e.at[0]) - w / 2.0, pz(e.at[1]) - 7.0, w, 14.0]);
    }
    let named = label_order(&v.areas);
    let wants = named
        .iter()
        .map(|a| {
            let r = [px(a.b[0]), pz(a.b[1]), px(a.b[2]), pz(a.b[3])];
            let w2 = two_lines(&a.title).map_or(0.0, |(l1, l2)| text_width(l1).max(text_width(l2)));
            Want { w: text_width(&a.title), w2, at: label_spot(a.kind, r), near: [r[0], r[1], r[2] - r[0], r[3] - r[1]] }
        })
        .collect();
    (named, wants, blocked)
}

fn mark(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, size: f32, argb: u32) {
    rect(host, b, x - size / 2.0 - 1.0, y - size / 2.0 - 1.0, size + 2.0, size + 2.0, 0xFF101010);
    rect(host, b, x - size / 2.0, y - size / 2.0, size, size, argb);
}

fn centred(host: &mut dyn Host, b: &mut HudBatch, t: &str, x: f32, y: f32, colour: u32) {
    let w = host.text_width(t);
    host.text(b, t, [x - w / 2.0, y], colour, true);
}

/// Draws the map if it's open; true when it did.
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) -> bool {
    let guard = VIEW.lock().unwrap();
    let Some(v) = guard.as_ref() else { return false };
    let [bx0, bz0, bx1, bz1] = v.bounds;
    if bx1 <= bx0 || bz1 <= bz0 {
        return false;
    }
    let (sw, sh) = (b.width, b.height);
    rect(host, b, 0.0, 0.0, sw, sh, 0xE0101820);
    let side = (sh - 40.0).min(sw - LEGEND_WIDTH * 2.0 - 16.0).max(60.0);
    let (mx, my) = ((sw - side) / 2.0, 20.0);
    let (sx, sz) = (side / (bx1 - bx0), side / (bz1 - bz0));
    let px = |x: f32| mx + (x.clamp(bx0, bx1) - bx0) * sx;
    let pz = |z: f32| my + (z.clamp(bz0, bz1) - bz0) * sz;
    centred(host, b, "World Map", sw / 2.0, 6.0, 0xFFFFFF);
    rect(host, b, mx - 2.0, my - 2.0, side + 4.0, side + 4.0, 0xFFD8E4F4);
    rect(host, b, mx, my, side, side, 0xFF2A4A68);
    // The picture of the world, when the content mod has it; the flat colours below stand in
    // for it (or for the part of the map it doesn't cover) otherwise.
    let mut art = false;
    for (tile, area) in ART_TILES {
        if let Some((src, w)) = art_crop(area, v.bounds) {
            let (x0, y0) = (px(w[0]), pz(w[1]));
            art |= host.sprite(b, tile, src, [x0, y0, px(w[2]) - x0, pz(w[3]) - y0]);
        }
    }
    for a in &v.areas {
        let (x0, y0, x1, y1) = (px(a.b[0]), pz(a.b[1]), px(a.b[2]), pz(a.b[3]));
        let colour = area_colour(a.kind);
        if matches!(a.kind, 'S' | 'W' | 'F') {
            // Weather areas are circles: drawn a row at a time. Over the art, a faint wash with a
            // solid rim, so the ground under them still shows.
            let (cx, cy, r) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (y1 - y0) / 2.0);
            let (wash, rim) = (colour & 0x00FF_FFFF | 0x3000_0000, colour | 0xFF00_0000);
            let mut y = -r;
            while y < r {
                let half = (r * r - (y + 0.5) * (y + 0.5)).max(0.0).sqrt();
                if art {
                    let inner = ((r - 1.5).powi(2) - (y + 0.5) * (y + 0.5)).max(0.0).sqrt();
                    rect(host, b, cx - inner, cy + y, inner * 2.0, 1.0, wash);
                    rect(host, b, cx - half, cy + y, (half - inner).max(1.0), 1.0, rim);
                    rect(host, b, cx + inner.min(half - 1.0), cy + y, (half - inner).max(1.0), 1.0, rim);
                } else {
                    rect(host, b, cx - half, cy + y, half * 2.0, 1.0, colour);
                }
                y += 1.0;
            }
        } else if art && a.kind != 'H' {
            // Quadrants over the picture: nothing. Their biomes reach past the zone box (to the plaza
            // and the shore), so a frame drew edges the world doesn't have; the picture shows them.
        } else if art {
            // The Arena: a thin frame, not a fill over the picture.
            let edge = 0x90FF_FFFF;
            rect(host, b, x0, y0, x1 - x0, 1.0, edge);
            rect(host, b, x0, y1 - 1.0, x1 - x0, 1.0, edge);
            rect(host, b, x0, y0 + 1.0, 1.0, y1 - y0 - 2.0, edge);
            rect(host, b, x1 - 1.0, y0 + 1.0, 1.0, y1 - y0 - 2.0, edge);
        } else {
            rect(host, b, x0, y0, x1 - x0, y1 - y0, colour);
        }
    }
    // Markers first, so the names can be laid out clear of them.
    for p in &v.heals {
        mark(host, b, px(p.at[0]), pz(p.at[1]), HEAL_MARK, pin_colour(heal_key(p.kind)));
    }
    for p in v.pins.iter().filter(|p| p.kind == 'N') {
        mark(host, b, px(p.at[0]), pz(p.at[1]), NEST_MARK, pin_colour('N'));
    }
    // Everything else is a numbered marker; its name is on the legend's row of the same number.
    let list = entries(v);
    let beat = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() / 400) % 2 == 0;
    for e in &list {
        numbered(host, b, px(e.at[0]), pz(e.at[1]), e.number, pin_colour(e.kind), e.live && beat);
    }
    // Environment names: each at its own spot if that's clear, else nudged off the other names and
    // every marker (Onii, 2026-11-15: Burrow Fields ran into Lantern Wood, Tidepool Coast into
    // Rumble Crags, and names sat on markers).
    let (named, wants, blocked) = name_layout(v, [mx, my, side, side], &mut |t| host.text_width(t));
    let spots = place_labels_cached(wants, blocked, [mx, my, side, side]);
    for (a, r) in named.iter().zip(&spots) {
        let colour = if matches!(a.kind, 'a'..='d') { 0xF0F4F8 } else { 0xFFE9A0 };
        match two_lines(&a.title).filter(|_| r[3] > LABEL_H) {
            Some((l1, l2)) => {
                let cx = r[0] + r[2] / 2.0;
                centred(host, b, l1, cx, r[1], colour);
                centred(host, b, l2, cx, r[1] + LABEL_H + 1.0, colour);
            }
            None => host.text(b, &a.title, [r[0], r[1]], colour, true),
        }
    }
    if let Some([x, z, yaw]) = v.me {
        let (x, y) = (px(x), pz(z));
        let (sin, cos) = yaw.to_radians().sin_cos();
        for i in 1..=3 {
            let d = i as f32 * 2.5;
            rect(host, b, x - sin * d - 1.0, y + cos * d - 1.0, 2.0, 2.0, 0xFFFFFFFF);
        }
        mark(host, b, x, y, 5.0, 0xFFFFFFFF);
    }
    // Right of the map: the legend. Key rows, then one numbered row per marker.
    let lx = mx + side + 8.0;
    let right_w = (sw - lx - 4.0).max(20.0);
    host.text(b, "Legend", [lx, my], 0xFFE9A0, true);
    let mut ly = my + 12.0;
    mark(host, b, lx + 4.0, ly + 3.0, 5.0, 0xFFFFFFFF);
    host.text(b, "You", [lx + 12.0, ly], 0xE0E8F0, true);
    ly += ROW;
    for (kind, name, size) in LEGEND {
        mark(host, b, lx + 4.0, ly + 3.0, size, pin_colour(kind));
        host.text(b, name, [lx + 12.0, ly], 0xE0E8F0, true);
        ly += ROW;
    }
    if v.areas.iter().any(|a| a.kind == 'H') {
        // The Arena: a frame over the art (a grey block without it), as the map draws it.
        if art {
            rect(host, b, lx, ly, 8.0, 7.0, 0x90FF_FFFF);
            rect(host, b, lx + 1.0, ly + 1.0, 6.0, 5.0, 0xFF101820);
        } else {
            rect(host, b, lx, ly, 8.0, 7.0, area_colour('H'));
        }
        host.text(b, "Arena", [lx + 12.0, ly], 0xE0E8F0, true);
        ly += ROW;
    }
    // Left of the map: the nearest of what people look for, so nobody has to measure pins.
    let left = 6.0f32.max(mx - LEGEND_WIDTH - 2.0);
    let left_w = (mx - 6.0 - left).max(20.0);
    let mut left_y = my;
    if let Some(me) = v.me {
        host.text(b, "Nearest", [left, my], 0xFFE9A0, true);
        let pick = |kinds: &[char]| v.pins.iter().filter(|p| kinds.contains(&p.kind)).cloned().collect::<Vec<Pin>>();
        let rows = [
            ("Heal", nearest_line(&v.heals, me, |p| heal_name(p.kind).to_string())),
            ("Nest", nearest_line(&pick(&['N']), me, |_| String::new())),
            ("Gym", nearest_line(&pick(&['G']), me, |_| String::new())),
            ("Raid", nearest_line(&pick(&['R']), me, |_| String::new())),
        ];
        let mut y = my + 12.0;
        for (label, line) in rows {
            host.text(b, label, [left, y], 0xC0D0E0, true);
            let text = line.unwrap_or_else(|| "not found yet".to_string());
            host.text(b, &text, [left, y + 10.0], 0xFFFFFF, true);
            y += 24.0;
        }
        left_y = y + 4.0;
    }
    // The numbered rows: under the key, then on under Nearest once the right side is full.
    let bottom = sh - 18.0;
    ly += 4.0;
    for e in &list {
        let right = ly + ROW <= bottom;
        let (x, w) = if right { (lx, right_w) } else { (left, left_w) };
        let y = if right { ly } else { left_y };
        if y + ROW > bottom {
            break;
        }
        numbered(host, b, x + 5.0, y + 4.0, e.number, pin_colour(e.kind), false);
        let name = fit(host, &e.name, w - 13.0);
        host.text(b, &name, [x + 13.0, y], 0xFFFFFF, true);
        if right {
            ly += ROW + 1.0;
        } else {
            left_y += ROW + 1.0;
        }
    }
    guide::keyed_centred(host, b, "[X] close  \u{2022}  walk near places to add them", sw / 2.0, sh - 13.0, 0xC0D0E0);
    true
}


#[cfg(test)]
mod legend_tests {
    use super::*;

    #[test]
    fn only_environments_are_named_on_the_map() {
        for kind in ['a', 'b', 'c', 'd', 'S', 'W', 'F'] {
            assert!(named_on_map(kind), "{kind} is an environment");
        }
        for kind in ['H', 'G', 'R', 'N', 'O', 'L', '?'] {
            assert!(!named_on_map(kind), "{kind} goes to the legend");
        }
    }

    #[test]
    fn every_place_and_event_gets_a_numbered_legend_row() {
        let v = decode(
            "-256,-256,256,256",
            "a,Burrow Fields,-200,-200,-41,-41;H,Arena,-40,-40,40,40;F,Frostcap Hollow,-188,-178,-132,-122",
            "N,Burrow Fields Nest,-29,-71;G,Dizzy's Gym,10,20;R,East Raid Den,200,0;G,Lia's Gym,-90,40",
            "O,Next: Lia's Gym,-90,40;H,Wooloo horde,5,6",
        )
        .unwrap();
        let rows = entries(&v);
        let names: Vec<(usize, char, &str, bool)> = rows.iter().map(|e| (e.number, e.kind, e.name.as_str(), e.live)).collect();
        assert_eq!(
            names,
            [
                (1, 'G', "Dizzy's Gym", false),
                (2, 'R', "East Raid Den", false),
                (3, 'G', "Lia's Gym", false),
                (4, 'O', "Next: Lia's Gym", true),
                (5, 'H', "Wooloo horde", true),
            ],
            "Nests share a key row; places come first, then what's on now"
        );
        assert_eq!(rows[1].at, [200.0, 0.0], "a row's marker sits where its place is");
        let on_map: Vec<&str> = v.areas.iter().filter(|a| named_on_map(a.kind)).map(|a| a.title.as_str()).collect();
        assert_eq!(on_map, ["Burrow Fields", "Frostcap Hollow"], "the Arena is in the legend, not on the map");
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn long_names_break_at_their_middle_space() {
        assert_eq!(two_lines("Frostcap Hollow"), Some(("Frostcap", "Hollow")));
        assert_eq!(two_lines("Bunny Shrine Gate Way"), Some(("Bunny Shrine", "Gate Way")));
        assert_eq!(two_lines("Arena"), None);
        // With no room on one line, a name takes two rather than sitting on a marker.
        let sq = [0.0, 0.0, 200.0, 200.0];
        let near = [90.0, 90.0, 20.0, 20.0];
        let walls: Vec<Rect> = (0..20).flat_map(|i| [[0.0, i as f32 * 10.0, 45.0, 1.0], [155.0, i as f32 * 10.0 + 5.0, 45.0, 1.0]]).collect();
        let got = place_labels(&[Want { w: 120.0, w2: 60.0, at: [100.0, 100.0], near }], &walls, sq);
        assert_eq!(got[0][3], LABEL_H2, "{got:?}");
        assert!(walls.iter().all(|m| !overlaps(got[0], *m)), "{got:?}");
    }

    #[test]
    fn a_clear_spot_is_kept_and_a_taken_one_moves() {
        let sq = [0.0, 0.0, 200.0, 200.0];
        let near = [90.0, 90.0, 20.0, 20.0];
        let one = place_labels(&[Want { w: 40.0, w2: 0.0, at: [100.0, 100.0], near }], &[], sq);
        assert_eq!(one[0], [80.0, 95.5, 40.0, LABEL_H], "nothing in the way: centred on its spot");
        let mark = square(100.0, 100.0, 10.0);
        let two = place_labels(&[Want { w: 40.0, w2: 0.0, at: [100.0, 100.0], near }, Want { w: 40.0, w2: 0.0, at: [104.0, 101.0], near }], &[mark], sq);
        assert!(!overlaps(two[0], two[1]), "{two:?}");
        assert!(two.iter().all(|r| !overlaps(*r, mark)), "{two:?}");
        assert!(two.iter().all(|r| gap_between(*r, near) < LABEL_H * 2.0), "moved away from its area: {two:?}");
    }
}

#[cfg(test)]
mod art_tests {
    use super::*;

    #[test]
    fn art_crop_follows_the_map_box() {
        let all = [-448.0, -448.0, 448.0, 448.0];
        let north = ART_TILES[0].1;
        // The whole band when the map box covers it.
        assert_eq!(art_crop(north, all), Some(([0.0, 0.0, 896.0, 448.0], north)));
        // A smaller map box shows only its own part of each band.
        assert_eq!(art_crop(ART_TILES[1].1, [-100.0, 0.0, 50.0, 300.0]), Some(([348.0, 0.0, 150.0, 300.0], [-100.0, 0.0, 50.0, 300.0])));
        assert_eq!(art_crop(north, [-100.0, 0.0, 50.0, 300.0]), None, "the north band ends at z 0");
        // A server still on the +-256 map: the middle of the picture, across both bands.
        assert_eq!(art_crop(north, [-256.0, -256.0, 256.0, 256.0]), Some(([192.0, 192.0, 512.0, 256.0], [-256.0, -256.0, 256.0, 0.0])));
        assert_eq!(art_crop(ART_TILES[1].1, [-256.0, -256.0, 256.0, 256.0]), Some(([192.0, 0.0, 512.0, 256.0], [-256.0, 0.0, 256.0, 256.0])));
        assert_eq!(art_crop(north, [500.0, -500.0, 600.0, -400.0]), None);
    }

    /// Width and height of a PNG.
    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[1..4], b"PNG");
        let at = |i: usize| u32::from_be_bytes(png[i..i + 4].try_into().unwrap());
        (at(16), at(20))
    }

    #[test]
    fn shipped_art_matches_its_box() {
        // quick.ps1 points CAPI_TEST_CONTENT_DIR at the repo's content folder.
        let Some(content) = std::env::var("CAPI_TEST_CONTENT_DIR").ok().map(std::path::PathBuf::from) else {
            println!("CAPI_TEST_CONTENT_DIR not set: art check skipped");
            return;
        };
        let dir = content.parent().unwrap().to_path_buf();
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("tools/mapgen/worldmap.json")).unwrap()).unwrap();
        let bounds = |m: &serde_json::Value| -> [f32; 4] {
            let v: Vec<f32> = m["bounds"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
            [v[0], v[1], v[2], v[3]]
        };
        let tiles = meta["tiles"].as_array().expect("worldmap.json tiles");
        assert_eq!(tiles.len(), ART_TILES.len(), "render_map.py's bands and ART_TILES differ: re-run it or update ART_TILES");
        for (m, (texture, area)) in tiles.iter().zip(ART_TILES) {
            assert_eq!(m["texture"], texture, "render_map.py's band and ART_TILES differ");
            assert_eq!(bounds(m), area, "{texture}: render_map.py's box and ART_TILES differ");
        }
        // This client's bands, and the +-256 picture older clients draw.
        let mut all: Vec<(&serde_json::Value, &str)> = tiles.iter().map(|m| (m, m["texture"].as_str().unwrap())).collect();
        all.push((&meta["legacy"], "capipimon:gui/worldmap"));
        for (m, texture) in all {
            let file = format!("assets/capipimon/textures/{}.png", texture.trim_start_matches("capipimon:"));
            let (w, h) = png_size(&std::fs::read(content.join(&file)).expect(&file));
            let b = bounds(m);
            assert_eq!((w as f32, h as f32), (b[2] - b[0], b[3] - b[1]), "{file} is one pixel per block of its box");
        }
        assert_eq!(bounds(&meta["legacy"]), [-256.0, -256.0, 256.0, 256.0], "older clients' ART_BOUNDS");
    }

    #[test]
    fn every_gui_texture_fits_what_horizon_packs() {
        // A content GUI texture past 1024x512 is left out of the HUD atlas without a word, and
        // whatever draws it falls back (the 896x896 map art did, 2026-11-15).
        let Some(content) = std::env::var("CAPI_TEST_CONTENT_DIR").ok().map(std::path::PathBuf::from) else { return };
        let mut stack = vec![content.join("assets/capipimon/textures/gui")];
        let (mut seen, mut big) = (0, Vec::new());
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "png") {
                    seen += 1;
                    let (w, h) = png_size(&std::fs::read(&p).unwrap());
                    if w > GUI_MAX.0 || h > GUI_MAX.1 {
                        big.push(format!("{} is {w}x{h}", p.display()));
                    }
                }
            }
        }
        assert!(seen >= 3, "only {seen} GUI textures found");
        assert!(big.is_empty(), "GUI textures Horizon won't pack (max {}x{}):
{}", GUI_MAX.0, GUI_MAX.1, big.join("
"));
    }
}

#[cfg(test)]
mod review_fixes {
    use super::*;

    #[test]
    fn non_finite_numbers_are_dropped() {
        assert!(numbers::<2>(&["NaN", "1"]).is_none());
        assert!(numbers::<2>(&["1", "inf"]).is_none());
        assert_eq!(numbers::<2>(&["1.5", "-2"]), Some([1.5, -2.0]));
    }
}
