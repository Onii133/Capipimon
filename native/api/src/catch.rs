//! Catching and parties (GDD 6): talent trainers throw capsules at weakened wilds, the target
//! struggles, a catch joins the trainer's party.
//!
//! - Trainers: `/capiadmin trainer <player> on|off` (op 2), saved. A trainer is never a Pokémon.
//!   G throws a capsule (18 b, 3 charges, one back every 20 s), X opens the order wheel.
//! - Catch (6.2): the target must be under the stage threshold (35/30/25%). The capsule freezes it
//!   in a ball for 6 s; it breaks out with 3 Space presses in the green zone (22% x HP/threshold,
//!   +2% per party member, +5% if this trainer failed on it in the last 60 s; each miss takes
//!   0.5 s off). Escape: 2 s invulnerable and +30 XP (once a minute per trainer). Caught: joins.
//! - Party (6.3/6.4): up to 6; friendly fire off; caught boost x1.5 (friendship tiers
//!   1.75/1.5/1.25/1.0); leash 48 b (pulled back to the trainer after 10 s); a fainted member
//!   comes back next to the trainer. Leaving: `/release <name>` (+50 XP, back to a Nest),
//!   `/leave` (no bonus), `/rebel` (friendship under 20 or 15+ min in, once per 10 min),
//!   contract end after 25 min (+150 XP Graduation), Jailbreak when every member has fainted
//!   within 30 s (all freed, 3 s invulnerable, the trainer can't throw for 20 s).
//! - Orders (6.3), one per 2 s, each counted for members within 30 b who aren't stunned or benched:
//!   Attack (hit it in 6 s), Move (within 4 b in 8 s), Regroup (within 8 b of her in 6 s), Guard
//!   (stay within 6 b for 6 s, or hit whoever hits them), Hold (no attacks and under 3 b for 4 s),
//!   All-out (60 s cooldown, your Ultimate lands on the target in 8 s). Followed: +3 friendship
//!   (max +30 a hunt) and +10 XP. Ignored: -2. Best Friends (80+) deal +10% to an ordered target.
//! - The Crown (6.5): the trainer with the biggest party glows. Wild Pokemon players within 30 b of
//!   a 6-member trainer deal +10% (Underdog Rally). A party reaching 6 fires the "Rabbit Hunt"
//!   alert. Jailbreaking the Crown's party pays
//!   a bounty of 100 + 50 per member (+50 per earlier Jailbreak of that trainer tonight, "Sakana's
//!   Biggest Headache"), split among whoever downed the members.
//! - Trainer kit (6.1), number keys: 1 capsule (also G), 2 Tether Rope (lasso a wild within 10 b,
//!   its HP can't drop below 1 for 10 s, snaps past 14 b, 1 per hunt), 3 order wheel (also X, then
//!   1-6), 5 Potion (+40% to the aimed member or the lowest one, 3 per hunt), 6 Evolution Charm (the
//!   aimed or first ready member evolves now, 1 per hunt), 7 release the aimed member, 8 Swat
//!   (no damage, 4 b knockback, 3.5 b, 1 s CD), 9 Signature "PIPIPIPIPI" (wilds within 8 b Slowed
//!   30% for 5 s, 45 s CD). A "hunt" is an Event Night phase, or 25 minutes outside a night.
//! - The ball (6.2) has 80 HP: other Pokemon players' moves (and wild NPCs' tackles) hit it
//!   instead of the target; broken = the target is free (escape rules) and the breaker gets +30 XP.
//!   Legendaries: 10% threshold, +10% zone, 2 hits, 2 party slots. A status costs 4% of the zone.
//!   `/autostruggle on|off`: a flat 35% escape roll instead of the minigame (resolved after 1.5 s).
//! - Healing (6.3): members within 8 b of their trainer get +1% HP a second. Each catch drops a
//!   Chicken Pizza at the trainer's feet: +30% HP to the first member of that party to walk over it.
//!
//! - Bench (6.3), on maps with a `party_bench` zone: 3 fight by default, the rest wait on the bench
//!   (4 XP a minute, kept inside it). 10 s past the leash = auto-bench. 4 = Recall/Deploy (3 s CD):
//!   aim at an active member to bench them (the longest-waiting one comes out), or aim at nothing
//!   to send the whole bench out (up to 6 active). A fainted member recovers for 20 s watching its
//!   trainer, then waits on the bench. A Jailbreak needs every active member down; it frees the
//!   bench too. Maps without a bench: all 6 fight, the leash pulls back.
//!
//! The Crown's compass marker: `crown_compass` sends each wild the bearing, the HUD draws it.
//!
//! Wire: server → client `trainer\t1|0`, `charges\t<n>\t<secs to next>`, `struggle2\t<secs>\t
//! <zone>\t<need>` / `struggle2\t-`, `struggle2n\t<hits>\t<missed 0|1>`, `balled\t<name>\t1|0` (everyone), `party\t<trainer>\t
//! <name,species,hp,max,friend;...>` / `party\t-`, `order\t<kind>\t<label>`, `kit\t<quick>\t<potions>\t
//! <charms>\t<shout secs>\t<command secs>`; client → server `throw`, `order`, `catchhit\tok|miss` (the word is ignored),
//! `order\t<attack|move|regroup|guard|hold|allout>`, `item\t<tether|potion|charm|release|swat|shout|recall>`.
//! (A bare `order` and `item\tquick` are what older clients send.)

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/catch.rs");

