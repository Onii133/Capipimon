//! Telling a player what they can do and what to do next: the prompt under the crosshair, the
//! first steps (a page of the Tab menu, `hub.rs`) and the help screen (Tab menu or /help).
//!
//! The server decides, because only it knows what F would do here; the client draws. Every key name
//! is written `[F]` in these strings and drawn as a key cap (`keyed`), so all prompts look alike.
//!
//! Wire (channel `capipimon:play`), each sent only when it changes:
//! - server -> client: `ctx\t<key|->\t<text>\t<sub>\t<0|1 warn>` / `ctx\t-`,
//!   `heal\t<P|R|B>\t<x>\t<z>` / `heal\t-`, `goal\t<progress>\t<title>\t<detail>\t<x|->\t<z|->` /
//!   `goal\t-`, `steps\t<one 0|1 per Step>\t<0|1 hidden>`.
//! - client -> server: `ui\t1` (this client draws all that: no action-bar copies), `ui\tz` (this
//!   client casts the Z-Move on Z, so F never does), `ui\thide` (older clients).
//!
//! `heal` lines from an older server are read and dropped: no low-HP pointer (Onii, 2026-11-12).
//!
//! An older client ignores those lines and never says `ui\t1`, so it gets the same words on the
//! action bar instead.

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/guide.rs");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Map,
    Wild,
    Heal,
    Gym,
}

pub const STEPS: [Step; 4] = [Step::Map, Step::Wild, Step::Heal, Step::Gym];

impl Step {
    fn id(self) -> &'static str {
        match self {
            Step::Map => "map",
            Step::Wild => "wild",
            Step::Heal => "heal",
            Step::Gym => "gym",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Step::Map => "Open your map",
            Step::Wild => "Beat a wild Pokémon",
            Step::Heal => "Heal up",
            Step::Gym => "Challenge your first dungeon",
        }
    }

    fn how(self) -> &'static str {
        match self {
            Step::Map => "Open [Tab], then Map. It shows healing spots and what you've found.",
            Step::Wild => "[LMB] basic attack, [Q] [E] [R] moves.",
            Step::Heal => "Hold still in a pond or on a rest spot, or [F] at a berry bush.",
            Step::Gym => "Press [F] at a dungeon's door.",
        }
    }
}

/// The nearest of `list` to `at` (x and z only): its index and distance.
pub fn nearest(list: &[[f64; 3]], at: [f64; 3]) -> Option<(usize, f64)> {
    list.iter().enumerate().map(|(i, p)| (i, (p[0] - at[0]).hypot(p[2] - at[2]))).min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Eight-point compass direction from `from` to `to` (-z is north).
pub fn compass(from: [f64; 3], to: [f64; 3]) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let deg = (to[0] - from[0]).atan2(from[2] - to[2]).to_degrees().rem_euclid(360.0);
    NAMES[(((deg + 22.5) / 45.0) as usize) % 8]
}

// ---------------------------------------------------------------- client

struct Goal {
    progress: String,
    title: String,
    detail: String,
    to: Option<[f64; 2]>,
}

#[derive(Default)]
struct View {
    ctx: Option<Ctx2>,
    goal: Option<Goal>,
    steps: [bool; 4],
    help: bool,
}

/// The F prompt: one line, no subcaption (Onii, 2026-11-13). An older server's subcaption is ignored.
struct Ctx2 {
    line: String,
}

static VIEW: LazyLock<Mutex<View>> = LazyLock::new(Default::default);
static SAID_HELLO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first `xp` line of a connection: tell the server this client draws the prompts.
pub fn hello(host: &mut dyn Host, f: &[&str]) {
    if f.first() == Some(&"xp") && !SAID_HELLO.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _ = host.net_send_to_server(CHANNEL, b"ui\t1");
        let _ = host.net_send_to_server(CHANNEL, b"ui\tz");
        // Lumi's Rift menu and the capsule opening (`riftmenu.rs`); older clients get chat instead.
        let _ = host.net_send_to_server(CHANNEL, b"ui\triftmenu");
        // Eimi's IV training menu (`riftmenu.rs`); older clients buy a random-IV ticket on F.
        let _ = host.net_send_to_server(CHANNEL, b"ui\tivmenu");
    }
}

