//! Weather areas: `map.json` zones flagged `weather_area` with `"weather": "sun" | "rain" | "snow"`,
//! `"shape": "circle"`, `"centre"` and `"radius"`. The zone's min/max is only its bounding box.
//!
//! Inside one, Fire and Water moves change strength (the main games' numbers: sun Fire x1.5 and
//! Water x0.5, rain the reverse) and Ice moves hit x1.3 in snow (no GDD number; a default). The
//! strength depends on where the attacker stands.
//!
//! Wire: server → client `weather\t<sun|rain|snow|->`; the client tints the screen. Particles are
//! server commands, forced for that player only.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/weather.rs");

// The server half of the public file of the same name, which includes it: private, never published
// (docs/CLIENT_SERVER_SPLIT.md). Its items are that file's own, so they see everything it has.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sun,
    Rain,
    Snow,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Sun => "sun",
            Kind::Rain => "rain",
            Kind::Snow => "snow",
        }
    }

    /// Its letter on the world map.
    pub fn letter(self) -> char {
        match self {
            Kind::Sun => 'S',
            Kind::Rain => 'W',
            Kind::Snow => 'F',
        }
    }

    fn effect(self) -> &'static str {
        match self {
            Kind::Sun => "Harsh sun: Fire moves up, Water moves down",
            Kind::Rain => "Rain: Water moves up, Fire moves down",
            Kind::Snow => "Snow: Ice moves up",
        }
    }
}

// ---------------------------------------------------------------- client

static TINT: LazyLock<Mutex<Option<Kind>>> = LazyLock::new(Default::default);

pub fn heard(f: &[&str]) -> bool {
    let ["weather", word, ..] = f else { return false };
    *TINT.lock().unwrap() = match *word {
        "sun" => Some(Kind::Sun),
        "rain" => Some(Kind::Rain),
        "snow" => Some(Kind::Snow),
        _ => None,
    };
    true
}

pub fn forget() {
    *TINT.lock().unwrap() = None;
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let argb = match *TINT.lock().unwrap() {
        Some(Kind::Sun) => 0x1CFF_B040,
        Some(Kind::Rain) => 0x2A30_4A70,
        Some(Kind::Snow) => 0x20D0_E8FF,
        None => return,
    };
    let (w, h) = (b.width, b.height);
    rect(host, b, 0.0, 0.0, w, h, argb);
}