// about 10 degrees
const CHARGES: u32 = 3;

/// The struggle needle sweeps the bar in this many seconds. Client and server both use [`needle_at`].
const NEEDLE_SWEEP_SECS: f32 = 0.6;
/// Presses this soon after the capsule lands are ignored: the needle can't have reached a zone.
const PRESS_EARLY_MS: u64 = 200;
/// A press this soon after the last counted hit is ignored, a little under one sweep.
const PRESS_GAP_MS: u64 = 500;
/// The server hears a press one round trip after the player made it, so the check looks back
/// further than it looks ahead.
const PRESS_BACK_MS: u64 = 250;
const PRESS_AHEAD_MS: u64 = 50;

// ---------------------------------------------------------------- client

#[derive(Default)]
pub struct CatchClient {
    trainer: bool,
    charges: (u32, u32, Option<Instant>),
    struggle: Option<(Instant, f32, f32, u32, u32)>,
    missed: Option<Instant>,
    balled: HashSet<String>,
    party: Option<(String, Vec<(String, String, f32, f32, u32, bool)>)>,
    order: Option<(String, Instant)>,
    /// Tether Ropes, Potions, Charms left; shout and All-out cooldown seconds, and when heard.
    kit: Option<(u32, u32, u32, u32, u32, Instant)>,
    /// The order wheel, open since.
    wheel: Option<Instant>,
    /// The Crown's compass marker: holder, metres, degrees right of facing, when heard.
    crown: Option<(String, f32, f32, Instant)>,
}

/// The order wheel, on keys 1-6 while it's open (GDD 6.3's order).
const WHEEL: [(&str, &str); 6] = [("attack", "Attack"), ("move", "Move"), ("regroup", "Regroup"), ("guard", "Guard"), ("hold", "Hold"), ("allout", "All-out")];
const WHEEL_SECS: f32 = 5.0;

pub static CATCH_CLIENT: LazyLock<Mutex<CatchClient>> = LazyLock::new(Default::default);

