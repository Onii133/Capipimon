//! Event Night (GDD 1.4 run sheet, 8 legendaries, 5.5 attendance), hosted by NPC Pippa's
//! announcements (GDD 7: "NPC" tagged so chat can tell the talent isn't on).
//!
//! `/capiadmin event start|stop|next|status` (op 2). The night runs Doors open 10 min, Hunts of
//! 25 min with 5 min intermissions, Hunt 4 "Crown Siege" (double XP in its last 5 min), Finale
//! 10 min (every party graduates, +150 XP each). Everyone sees a timer strip.
//!
//! Legendaries (GDD 8.1/8.3): 0:22 and 0:52 at the Storm Shrine (Zapdos), 1:22 at the Tide
//! Shrine (Lugia, Hunt 3: no `!vote` yet), 1:50 both. A 60 s warning, then the shrine wakes
//! when enough players are online. The first wild Pokémon player to stand on it alone long
//! enough captures it and becomes the legendary. +20 XP every 15 s uncaught; 8 min uncaught =
//! Escaped (+800 XP); KO'd = Defeated (the KO-er gets +150); caught = joins the party as the
//! legendary until the hunt ends. Escaping, or graduating from that party, unlocks the
//! legendary for the player for good. One life: the role always ends with the player back as
//! their own species.
//!
//! Wild Hours (GDD 8.1, the `legends` PvE flag, off by default): between nights, one legendary every
//! 3 h, Zapdos and Lugia in turn, with NPC Pippa 5 min and 60 s ahead; once a UTC day per player.
//!
//! Attendance (GDD 5.5): 30 minutes online during Hunts = +200 XP once per night.
//!
//! HQ and play area (GDD 9.2/9.3, `map.json` zones by flag; a map without them skips this):
//! during a night the play area is `play_area_event_night` instead of Wild Hours'; trainers stay
//! in `hq_safe_trainers` until 2 minutes into Hunt 1 (GDD 1.4: they leave at 0:12); wild Pokémon
//! players (not party members) are kept out of HQ except the `wilds_allowed_non_combat` wings,
//! and the `trainer_exit` bridgeheads during the Crown Siege. Meadow/Hatchery (`xp_off`) give no XP.
//!
//! Wire: `event\t<label>\t<secs left>` once a second while a night runs, `event\t-` when it stops.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/event.rs");

// ---------------------------------------------------------------- client

static EVENT_CLIENT: LazyLock<Mutex<Option<(String, u32, Instant)>>> = LazyLock::new(Default::default);

pub fn heard(f: &[&str]) -> bool {
    match f {
        ["event", "-", ..] => *EVENT_CLIENT.lock().unwrap() = None,
        ["event", label, secs, ..] => *EVENT_CLIENT.lock().unwrap() = Some((label.to_string(), secs.parse().unwrap_or(0), Instant::now())),
        _ => return false,
    }
    true
}

pub fn forget() {
    *EVENT_CLIENT.lock().unwrap() = None;
}

/// Whether the Event Night strip is on screen (other top-centre HUD moves under it).
pub fn showing() -> bool {
    EVENT_CLIENT.lock().unwrap().is_some()
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let Some((label, secs, at)) = EVENT_CLIENT.lock().unwrap().clone() else { return };
    let left = secs.saturating_sub(at.elapsed().as_secs() as u32);
    let t = format!("{label}  {}:{:02}", left / 60, left % 60);
    let tw = host.text_width(&t);
    let x = b.width / 2.0 - tw / 2.0;
    pill(host, b, x - 6.0, 4.0, tw + 12.0, 14.0, HUD_PANEL);
    host.text(b, &t, [x, 7.0], if label.contains("x2") { 0xFFD24A } else { 0xFFFFFF }, true);
}

