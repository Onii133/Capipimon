//! Launch extras, the client half: titles over nameplates, the welcome-back card, the community
//! goal bar and the mute list. The server decides everything; this draws what it is told.
//!
//! Wire (channel `capipimon:play`), server -> client:
//! - `titles\t<name>\t<title>[\t<name>\t<title>...]`: who wears what, `-` for nothing.
//! - `welcome\t<heading>\t<row>...`: the card; each row is a letter, a colon and text (`a` away,
//!   `n` news, `q` quest, `e` event, `s` season, `g` goal).
//! - `community\t<count>\t<target>\t<bonus seconds left>\t<note>`, or `community\t-`.
//! - `muted\t<uuid hex>,<uuid hex>,...`: whose chat this client hides.
//!
//! Client -> server, once per connection: `feat\ttitles,welcome,goal,mute,dex` (what this client draws;
//! a server that doesn't hear it uses chat lines instead).

use super::*;

const FEATS: &str = "feat\ttitles,welcome,goal,mute,dex";
/// The card closes by itself after this long.
const CARD_SECS: f32 = 40.0;
const CARD_W: f32 = 236.0;
const BAR_W: f32 = 150.0;
/// The status panel and the first-steps tracker fill the top left this far across.
const LEFT_PANELS: f32 = 166.0;

/// Where something `w` wide goes on a `screen` wide screen: centred, moved right of the top-left
/// panels on a small screen, and never off the right edge.
fn place(screen: f32, w: f32) -> f32 {
    (screen / 2.0 - w / 2.0).max(LEFT_PANELS).min((screen - w - 4.0).max(0.0))
}

struct Goal {
    count: u64,
    target: u64,
    /// Seconds of bonus hour left when heard, and when that was.
    left: u64,
    at: Instant,
    note: String,
}

#[derive(Default)]
struct View {
    titles: HashMap<String, String>,
    card: Option<(String, Vec<String>, Instant)>,
    goal: Option<Goal>,
    muted: HashSet<u128>,
}

static VIEW: LazyLock<Mutex<View>> = LazyLock::new(Default::default);
static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first `xp` line of a connection: tell the server what this client draws.
pub fn hello(host: &mut dyn Host, f: &[&str]) {
    if f.first() == Some(&"xp") && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let _ = host.net_send_to_server(CHANNEL, FEATS.as_bytes());
    }
}

pub fn heard(f: &[&str]) -> bool {
    let mut v = VIEW.lock().unwrap();
    match f {
        ["titles", pairs @ ..] => {
            for p in pairs.chunks_exact(2) {
                if p[1] == "-" || p[1].is_empty() {
                    v.titles.remove(p[0]);
                } else {
                    v.titles.insert(p[0].to_string(), p[1].chars().take(32).collect());
                }
            }
        }
        ["welcome", head, rows @ ..] => {
            let rows = rows.iter().filter(|r| r.len() > 2 && r.as_bytes()[1] == b':').take(24).map(|r| r.to_string()).collect();
            v.card = Some((head.to_string(), rows, Instant::now()));
        }
        ["community", "-", ..] => v.goal = None,
        ["community", count, target, left, rest @ ..] => {
            v.goal = Some(Goal {
                count: count.parse().unwrap_or(0),
                target: target.parse().unwrap_or(0).max(1),
                left: left.parse().unwrap_or(0),
                at: Instant::now(),
                note: rest.first().map_or(String::new(), |n| n.to_string()),
            });
        }
        ["muted", list, ..] => v.muted = list.split(',').filter_map(|h| u128::from_str_radix(h, 16).ok()).collect(),
        ["muted"] => v.muted.clear(),
        _ => return false,
    }
    true
}