pub fn heard(f: &[&str]) -> bool {
    let mut c = CATCH_CLIENT.lock().unwrap();
    match f {
        ["trainer", on, ..] => c.trainer = *on == "1",
        ["charges", n, secs, ..] => c.charges = (n.parse().unwrap_or(0), secs.parse().unwrap_or(0), Some(Instant::now())),
        ["struggle2", "-", ..] => c.struggle = None,
        ["struggle2n", hits, miss, ..] => {
            if let Some(s) = c.struggle.as_mut() {
                s.4 = hits.parse().unwrap_or(s.4);
            }
            c.missed = (*miss == "1").then(Instant::now);
        }
        ["struggle2", secs, zone, need, ..] => {
            c.struggle = Some((Instant::now(), secs.parse().unwrap_or(6.0), zone.parse().unwrap_or(0.22), need.parse().unwrap_or(3), 0));
        }
        ["balled", name, on, ..] => {
            if *on == "1" {
                c.balled.insert(name.to_string());
            } else {
                c.balled.remove(*name);
            }
        }
        ["party", "-", ..] => c.party = None,
        ["party", trainer, list, ..] => {
            let ms = list
                .split(';')
                .filter_map(|e| {
                    let v: Vec<&str> = e.split(',').collect();
                    (v.len() >= 5).then(|| (v[0].to_string(), v[1].to_string(), v[2].parse().unwrap_or(0.0), v[3].parse().unwrap_or(1.0), v[4].parse().unwrap_or(50), v.get(5) == Some(&"1")))
                })
                .collect();
            c.party = Some((trainer.to_string(), ms));
        }
        ["party", trainer] => c.party = Some((trainer.to_string(), Vec::new())),
        ["crown", "-", ..] => c.crown = None,
        ["crown", name, d, rel, ..] => c.crown = Some((name.to_string(), d.parse().unwrap_or(0.0), rel.parse().unwrap_or(0.0), Instant::now())),
        ["order", _, text, ..] => c.order = Some((text.to_string(), Instant::now())),
        ["kit", q, po, ch, sh, cm, ..] => {
            let n = |s: &str| s.parse().unwrap_or(0);
            c.kit = Some((n(q), n(po), n(ch), n(sh), n(cm), Instant::now()));
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *CATCH_CLIENT.lock().unwrap() = CatchClient::default();
}

pub fn balled(name: &str) -> bool {
    CATCH_CLIENT.lock().unwrap().balled.contains(name)
}

/// The struggle needle, 0..1, `secs` after it started: back and forth every 1.2 s.
fn needle_at(secs: f32) -> f32 {
    let t = (secs / NEEDLE_SWEEP_SECS) % 2.0;
    if t < 1.0 { t } else { 2.0 - t }
}

fn needle(at: Instant) -> f32 {
    needle_at(at.elapsed().as_secs_f32())
}

/// What the server makes of a struggle press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Press {
    Ignored,
    Hit,
    Miss,
}

/// Judges a press heard `ms` after the capsule landed. A hit needs the needle in the zone around
/// that moment; presses too early or too soon after the last hit don't count at all.
fn judge_press(ms: u64, last_hit_ms: Option<u64>, zone: f32) -> Press {
    if ms < PRESS_EARLY_MS || last_hit_ms.is_some_and(|t| ms < t + PRESS_GAP_MS) {
        return Press::Ignored;
    }
    let green = 0.5 - zone / 2.0..=0.5 + zone / 2.0;
    let from = ms.saturating_sub(PRESS_BACK_MS);
    if (from..=ms + PRESS_AHEAD_MS).step_by(10).any(|t| green.contains(&needle_at(t as f32 / 1000.0))) {
        Press::Hit
    } else {
        Press::Miss
    }
}

pub fn key(host: &mut dyn Host, key: &str) -> bool {
    let mut c = CATCH_CLIENT.lock().unwrap();
    if let Some((at, _, zone, _, hits)) = c.struggle.as_mut() {
        if key == "key.keyboard.space" {
            let n = needle(*at);
            let ok = (0.5 - *zone / 2.0..=0.5 + *zone / 2.0).contains(&n);
            if ok {
                *hits += 1;
            } else {
                c.missed = Some(Instant::now());
            }
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, if ok { b"catchhit\tok".as_slice() } else { b"catchhit\tmiss".as_slice() });
            return true;
        }
        // Frozen in the ball: move keys do nothing.
        return matches!(key, "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" | "key.keyboard.4" | "key.keyboard.5" | "key.keyboard.c" | "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r");
    }
    if !c.trainer || is_pokemon() {
        return false;
    }
    if c.wheel.is_some_and(|at| at.elapsed().as_secs_f32() >= WHEEL_SECS) {
        c.wheel = None;
    }
    if c.wheel.is_some() {
        if matches!(key, "key.keyboard.x" | "key.keyboard.escape") {
            c.wheel = None;
            return true;
        }
        let pick = key.strip_prefix("key.keyboard.").and_then(|d| d.parse::<usize>().ok()).filter(|d| (1..=WHEEL.len()).contains(d));
        if let Some(d) = pick {
            c.wheel = None;
            drop(c);
            let _ = host.net_send_to_server(CHANNEL, format!("order\t{}", WHEEL[d - 1].0).as_bytes());
            return true;
        }
    } else if matches!(key, "key.keyboard.x" | "key.keyboard.3") {
        c.wheel = Some(Instant::now());
        return true;
    }
    drop(c);
    // GDD 6.1 slots on the number keys; G stays as the quick throw.
    let msg: &[u8] = match key {
        "key.keyboard.g" | "key.keyboard.1" => b"throw",
        "key.keyboard.2" => b"item\ttether",
        "key.keyboard.4" => b"item\trecall",
        "key.keyboard.5" => b"item\tpotion",
        "key.keyboard.6" => b"item\tcharm",
        "key.keyboard.7" => b"item\trelease",
        "key.keyboard.8" => b"item\tswat",
        "key.keyboard.9" => b"item\tshout",
        _ => return false,
    };
    let _ = host.net_send_to_server(CHANNEL, msg);
    true
}

