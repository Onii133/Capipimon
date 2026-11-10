//! Crews: small player groups (3 to 12) with a name, a tag, a banner, their own chat, a shared dex,
//! weekly goals and a season board. A Crew is identity only: nothing in combat, healing, catching
//! or XP rules ever looks at who is in which Crew.
//!
//! This half is what the client draws: the banner chip over a Crew member's battle box.
//!
//! Wire (channel `capipimon:play`, new; older clients ignore it):
//! - server → client: `crews\t<player>:<TAG>:<shape>:<rgb>:<rgb>,...` (everyone online who is in a
//!   Crew: the tag, the banner's shape number and its two colours as hex).

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/crew.rs");

/// The banner shapes, in wire order: every glyph is in the game's default font.
pub const SHAPES: [(&str, &str); 8] = [
    ("star", "\u{2605}"),
    ("heart", "\u{2665}"),
    ("diamond", "\u{2666}"),
    ("club", "\u{2663}"),
    ("spade", "\u{2660}"),
    ("triangle", "\u{25B2}"),
    ("sun", "\u{263C}"),
    ("note", "\u{266A}"),
];

/// A list not refreshed for this long is dropped (the server sends one every 2 s).
const STALE_SECS: f32 = 10.0;

/// What is drawn over a Crew member's battle box.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub tag: String,
    pub glyph: &'static str,
    /// The chip's fill and its text colour.
    pub rgb: u32,
    pub trim: u32,
}

impl Chip {
    pub fn label(&self) -> String {
        format!("{}{}", self.glyph, self.tag)
    }
}

static CHIPS: LazyLock<Mutex<(HashMap<String, Chip>, Option<Instant>)>> = LazyLock::new(Default::default);

/// One `player:TAG:shape:rgb:rgb` entry.
fn parse_chip(entry: &str) -> Option<(String, Chip)> {
    let f: Vec<&str> = entry.split(':').collect();
    let [name, tag, shape, rgb, trim] = f.as_slice() else { return None };
    if name.is_empty() || tag.is_empty() || tag.len() > 4 || !tag.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().map(|v| v & 0xFF_FFFF);
    let glyph = SHAPES.get(shape.parse::<usize>().ok()?)?.1;
    Some((name.to_string(), Chip { tag: tag.to_string(), glyph, rgb: hex(rgb)?, trim: hex(trim)? }))
}

/// The server's `crews` list.
pub fn heard(f: &[&str]) -> bool {
    let ["crews", list, ..] = f else { return false };
    let mut c = CHIPS.lock().unwrap();
    c.0 = list.split(',').filter_map(parse_chip).collect();
    c.1 = Some(Instant::now());
    true
}

/// The Crew chip for this player's nameplate, if they're in one.
pub fn chip_of(name: &str) -> Option<Chip> {
    let c = CHIPS.lock().unwrap();
    c.1.filter(|at| at.elapsed().as_secs_f32() < STALE_SECS)?;
    c.0.get(name).cloned()
}

#[cfg(test)]
mod chip_tests {
    use super::*;

    #[test]
    fn reads_a_chip_and_refuses_junk() {
        let (name, chip) = parse_chip("Bob:ACES:0:3c44aa:f9fffe").unwrap();
        assert_eq!(name, "Bob");
        assert_eq!(chip, Chip { tag: "ACES".into(), glyph: "\u{2605}", rgb: 0x3C44AA, trim: 0xF9FFFE });
        assert_eq!(chip.label(), "\u{2605}ACES");
        for bad in ["", "Bob:ACES", "Bob:TOOLONG:0:1:2", "Bob:A B:0:1:2", "Bob:ACES:99:1:2", "Bob:ACES:0:zz:2", ":ACES:0:1:2"] {
            assert!(parse_chip(bad).is_none(), "{bad}");
        }
    }
}
