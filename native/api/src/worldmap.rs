//! The M-key world map: quadrants and weather areas, the player, the Nests, gyms and raid dens
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

pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let open = VIEW.lock().unwrap().is_some();
    if key == "key.keyboard.m" || (open && key == "key.keyboard.escape") {
        let want = !open && !*WANTED.lock().unwrap();
        *WANTED.lock().unwrap() = want;
        if !want {
            *VIEW.lock().unwrap() = None;
        }
        let _ = host.net_send_to_server(CHANNEL, if want { b"map\topen".as_slice() } else { b"map\tclose".as_slice() });
        return true;
    }
    // You can still walk with it up; moves and everything else wait.
    open && !matches!(key, "key.keyboard.w" | "key.keyboard.a" | "key.keyboard.s" | "key.keyboard.d" | "key.keyboard.space" | "key.keyboard.left.shift")
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
        'H' => 0xFFFF50E0,
        'P' => 0xFF58C8FF,
        'T' => 0xFF7CE0C0,
        'B' => 0xFFFF7FBF,
        'O' => 0xFFB6FF3C,
        _ => 0xFFFFE060,
    }
}

/// (marker, words, size). `T` is a rest spot's colour on the map (its wire letter `R` is a raid's).
const LEGEND: [(char, &str, f32); 9] = [
    ('O', "Next goal", 6.0),
    ('P', "Pond: heal", 4.0),
    ('T', "Rest spot: heal", 4.0),
    ('B', "Berries: heal", 4.0),
    ('N', "Nest (PC)", 4.0),
    ('G', "Gym", 6.0),
    ('R', "Raid den", 6.0),
    ('H', "Horde", 6.0),
    ('L', "Raid sign-up", 6.0),
];
const LEGEND_WIDTH: f32 = 92.0;

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

fn mark(host: &mut dyn Host, b: &mut HudBatch, x: f32, y: f32, size: f32, argb: u32) {
    rect(host, b, x - size / 2.0 - 1.0, y - size / 2.0 - 1.0, size + 2.0, size + 2.0, 0xFF101010);
    rect(host, b, x - size / 2.0, y - size / 2.0, size, size, argb);
}

fn centred(host: &mut dyn Host, b: &mut HudBatch, t: &str, x: f32, y: f32, colour: u32) {
    let w = host.text_width(t);
    host.text(b, t, [x - w / 2.0, y], colour, true);
}

/// Draws the map if it's open; true when it did.
/// Where an area's title is centred: over its area, kept inside the map. A title wider than the map is centred on it
/// (clamping to reversed bounds would panic).
fn label_x(want: f32, half: f32, mx: f32, side: f32) -> f32 {
    let (lo, hi) = (mx + half + 2.0, mx + side - half - 2.0);
    if lo <= hi { want.clamp(lo, hi) } else { mx + side / 2.0 }
}

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
    for a in &v.areas {
        let (x0, y0, x1, y1) = (px(a.b[0]), pz(a.b[1]), px(a.b[2]), pz(a.b[3]));
        let colour = area_colour(a.kind);
        if matches!(a.kind, 'S' | 'W' | 'F') {
            // Weather areas are circles: drawn a row at a time.
            let (cx, cy, r) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (y1 - y0) / 2.0);
            let mut y = -r;
            while y < r {
                let half = (r * r - (y + 0.5) * (y + 0.5)).max(0.0).sqrt();
                rect(host, b, cx - half, cy + y, half * 2.0, 1.0, colour);
                y += 1.0;
            }
        } else {
            rect(host, b, x0, y0, x1 - x0, y1 - y0, colour);
        }
    }
    for a in &v.areas {
        let (x0, y0, x1, y1) = (px(a.b[0]), pz(a.b[1]), px(a.b[2]), pz(a.b[3]));
        let half = host.text_width(&a.title) / 2.0;
        let cx = label_x((x0 + x1) / 2.0, half, mx, side);
        // Quadrant names sit toward the middle of the map, clear of the weather circles near its edges.
        let (y, colour) = match a.kind {
            'a' | 'b' => (y1 - 11.0, 0xF0F4F8),
            'c' | 'd' => (y0 + 3.0, 0xF0F4F8),
            'H' => ((y0 + y1) / 2.0 - 4.0, 0xFFFFFF),
            _ => ((y0 + y1) / 2.0 - 4.0, 0xFFE9A0),
        };
        centred(host, b, &a.title, cx, y, colour);
    }
    for p in &v.heals {
        mark(host, b, px(p.at[0]), pz(p.at[1]), 3.0, pin_colour(heal_key(p.kind)));
    }
    // Places can sit side by side: a name that would land on another is left off (the legend and
    // the Nearest list still cover it).
    let mut labels: Vec<[f32; 3]> = Vec::new();
    for p in &v.pins {
        let size = if p.kind == 'N' { 4.0 } else { 6.0 };
        let (x, y) = (px(p.at[0]), pz(p.at[1]));
        mark(host, b, x, y, size, pin_colour(p.kind));
        if p.kind != 'N' {
            let half = host.text_width(&p.name) / 2.0;
            if !labels.iter().any(|l| (l[0] - x).abs() < l[2] + half + 2.0 && (l[1] - y).abs() < 10.0) {
                labels.push([x, y, half]);
                centred(host, b, &p.name, x, y + 5.0, 0xFFFFFF);
            }
        }
    }
    let beat = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() / 400) % 2 == 0;
    for e in &v.events {
        let (x, y) = (px(e.at[0]), pz(e.at[1]));
        mark(host, b, x, y, if beat { 9.0 } else { 7.0 }, pin_colour(e.kind));
        centred(host, b, &e.name, x, y + 7.0, 0xFFFFFF);
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
    let lx = mx + side + 8.0;
    host.text(b, "Legend", [lx, my], 0xFFE9A0, true);
    for (i, (kind, name, size)) in LEGEND.iter().enumerate() {
        let ly = my + 12.0 + i as f32 * 12.0;
        mark(host, b, lx + 4.0, ly + 3.0, *size, pin_colour(*kind));
        host.text(b, name, [lx + 12.0, ly], 0xE0E8F0, true);
    }
    let ly = my + 12.0 + LEGEND.len() as f32 * 12.0;
    mark(host, b, lx + 4.0, ly + 3.0, 5.0, 0xFFFFFFFF);
    host.text(b, "You", [lx + 12.0, ly], 0xE0E8F0, true);
    // Left of the map: the nearest of what people look for, so nobody has to measure pins.
    let left = 6.0f32.max(mx - LEGEND_WIDTH - 2.0);
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
    }
    guide::keyed_centred(host, b, "[M] or [Esc] close  \u{2022}  walk near places to add them", sw / 2.0, sh - 13.0, 0xC0D0E0);
    true
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

    #[test]
    fn a_title_wider_than_the_map_is_centred() {
        assert_eq!(label_x(10.0, 80.0, 100.0, 60.0), 130.0);
        assert_eq!(label_x(500.0, 10.0, 100.0, 200.0), 288.0);
        assert_eq!(label_x(150.0, 10.0, 100.0, 200.0), 150.0);
    }
}