pub fn movement_input(event: &mut MovementInputEvent) {
    if CATCH_CLIENT.lock().unwrap().struggle.is_some() {
        event.forward = 0.0;
        event.strafe = 0.0;
    }
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let c = CATCH_CLIENT.lock().unwrap();
    let (cx, cy) = (b.width / 2.0, b.height / 2.0);
    if let Some((at, secs, zone, need, hits)) = c.struggle {
        let (w, h) = (180.0f32, 12.0f32);
        let (x, y) = (cx - w / 2.0, cy - 40.0);
        pill(host, b, x - 3.0, y - 16.0, w + 6.0, h + 30.0, HUD_PANEL);
        rect(host, b, x, y, w, h, 0xFF40_2020);
        rect(host, b, x + w * (0.5 - zone / 2.0), y, w * zone, h, 0xFF2F_9E5A);
        let nx = x + w * needle(at);
        rect(host, b, nx - 1.0, y - 3.0, 3.0, h + 6.0, 0xFFFF_FFFF);
        let missed = c.missed.is_some_and(|m| m.elapsed().as_secs_f32() < 0.5);
        let left = (secs - at.elapsed().as_secs_f32()).max(0.0);
        let t = if missed { "Missed!".to_string() } else { format!("STRUGGLE! Space in the green  {hits}/{need}") };
        let tw = host.text_width(&t);
        host.text(b, &t, [cx - tw / 2.0, y - 13.0], if missed { 0xFF6060 } else { 0xFFFFFF }, true);
        rect(host, b, x, y + h + 4.0, w * (left / secs.max(0.1)).clamp(0.0, 1.0), 3.0, 0xFFE3_350D);
    }
    // The Crown's compass marker (GDD 6.5), top right: a ring with a dot toward the holder.
    if let Some((name, d, rel, _)) = c.crown.as_ref().filter(|cr| cr.3.elapsed().as_secs_f32() < 2.0 && is_pokemon()) {
        let t = format!("Crown: {name} {d:.0} m");
        let tw = host.text_width(&t);
        let (r, w) = (7.0f32, tw + 34.0);
        let (x, y) = (b.width - w - 8.0, 40.0);
        pill(host, b, x, y, w, 20.0, HUD_PANEL);
        let (ox, oy) = (x + 14.0, y + 10.0);
        for i in 0..12 {
            let a = i as f32 * std::f32::consts::TAU / 12.0;
            rect(host, b, ox + r * a.sin() - 0.5, oy - r * a.cos() - 0.5, 1.0, 1.0, 0x80FF_FFFF);
        }
        let a = rel.to_radians();
        rect(host, b, ox + r * a.sin() - 1.5, oy - r * a.cos() - 1.5, 3.0, 3.0, 0xFFFF_D24A);
        host.text(b, &t, [x + 26.0, y + 6.0], 0xFFD24A, true);
    }
    if c.trainer && !is_pokemon() {
        let (n, secs, at) = c.charges;
        let left = at.map_or(0, |a| secs.saturating_sub(a.elapsed().as_secs() as u32));
        // Top centre, under the Event Night strip: the bottom is the vanilla hotbar for trainers.
        let (x, y) = (cx - 60.0, 22.0);
        pill(host, b, x, y, 120.0, 16.0, HUD_PANEL);
        for i in 0..CHARGES {
            let full = i < n;
            if full {
                ball(host, b, x + 10.0 + i as f32 * 14.0, y + 8.0, 5.0);
            } else {
                rect(host, b, x + 6.0 + i as f32 * 14.0, y + 4.0, 8.0, 8.0, 0x80FF_FFFF);
            }
        }
        let t = if n < CHARGES { format!("G throw  {left}s") } else { "G throw  X order".to_string() };
        host.text(b, &t, [x + 50.0, y + 4.0], 0xFFFFFF, true);
        // The kit row: what's left this hunt and the cooldowns (GDD 6.1 slots).
        if let Some((q, po, ch, sh, cm, at)) = c.kit {
            let gone = at.elapsed().as_secs() as u32;
            let (sh, cm) = (sh.saturating_sub(gone), cm.saturating_sub(gone));
            let cd = |s: u32| if s > 0 { format!("{s}s") } else { "ok".to_string() };
            let parts = [
                (format!("2 Rope {q}"), q > 0),
                // The order wheel itself is always ready; All-out has its own 60 s cooldown.
                (if cm > 0 { format!("3 Orders, All-out {}", cd(cm)) } else { "3 Orders".to_string() }, true),
                ("4 Recall".to_string(), true),
                (format!("5 Potion {po}"), po > 0),
                (format!("6 Charm {ch}"), ch > 0),
                ("8 Swat".to_string(), true),
                (format!("9 Shout {}", cd(sh)), sh == 0),
            ];
            let gap = 8.0;
            let total: f32 = parts.iter().map(|(s, _)| host.text_width(s)).sum::<f32>() + gap * (parts.len() as f32 - 1.0);
            let (mut px, py) = (cx - total / 2.0, y + 20.0);
            pill(host, b, px - 5.0, py - 3.0, total + 10.0, 14.0, HUD_PANEL);
            for (s, ready) in parts {
                host.text(b, &s, [px, py], if ready { 0xFFFFFF } else { 0x808080 }, true);
                px += host.text_width(&s) + gap;
            }
        }
    }
    if let Some((trainer, ms)) = &c.party {
        // Trainers: under the kit row.
        let (x, mut y) = (b.width - 112.0, if c.trainer && !is_pokemon() { 64.0f32 } else { 40.0f32 });
        pill(host, b, x - 4.0, y - 4.0, 112.0, 14.0 + ms.len() as f32 * 12.0, HUD_PANEL);
        host.text(b, &fit(host, &format!("{trainer}'s party"), 100.0), [x, y], 0xFFE9A0, true);
        y += 12.0;
        for (name, _species, hp, max, f, benched) in ms {
            host.text(b, &fit(host, name, 52.0), [x, y], if *benched { 0x909090 } else { 0xFFFFFF }, true);
            if *benched {
                host.text(b, "bench", [x + 56.0, y], 0x909090, true);
                y += 12.0;
                continue;
            }
            rect(host, b, x + 54.0, y + 2.0, 36.0, 4.0, 0xFF26_2626);
            rect(host, b, x + 54.0, y + 2.0, 36.0 * (hp / max.max(1.0)).clamp(0.0, 1.0), 4.0, 0xFF4C_D964);
            let heart = match f {
                80.. => 0xFF5C8A,
                40..=79 => 0xFFB0C0,
                20..=39 => 0xA08890,
                _ => 0x606060,
            };
            host.text(b, "\u{2665}", [x + 94.0, y], heart, true);
            y += 12.0;
        }
    }
    if c.trainer && !is_pokemon() && c.wheel.is_some_and(|at| at.elapsed().as_secs_f32() < WHEEL_SECS) {
        let allout = c.kit.map_or(0, |(_, _, _, _, cm, at)| cm.saturating_sub(at.elapsed().as_secs() as u32));
        for (i, (kind, name)) in WHEEL.iter().enumerate() {
            let turn = std::f32::consts::TAU * i as f32 / WHEEL.len() as f32 - std::f32::consts::FRAC_PI_2;
            let (x, y) = (cx + turn.cos() * 70.0, cy + turn.sin() * 44.0);
            let t = if *kind == "allout" && allout > 0 { format!("{} {name} {allout}s", i + 1) } else { format!("{} {name}", i + 1) };
            let w = host.text_width(&t);
            pill(host, b, x - w / 2.0 - 4.0, y - 6.0, w + 8.0, 14.0, HUD_PANEL);
            host.text(b, &t, [x - w / 2.0, y - 3.0], if *kind == "allout" && allout > 0 { 0x808080 } else { 0xFFFFFF }, true);
        }
    }
    if let Some((t, at)) = &c.order {
        if at.elapsed().as_secs_f32() < 4.0 {
            let tw = host.text_width(t);
            host.text(b, t, [cx - tw / 2.0, cy - 60.0], 0xFF9040, true);
        }
    }
}