pub fn forget() {
    *VIEW.lock().unwrap() = View::default();
    SAID.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// The title `name` wears, for the nameplate.
pub fn title_of(name: &str) -> Option<String> {
    VIEW.lock().unwrap().titles.get(name).cloned()
}

/// Whether this client hides chat from `sender` (0: not a player).
pub fn is_muted(sender: u128) -> bool {
    sender != 0 && VIEW.lock().unwrap().muted.contains(&sender)
}

/// [X] closes the card; a move key closes it too, and still casts.
pub fn key(key: &str) -> bool {
    let mut v = VIEW.lock().unwrap();
    if v.card.is_none() {
        return false;
    }
    match key {
        "key.keyboard.x" => {
            v.card = None;
            true
        }
        "key.keyboard.1" | "key.keyboard.2" | "key.keyboard.3" | "key.keyboard.4" | "key.keyboard.5" | "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r" | "key.keyboard.c" => {
            v.card = None;
            false
        }
        _ => false,
    }
}

/// 41200 as "41,200".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `text` in lines of at most `width`, by `measure`.
fn wrap(text: &str, width: f32, measure: impl Fn(&str) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if measure(&candidate) > width && !line.is_empty() {
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

fn heading(kind: u8) -> Option<&'static str> {
    Some(match kind {
        b'n' => "NEW SINCE YOU LAST PLAYED",
        b'q' => "TODAY'S QUESTS",
        b'e' => "COMING UP",
        b's' => "SEASON",
        b'g' => "COMMUNITY GOAL",
        _ => return None,
    })
}

/// The card as lines to draw: (text, colour, extra space above).
fn layout(rows: &[String], width: f32, measure: impl Fn(&str) -> f32) -> Vec<(String, u32, f32)> {
    let mut out = Vec::new();
    let mut last = 0u8;
    for r in rows {
        let (kind, text) = (r.as_bytes()[0], &r[2..]);
        if kind != last {
            if let Some(h) = heading(kind) {
                out.push((h.to_string(), 0xFFD24A, 5.0));
            }
            last = kind;
        }
        let done = kind == b'q' && text.ends_with('\u{2714}');
        let rgb = match kind {
            b'a' => 0xC0C8D8,
            _ if done => 0x7FE08A,
            _ => 0xFFFFFF,
        };
        for (i, l) in wrap(text, width - 6.0, &measure).into_iter().enumerate() {
            out.push((if i == 0 && kind != b'a' { format!("\u{2022} {l}") } else if kind == b'a' { l } else { format!("  {l}") }, rgb, 0.0));
        }
    }
    out
}

fn goal_bar(host: &mut dyn Host, b: &mut HudBatch, g: &Goal) {
    let left = g.left.saturating_sub(g.at.elapsed().as_secs());
    let bonus = g.left > 0 && left > 0;
    let text = if bonus {
        format!("Double XP hour  {}:{:02}", left / 60, left % 60)
    } else if !g.note.is_empty() && g.at.elapsed().as_secs() % 8 >= 5 {
        // What reaching it gives, every few seconds in place of the count.
        g.note.clone()
    } else {
        format!("Community KOs  {} / {}", grouped(g.count), grouped(g.target))
    };
    let tw = host.text_width(&text);
    let w = BAR_W.max(tw + 12.0);
    // Under the Event Night strip when that is up.
    let y = if event::showing() { 21.0 } else { 4.0 };
    let x = place(b.width, w);
    pill(host, b, x, y, w, 17.0, HUD_PANEL);
    host.text(b, &text, [x + w / 2.0 - tw / 2.0, y + 2.5], if bonus || g.count >= g.target { 0xFFD24A } else { 0xFFFFFF }, true);
    let frac = (g.count as f32 / g.target as f32).clamp(0.0, 1.0);
    rect(host, b, x + 4.0, y + 12.5, w - 8.0, 2.0, 0xFF26_2626);
    rect(host, b, x + 4.0, y + 12.5, (w - 8.0) * frac, 2.0, if frac >= 1.0 { 0xFFFF_D24A } else { 0xFF4C_D964 });
}

pub fn hud(host: &mut dyn Host, b: &mut HudBatch) {
    let mut v = VIEW.lock().unwrap();
    if v.card.as_ref().is_some_and(|(_, _, at)| at.elapsed().as_secs_f32() > CARD_SECS) {
        v.card = None;
    }
    let card = v.card.as_ref().map(|(h, r, _)| (h.clone(), r.clone()));
    if let Some(g) = &v.goal {
        goal_bar(host, b, g);
    }
    drop(v);
    let Some((head, rows)) = card else { return };
    // A small screen gets a narrower card, beside the status panel instead of under it.
    let card_w = CARD_W.min(b.width - LEFT_PANELS - 4.0).max(120.0);
    let lines = layout(&rows, card_w - 12.0, |t| host.text_width(t));
    let body: f32 = lines.iter().map(|(_, _, gap)| 10.0 + gap).sum();
    let h = 16.0 + body + 18.0;
    let (x, y) = (place(b.width, card_w), 42.0f32.max(b.height * 0.12).min((b.height - h - 4.0).max(0.0)));
    pill(host, b, x, y, card_w, h, HUD_PANEL);
    pill(host, b, x, y, card_w, 13.0, 0xFFB8_2A1A);
    ball(host, b, x + 8.0, y + 6.5, 4.0);
    host.text(b, &fit(host, &head, card_w - 22.0), [x + 16.0, y + 3.0], 0xFFFFFF, true);
    let mut ly = y + 17.0;
    for (text, rgb, gap) in &lines {
        ly += gap;
        host.text(b, text, [x + 6.0, ly], *rgb, true);
        ly += 10.0;
    }
    guide::keyed_centred(host, b, "[X] Close", x + card_w / 2.0, y + h - 13.0, 0xC0C8D8);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share one view.
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn titles_come_and_go_by_name() {
        let _g = LOCK.lock().unwrap();
        forget();
        assert!(heard(&["titles", "Ann", "First 100", "Bo", "Bug Hunter"]));
        assert_eq!(title_of("Ann").as_deref(), Some("First 100"));
        assert!(heard(&["titles", "Ann", "-"]));
        assert_eq!((title_of("Ann"), title_of("Bo").as_deref()), (None, Some("Bug Hunter")));
        forget();
        assert_eq!(title_of("Bo"), None);
    }

    #[test]
    fn muted_players_are_hidden_and_the_server_never_is() {
        let _g = LOCK.lock().unwrap();
        forget();
        assert!(heard(&["muted", &format!("{:032x},{:032x}", 5, 9)]));
        assert!(is_muted(5) && is_muted(9) && !is_muted(6));
        assert!(!is_muted(0));
        assert!(heard(&["muted", ""]));
        assert!(!is_muted(5));
    }

    #[test]
    fn the_card_closes_on_x_and_on_a_move_key() {
        let _g = LOCK.lock().unwrap();
        forget();
        assert!(!key("key.keyboard.x"), "no card: the key is somebody else's");
        heard(&["welcome", "Welcome back, Ann!", "q:KO 5 wild Pok\u{e9}mon (0/5)", "junk"]);
        assert_eq!(VIEW.lock().unwrap().card.as_ref().unwrap().1.len(), 1, "rows without a kind are dropped");
        assert!(!key("key.keyboard.q"), "the move still casts");
        assert!(VIEW.lock().unwrap().card.is_none());
        heard(&["welcome", "Welcome back, Ann!"]);
        assert!(key("key.keyboard.x"));
    }

    #[test]
    fn the_goal_line_is_read_and_cleared() {
        let _g = LOCK.lock().unwrap();
        forget();
        assert!(heard(&["community", "41200", "50000", "0", ""]));
        let (count, target) = VIEW.lock().unwrap().goal.as_ref().map(|g| (g.count, g.target)).unwrap();
        assert_eq!((count, target), (41_200, 50_000));
        assert!(heard(&["community", "-"]));
        assert!(VIEW.lock().unwrap().goal.is_none());
        assert_eq!(grouped(41_200), "41,200");
    }

    #[test]
    fn the_card_and_the_bar_keep_clear_of_the_status_panel() {
        // A 320-wide screen (GUI scale 4 at 720p): right of the panels, inside the screen.
        assert_eq!(place(320.0, 150.0), LEFT_PANELS);
        assert!(place(320.0, 190.0) + 190.0 <= 320.0);
        // A wide screen: centred.
        assert_eq!(place(960.0, 236.0), 480.0 - 118.0);
    }

    #[test]
    fn rows_get_headings_and_long_ones_wrap() {
        let rows = vec!["a:You were away for 2 days.".to_string(), "n:one two three four".to_string(), "q:A".to_string(), "q:B \u{2714}".to_string()];
        let lines = layout(&rows, 66.0, |t| t.chars().count() as f32 * 6.0);
        let text: Vec<&str> = lines.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(text[0], "You were away for 2 days.".split(' ').take(2).collect::<Vec<_>>().join(" "));
        assert!(text.contains(&"NEW SINCE YOU LAST PLAYED") && text.contains(&"TODAY'S QUESTS"));
        assert_eq!(text.iter().filter(|t| **t == "TODAY'S QUESTS").count(), 1);
        assert_eq!(lines.last().unwrap().1, 0x7FE08A, "a finished quest is green");
    }
}