fn number2(x: &str, z: &str) -> Option<[f64; 2]> {
    Some([x.parse().ok()?, z.parse().ok()?])
}

pub fn heard(f: &[&str]) -> bool {
    let mut v = VIEW.lock().unwrap();
    match f {
        ["ctx", "-", ..] if f.len() == 2 => v.ctx = None,
        ["ctx", key, text, ..] => {
            let line = if *key == "-" { text.to_string() } else { format!("[{key}] {text}") };
            v.ctx = Some(Ctx2 { line });
        }
        ["heal", ..] => {}
        ["goal", "-", ..] => v.goal = None,
        ["goal", progress, title, detail, x, z, ..] => {
            v.goal = Some(Goal { progress: progress.to_string(), title: title.to_string(), detail: detail.to_string(), to: number2(x, z) });
        }
        ["steps", bits, ..] => {
            for (o, c) in v.steps.iter_mut().zip(bits.chars()) {
                *o = c == '1';
            }
        }
        _ => return false,
    }
    true
}

pub fn forget() {
    *VIEW.lock().unwrap() = View::default();
    SAID_HELLO.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// The help screen's keys while it's open: X goes back (GDD 3.1: it has no key of its own; Esc is
/// only the pause menu).
pub fn key(_host: &mut dyn Host, key: &str) -> bool {
    let mut v = VIEW.lock().unwrap();
    if !v.help {
        return false;
    }
    match key {
        "key.keyboard.x" => v.help = false,
        _ => return !matches!(key, "key.keyboard.w" | "key.keyboard.a" | "key.keyboard.s" | "key.keyboard.d" | "key.keyboard.space" | "key.keyboard.left.shift" | "key.keyboard.tab"),
    }
    true
}

pub fn help_open() -> bool {
    VIEW.lock().unwrap().help
}

pub fn open_help() {
    VIEW.lock().unwrap().help = true;
    menus::opening(menus::Menu::Help);
}

pub fn close_help() {
    VIEW.lock().unwrap().help = false;
}

/// The next goal for the Tab menu: progress, title, detail, and how far and which way.
pub type NextGoal = (String, String, String, Option<String>);

/// For the Tab menu: which first steps are done, and the next goal.
pub fn first_steps(host: &dyn Host) -> ([bool; 4], Option<NextGoal>) {
    let v = VIEW.lock().unwrap();
    let way = |to: [f64; 2]| {
        let (m, _) = me(host)?;
        Some(format!("{:.0} m {}", (to[0] - m[0]).hypot(to[1] - m[1]), compass([m[0], 0.0, m[1]], [to[0], 0.0, to[1]])))
    };
    (v.steps, v.goal.as_ref().map(|g| (g.progress.clone(), g.title.clone(), g.detail.clone(), g.to.and_then(way))))
}

/// `/help` and `/controls` open the help screen. True when it was one of those.
pub fn command(command: &str) -> bool {
    if !matches!(command.trim_start_matches('/').trim(), "help" | "controls") {
        return false;
    }
    open_help();
    true
}

/// One run of a `keyed` string: plain text, or a key name drawn as a key cap.
#[derive(Debug, PartialEq)]
enum Run<'a> {
    Text(&'a str),
    Key(&'a str),
}

fn runs(text: &str) -> Vec<Run<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(len) = rest[open..].find(']') else { break };
        if open > 0 {
            out.push(Run::Text(&rest[..open]));
        }
        out.push(Run::Key(&rest[open + 1..open + len]));
        rest = &rest[open + len + 1..];
    }
    if !rest.is_empty() {
        out.push(Run::Text(rest));
    }
    out
}

const CAP_PAD: f32 = 5.0;
const CAP_FILL: u32 = 0xFFE4_E8F0;
const CAP_EDGE: u32 = 0xFF8A_94A8;
const CAP_TEXT: u32 = 0x1A20_30;
/// One line of `keyed` text, key caps included.
pub const LINE: f32 = 11.0;

