//! Launch extras, the client half: titles over nameplates, the welcome-back news, the community
//! goal and the mute list. The server decides everything; this keeps what it is told. The news and
//! the goal are a page of the Tab menu (`hub.rs`), never on the game screen (Onii, 2026-11-12).
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
    card: Option<(String, Vec<String>)>,
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
            v.card = Some((head.to_string(), rows));
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

/// The community goal as one line, for the Tab menu's News page.
pub fn goal_text() -> Option<String> {
    let v = VIEW.lock().unwrap();
    let g = v.goal.as_ref()?;
    let left = g.left.saturating_sub(g.at.elapsed().as_secs());
    let mut t = format!("Community KOs  {} / {}", grouped(g.count), grouped(g.target));
    if g.left > 0 && left > 0 {
        t = format!("Double XP hour  {}:{:02}  ({t})", left / 60, left % 60);
    } else if !g.note.is_empty() {
        t = format!("{t}: {}", g.note);
    }
    Some(t)
}

/// The welcome-back card as lines for the News page: (text, colour, extra space above).
pub fn news(width: f32, measure: impl Fn(&str) -> f32) -> Vec<(String, u32, f32)> {
    let v = VIEW.lock().unwrap();
    let Some((head, rows)) = &v.card else { return Vec::new() };
    // The live goal line already sits above the card, so the card's copy of it would say it twice.
    let rows: Vec<String> = rows.iter().filter(|r| v.goal.is_none() || !r.starts_with("g:")).cloned().collect();
    let mut out = vec![(head.clone(), 0xFFE9A0, 0.0)];
    out.extend(layout(&rows, width, measure));
    out
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
    fn the_card_waits_in_the_news_page_and_takes_no_keys() {
        let _g = LOCK.lock().unwrap();
        forget();
        assert!(news(200.0, |t| t.len() as f32).is_empty());
        heard(&["welcome", "Welcome back, Ann!", "q:KO 5 wild Pok\u{e9}mon (0/5)", "junk"]);
        let lines = news(200.0, |t| t.len() as f32);
        assert_eq!(lines[0].0, "Welcome back, Ann!");
        assert!(lines.iter().any(|l| l.0 == "TODAY'S QUESTS"));
        assert_eq!(lines.iter().filter(|l| l.0.starts_with('\u{2022}')).count(), 1, "rows without a kind are dropped");
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
        assert!(heard(&["community", "41200", "50000", "0", "Double XP for an hour"]));
        assert_eq!(goal_text().as_deref(), Some("Community KOs  41,200 / 50,000: Double XP for an hour"));
        // The welcome card's own goal row is dropped while the live line shows, and kept without it.
        assert!(heard(&["welcome", "Welcome back!", "g:Community KOs this week: 41,200 / 50,000", "q:A"]));
        let has_goal = |l: &[(String, u32, f32)]| l.iter().any(|x| x.0.contains("Community KOs") || x.0 == "COMMUNITY GOAL");
        assert!(!has_goal(&news(400.0, |t| t.len() as f32)));
        assert!(heard(&["community", "-"]));
        assert!(has_goal(&news(400.0, |t| t.len() as f32)));
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