/// How wide `keyed` will draw this.
pub fn keyed_width(host: &dyn Host, text: &str) -> f32 {
    runs(text).iter().map(|r| match r {
        Run::Text(t) => host.text_width(t),
        Run::Key(k) => host.text_width(k) + CAP_PAD + 1.0,
    }).sum()
}

/// Draws text where `[F]` becomes a key cap, the same everywhere in the game's own screens.
pub fn keyed(host: &mut dyn Host, b: &mut HudBatch, text: &str, at: [f32; 2], rgb: u32) {
    let mut x = at[0];
    for r in runs(text) {
        match r {
            Run::Text(t) => {
                host.text(b, t, [x, at[1]], rgb, true);
                x += host.text_width(t);
            }
            Run::Key(k) => {
                let w = host.text_width(k) + CAP_PAD;
                pill(host, b, x, at[1] - 1.0, w, 10.0, CAP_EDGE);
                pill(host, b, x + 0.5, at[1] - 1.0, w - 1.0, 9.0, CAP_FILL);
                host.text(b, k, [x + CAP_PAD / 2.0, at[1]], CAP_TEXT, false);
                x += w + 1.0;
            }
        }
    }
}

/// `keyed`, centred on `cx`.
pub fn keyed_centred(host: &mut dyn Host, b: &mut HudBatch, text: &str, cx: f32, y: f32, rgb: u32) {
    let w = keyed_width(host, text);
    keyed(host, b, text, [cx - w / 2.0, y], rgb);
}

/// Splits `text` into lines no wider than `width`, keeping key caps whole.
pub fn wrap(host: &dyn Host, text: &str, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if keyed_width(host, &candidate) > width && !line.is_empty() {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// How far to turn, clockwise in degrees, to face `to` (Minecraft yaw: 0 is +z, 90 is -x).
pub fn relative_deg(me: [f64; 2], yaw: f32, to: [f64; 2]) -> f32 {
    let bearing = (-(to[0] - me[0])).atan2(to[1] - me[1]).to_degrees() as f32;
    (bearing - yaw + 540.0).rem_euclid(360.0) - 180.0
}

/// The local player's place and facing, for pointing at things.
fn me(host: &dyn Host) -> Option<([f64; 2], f32)> {
    let cam = host.camera()?;
    let feet = local_feet().unwrap_or(cam.position);
    Some(([feet[0], feet[2]], cam.yaw))
}

/// What F (or standing here) does: bottom right beside the move bar that ends at `bar_end` (Onii's
/// mockup), raised over it when the screen is too narrow; under the crosshair with no bar (None).
pub fn hud_prompt(host: &mut dyn Host, b: &mut HudBatch, bar_end: Option<f32>) {
    let v = VIEW.lock().unwrap();
    let Some(c) = &v.ctx else { return };
    let line = c.line.clone();
    drop(v);
    let w = keyed_width(host, &line) + 14.0;
    let h = 15.0;
    let (cx, y) = match bar_end {
        None => (b.width / 2.0, b.height / 2.0 + 26.0),
        Some(end) => {
            let x = b.width - w - 4.0;
            let y = if x >= end + 6.0 { b.height - h - 3.0 } else { b.height - 26.0 - 3.0 - h - 4.0 };
            (x + w / 2.0, y + 3.0)
        }
    };
    pill(host, b, cx - w / 2.0, y - 3.0, w, h, HUD_PANEL);
    keyed_centred(host, b, &line, cx, y + 1.0, 0xFFFFFF);
}

/// GDD 3.1 Pokémon keybinds (Onii, 2026-11-13). B and M are the two exceptions to "F interacts,
/// Tab opens the menu", and this page is the only place that names them: no hint or prompt anywhere
/// else. T and Enter don't open the chat (`lib.rs`); `/` does, and so does the Tab menu.
const CONTROLS: [&str; 14] = [
    "[W] [A] [S] [D]  Move",
    "[Space]  Jump",
    "[LMB] [Q] [E] [R]  Moves 1 to 4",
    "[Z]  Z-Move",
    "[Shift]  Guard (drains PP)",
    "Double-tap a direction: dodge (25 PP)",
    "[F]  Use what's in front of you",
    "[Tab]  Menu: map, emotes, chat...",
    "[B]  Emote wheel",
    "[M]  Map",
    "Menus: [Space] [LMB]  Accept",
    "Menus: [X] [RMB]  Back",
    "[/]  Chat and commands",
    "[Esc]  Pause",
];

/// Rows between the Controls lines on the help screen.
const CONTROLS_ROW: f32 = 12.0;

const HEALING: [&str; 4] = ["Hold still in a pond", "Hold still on a rest spot", "[F] at a berry bush", "All of them wait until 5 s after a fight."];
const PLACES: [&str; 4] = ["Nest: [F] PC to switch Pokémon", "Dungeon: [F] at the door", "Raid den: [F] to call or join", "Fainted? You hatch at a Nest."];

/// The help screen; true when it's open and drew.
pub fn overlay(host: &mut dyn Host, b: &mut HudBatch) -> bool {
    let steps = {
        let v = VIEW.lock().unwrap();
        if !v.help {
            return false;
        }
        v.steps
    };
    let (sw, sh) = (b.width, b.height);
    rect(host, b, 0.0, 0.0, sw, sh, 0xC010_1820);
    let (w, h) = (396.0f32.min(sw - 8.0), 214.0f32.min(sh - 8.0));
    let (x, y) = ((sw - w) / 2.0, (sh - h) / 2.0);
    pill(host, b, x, y, w, h, HUD_PANEL);
    pill(host, b, x, y, w, 14.0, 0xFF2A_4F8C);
    host.text(b, "Capipimon: how to play", [x + 6.0, y + 3.0], 0xFFFFFF, true);
    let right = x + w / 2.0 + 2.0;
    let heading = |host: &mut dyn Host, b: &mut HudBatch, t: &str, px: f32, py: f32| host.text(b, t, [px, py], 0xFFE9A0, true);
    heading(host, b, "Controls", x + 6.0, y + 19.0);
    for (i, line) in CONTROLS.iter().enumerate() {
        keyed(host, b, line, [x + 6.0, y + 32.0 + i as f32 * CONTROLS_ROW], 0xFFFFFF);
    }
    heading(host, b, "First steps", right, y + 19.0);
    for (i, s) in STEPS.iter().enumerate() {
        let ly = y + 31.0 + i as f32 * 11.0;
        let done = steps[i];
        rect(host, b, right, ly + 1.0, 6.0, 6.0, if done { 0xFF4C_D964 } else { 0xFF3A_3D4D });
        host.text(b, s.title(), [right + 10.0, ly], if done { 0x9CF27A } else { 0xFFFFFF }, true);
    }
    heading(host, b, "Healing", right, y + 79.0);
    for (i, line) in HEALING.iter().enumerate() {
        keyed(host, b, line, [right, y + 91.0 + i as f32 * LINE], if i == 3 { 0xC0C8D8 } else { 0xFFFFFF });
    }
    heading(host, b, "Places", right, y + 139.0);
    for (i, line) in PLACES.iter().enumerate() {
        keyed(host, b, line, [right, y + 151.0 + i as f32 * LINE], 0xFFFFFF);
    }
    keyed_centred(host, b, "[X] back", x + w / 2.0, y + h - 11.0, 0xD8E4F4);
    true
}

#[cfg(test)]
mod controls_tests {
    use super::*;

    #[test]
    fn the_controls_page_lists_the_exceptions_and_fits() {
        let has = |k: &str| CONTROLS.iter().any(|l| l.starts_with(k));
        assert!(has("[B]") && has("[M]") && has("[/]") && has("[Esc]  Pause"));
        assert!(!CONTROLS.iter().any(|l| l.contains("[T]") || l.contains("[Enter]")), "T and Enter don't chat any more");
        // The help panel is 214 high: the last line ends above the footer at h - 11.
        let last = 32.0 + (CONTROLS.len() - 1) as f32 * CONTROLS_ROW + 9.0;
        assert!(last <= 214.0 - 11.0, "{last}");
    }
}
