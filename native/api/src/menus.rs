//! One menu at a time, nothing for the Pokémon while one is up, and held W/S scrolling lists
//! (play-test 2026-11-14 #33, #8, #37). Client only.
//!
//! Every Capipimon screen says so here when it opens (`opening`), and whatever else is up closes
//! first. A screen the server keeps open for refreshes (PC, Tutor, wardrobe, map) is closed the way
//! its own X closes it, with the same close line to the server. Lines that open a screen come
//! without a `Host`, so those closes wait in an outbox that `flush` sends (each frame, and after
//! every server line). What is open is read from each screen's own state, so nothing here can
//! drift from what is drawn.
//!
//! While any of them is up (`any_open`): no moves, Z-Move, dodge, guard, F, clicks, walking or
//! jumping (`onboard::key`, `play::key`, `movement_input`, `lib.rs` `interaction_key`). The open
//! screen's own keys still work.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    /// The Tab menu (`hub`).
    Tab,
    /// Help & controls (`guide`).
    Help,
    /// The M map (`worldmap`).
    Map,
    Wardrobe,
    /// The Capipidex book (`dex`).
    Dex,
    /// The Nest PC, its box and its Moves list with the move info.
    Pc,
    /// The Move Tutor, with the move info.
    Tutor,
    /// The emote wheel (B).
    Emotes,
    /// A trainer's order wheel (X or 3, `catch`).
    Orders,
    /// Photo mode (`/photo`, or the Tab menu's row).
    Photo,
    /// Lumi's Rift menu (`riftmenu`).
    Rift,
    /// A Rift Capsule opening (`riftmenu`): no menu, but it holds the Pokémon still and takes the keys.
    Capsule,
}

pub const ALL: [Menu; 12] = [Menu::Tab, Menu::Help, Menu::Map, Menu::Wardrobe, Menu::Dex, Menu::Pc, Menu::Tutor, Menu::Emotes, Menu::Orders, Menu::Photo, Menu::Rift, Menu::Capsule];
const N: usize = ALL.len();

impl Menu {
    pub fn is_up(self) -> bool {
        match self {
            Menu::Tab => hub::is_open(),
            Menu::Help => guide::help_open(),
            Menu::Map => worldmap::is_open(),
            Menu::Wardrobe => cosmetics::draw::is_open(),
            Menu::Dex => dex::is_open(),
            Menu::Pc => pc::is_open(),
            Menu::Tutor => tutor::is_open(),
            Menu::Emotes => crate::emotes::wheel_open(),
            Menu::Orders => catch::wheel_open(),
            Menu::Photo => photo::on(),
            Menu::Rift => riftmenu::is_open(),
            Menu::Capsule => riftmenu::revealing(),
        }
    }

    /// Up, or asked for and on its way (the map waits for its data): another opening closes it.
    fn up_or_coming(self) -> bool {
        self.is_up() || (self == Menu::Map && worldmap::pending())
    }

    /// Screens with a list W/S move through: a held key repeats there.
    pub fn is_list(self) -> bool {
        matches!(self, Menu::Tab | Menu::Wardrobe | Menu::Dex | Menu::Pc | Menu::Tutor | Menu::Rift)
    }

    fn index(self) -> usize {
        ALL.iter().position(|m| *m == self).unwrap_or(0)
    }

    /// Closes it as its own X would, the server's close line included.
    fn shut(self) {
        let line: Option<&'static str> = match self {
            Menu::Tab => {
                hub::close();
                None
            }
            Menu::Help => {
                guide::close_help();
                None
            }
            Menu::Map => worldmap::shut().then_some("map\tclose"),
            Menu::Wardrobe => cosmetics::draw::shut().then_some("ward\tclose"),
            Menu::Dex => {
                dex::shut();
                None
            }
            Menu::Pc => pc::shut().then_some("pc\tclose"),
            Menu::Tutor => tutor::shut().then_some("tutor\tclose"),
            Menu::Emotes => {
                crate::emotes::close_wheel();
                None
            }
            Menu::Orders => {
                catch::close_wheel();
                None
            }
            Menu::Photo => {
                photo::stop();
                None
            }
            Menu::Rift => riftmenu::shut().then_some("riftm	close"),
            Menu::Capsule => {
                riftmenu::stop_reveal();
                None
            }
        };
        if let Some(l) = line {
            OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).push(l);
        }
    }
}

/// Close lines for the server, sent by `flush`.
static OUTBOX: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// When each menu was last closed by another opening. The server may already have a refresh of
/// it on the way (the wardrobe sends one after every change); that line mustn't bring it back.
static DISPLACED: Mutex<[Option<Instant>; N]> = Mutex::new([None; N]);
const DISPLACED_SECS: f32 = 0.5;

/// The menus up now, in `ALL` order. Normally none or one.
pub fn open_now() -> Option<Menu> {
    ALL.into_iter().find(|m| m.is_up())
}

/// Whether any Capipimon menu is up: the Pokémon gets no gameplay input.
pub fn any_open() -> bool {
    open_now().is_some()
}

/// `menu` is opening: everything else closes first. Call it with no menu's own lock held.
pub fn opening(menu: Menu) {
    for other in ALL {
        if other != menu && other.up_or_coming() {
            other.shut();
            DISPLACED.lock().unwrap_or_else(|e| e.into_inner())[other.index()] = Some(Instant::now());
        }
    }
}

/// A server line would open `menu` (it isn't up yet): false when it is a late refresh of a menu
/// this client just closed for another one.
pub fn server_may_open(menu: Menu) -> bool {
    DISPLACED.lock().unwrap_or_else(|e| e.into_inner())[menu.index()].is_none_or(|at| at.elapsed().as_secs_f32() >= DISPLACED_SECS)
}

/// This client asked the server for `menu` (a Tab menu row): its answer opens it.
pub fn asked(menu: Menu) {
    DISPLACED.lock().unwrap_or_else(|e| e.into_inner())[menu.index()] = None;
}

/// F went to the server: whatever it opens (PC, Tutor, wardrobe) was asked for.
pub fn asked_any() {
    *DISPLACED.lock().unwrap_or_else(|e| e.into_inner()) = [None; N];
}

/// Leaving a server: nothing queued for it, held or remembered carries over.
pub fn forget() {
    OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).clear();
    asked_any();
    *HELD.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *WAS_OPEN.lock().unwrap_or_else(|e| e.into_inner()) = false;
}

/// Sends the waiting close lines.
pub fn flush(host: &mut dyn Host) {
    let lines = std::mem::take(&mut *OUTBOX.lock().unwrap_or_else(|e| e.into_inner()));
    for l in lines {
        let _ = host.net_send_to_server(CHANNEL, l.as_bytes());
    }
}

/// Walking, jumping and crouching stop while a menu is up, held keys included (the menu uses
/// WASD and Space itself). Photo mode reads them first (`photo::movement_input`): they fly its camera.
pub fn movement_input(event: &mut MovementInputEvent) {
    hold_still(&mut event.forward, &mut event.strafe, &mut event.jump, &mut event.sneak);
}

fn hold_still(forward: &mut f32, strafe: &mut f32, jump: &mut bool, sneak: &mut bool) {
    if any_open() {
        (*forward, *strafe, *jump, *sneak) = (0.0, 0.0, false, false);
    }
}

// ---------------------------------------------------------------- held keys (#37)

/// A held list key repeats after this long...
pub const REPEAT_DELAY: f32 = 0.3;
/// ...at this many steps a second...
pub const REPEAT_START: f32 = 8.0;
/// ...rising to this many...
pub const REPEAT_TOP: f32 = 20.0;
/// ...over this long.
pub const REPEAT_RAMP: f32 = 1.5;
/// A slow frame never sends more than this many steps at once.
pub const REPEAT_BURST: u32 = 3;

/// Repeat steps due once a key has been held `held` seconds (the press itself not counted): the
/// first at `REPEAT_DELAY`, then at a rate rising evenly from `REPEAT_START` to `REPEAT_TOP` a
/// second over `REPEAT_RAMP`.
pub fn repeats_by(held: f32) -> u32 {
    let u = held - REPEAT_DELAY;
    if u < 0.0 || u.is_nan() {
        return 0;
    }
    let r = REPEAT_RAMP;
    let area = if u <= r {
        REPEAT_START * u + (REPEAT_TOP - REPEAT_START) * u * u / (2.0 * r)
    } else {
        (REPEAT_START + REPEAT_TOP) / 2.0 * r + REPEAT_TOP * (u - r)
    };
    1 + area as u32
}

/// Steps to send now, `sent` already sent and the key held `held` seconds: (steps, new `sent`).
/// A long frame drops the extra steps instead of jumping down the list.
pub fn repeat_step(sent: u32, held: f32) -> (u32, u32) {
    let due = repeats_by(held);
    (due.saturating_sub(sent).min(REPEAT_BURST), due.max(sent))
}

/// The keys that repeat: W/S and the up/down arrows.
fn list_key(key: &str) -> Option<&'static str> {
    ["key.keyboard.w", "key.keyboard.s", "key.keyboard.up", "key.keyboard.down"].into_iter().find(|k| *k == key)
}

struct Held {
    key: &'static str,
    menu: Menu,
    at: Instant,
    sent: u32,
}

static HELD: Mutex<Option<Held>> = Mutex::new(None);

/// A fresh press went to the open menu: W/S on a list start repeating; any other key stops it.
pub fn pressed(key: &str) {
    let mut h = HELD.lock().unwrap_or_else(|e| e.into_inner());
    *h = match (list_key(key), open_now()) {
        (Some(key), Some(menu)) if menu.is_list() => Some(Held { key, menu, at: Instant::now(), sent: 0 }),
        _ => None,
    };
}

pub fn released(key: &str) {
    let mut h = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if h.as_ref().is_some_and(|held| held.key == key) {
        *h = None;
    }
}

/// The open list's own key handler.
fn send_key(host: &mut dyn Host, menu: Menu, key: &str) {
    let _ = match menu {
        Menu::Tab => hub::key(host, key),
        Menu::Wardrobe => cosmetics::draw::key(host, key),
        Menu::Dex => dex::key(key),
        Menu::Pc => pc::key(host, key),
        Menu::Tutor => tutor::key(host, key),
        Menu::Rift => riftmenu::key(host, key),
        _ => false,
    };
}

/// Whether a menu was up last frame, for the guard and dodge reset when one opens.
static WAS_OPEN: Mutex<bool> = Mutex::new(false);

/// Each frame: the outbox, held-key repeats, and whether a menu just opened or closed (true, the
/// new state) so `play` can drop or restore the guard.
pub fn tick(host: &mut dyn Host) -> Option<bool> {
    riftmenu::tick(host);
    flush(host);
    let now = open_now();
    let step = {
        let mut h = HELD.lock().unwrap_or_else(|e| e.into_inner());
        match h.as_mut() {
            Some(held) if now == Some(held.menu) && host.playing() => {
                let (n, sent) = repeat_step(held.sent, held.at.elapsed().as_secs_f32());
                held.sent = sent;
                Some((held.menu, held.key, n))
            }
            Some(_) => {
                *h = None;
                None
            }
            None => None,
        }
    };
    if let Some((menu, key, n)) = step {
        for _ in 0..n {
            send_key(host, menu, key);
        }
        flush(host);
    }
    let open = now.is_some();
    let mut was = WAS_OPEN.lock().unwrap_or_else(|e| e.into_inner());
    (std::mem::replace(&mut *was, open) != open).then_some(open)
}

/// One lock for every test that opens or closes a menu: they share these statics.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        for m in ALL {
            m.shut();
        }
        OUTBOX.lock().unwrap().clear();
        *DISPLACED.lock().unwrap() = [None; N];
        *HELD.lock().unwrap() = None;
    }

    fn up() -> Vec<Menu> {
        ALL.into_iter().filter(|m| m.is_up()).collect()
    }

    fn outbox() -> Vec<&'static str> {
        std::mem::take(&mut *OUTBOX.lock().unwrap())
    }

    #[test]
    fn opening_a_menu_closes_the_one_that_was_up() {
        let _g = test_lock();
        reset();
        assert!(up().is_empty());
        hub::toggle();
        assert_eq!(up(), [Menu::Tab]);
        // The server opens the wardrobe (F at the counter, /wardrobe) over the Tab menu.
        assert!(cosmetics::draw::heard(&["ward", "5", "-", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Wardrobe]);
        assert_eq!(outbox(), Vec::<&str>::new(), "the Tab menu is the client's own");
        // Onii's play-test: the Capipidex opened over the wardrobe and both stayed.
        assert!(dex::heard(&["dex", "open", "12", "989"]));
        assert_eq!(up(), [Menu::Dex]);
        assert_eq!(outbox(), ["ward\tclose"], "the server stops refreshing the wardrobe");
        assert!(pc::heard(&["pc", "0", "pikachu", "pikachu,eevee"]));
        assert_eq!(up(), [Menu::Pc]);
        assert!(tutor::heard(&["tutor", "3", "a", "b", "c", "-"]));
        assert_eq!(up(), [Menu::Tutor]);
        assert_eq!(outbox(), ["pc\tclose"]);
        guide::open_help();
        assert_eq!(up(), [Menu::Help]);
        assert_eq!(outbox(), ["tutor\tclose"]);
        crate::emotes::open_wheel();
        assert_eq!(up(), [Menu::Emotes]);
        photo::start();
        assert_eq!(up(), [Menu::Photo]);
        hub::toggle();
        assert_eq!(up(), [Menu::Tab]);
        catch::open_wheel();
        assert_eq!(up(), [Menu::Orders]);
        reset();
    }

    #[test]
    fn a_late_refresh_does_not_bring_a_closed_menu_back() {
        let _g = test_lock();
        reset();
        assert!(cosmetics::draw::heard(&["ward", "5", "-", "-", "-", "hat.bow"]));
        assert!(dex::heard(&["dex", "open", "12", "989"]));
        // The wardrobe's refresh was already on its way when the book opened.
        assert!(cosmetics::draw::heard(&["ward", "4", "hat.bow", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Dex]);
        // F at the counter asks again: the answer opens it (the book closes).
        asked_any();
        assert!(cosmetics::draw::heard(&["ward", "4", "hat.bow", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Wardrobe]);
        asked(Menu::Dex);
        assert!(dex::heard(&["dex", "open", "12", "989"]));
        assert!(cosmetics::draw::heard(&["ward", "4", "hat.bow", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Dex], "displaced again: a late refresh");
        // The same line, asked for from the Tab menu's Wardrobe row, opens it.
        asked(Menu::Wardrobe);
        assert!(cosmetics::draw::heard(&["ward", "4", "hat.bow", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Wardrobe]);
        // A refresh of the open wardrobe closes nothing and isn't an opening.
        assert!(cosmetics::draw::heard(&["ward", "3", "hat.bow", "-", "-", "hat.bow"]));
        assert_eq!(up(), [Menu::Wardrobe]);
        reset();
    }

    #[test]
    fn typed_help_closes_the_open_menu() {
        let _g = test_lock();
        reset();
        asked(Menu::Wardrobe);
        assert!(cosmetics::draw::heard(&["ward", "5", "-", "-", "-", "-"]));
        assert_eq!(up(), [Menu::Wardrobe]);
        assert!(guide::command("/help"));
        assert_eq!(up(), [Menu::Help]);
        assert_eq!(outbox(), ["ward\tclose"]);
        asked(Menu::Pc);
        assert!(pc::heard(&["pc", "0", "pikachu", "pikachu,eevee"]));
        assert_eq!(up(), [Menu::Pc]);
        assert!(guide::command("/controls"));
        assert_eq!(up(), [Menu::Help]);
        assert_eq!(outbox(), ["pc\tclose"]);
        reset();
    }

    #[test]
    fn a_map_on_its_way_closes_when_another_menu_opens() {
        let _g = test_lock();
        reset();
        assert!(worldmap::ask(), "M: the map is asked for");
        assert!(worldmap::pending());
        assert!(up().is_empty(), "not up until its data comes");
        hub::toggle();
        assert_eq!(up(), [Menu::Tab]);
        assert_eq!(outbox(), ["map\tclose"], "the server stops sending map data");
        // The answer to the first ask arrives after the Tab menu opened.
        assert!(worldmap::heard(&["mapdata", "0,0,100,100", "", "", ""]));
        assert_eq!(up(), [Menu::Tab], "late map data doesn't open the map over the Tab menu");
        // Asked again with nothing in the way, the data opens it.
        hub::close();
        assert!(worldmap::ask());
        assert!(worldmap::heard(&["mapdata", "0,0,100,100", "", "", ""]));
        assert_eq!(up(), [Menu::Map]);
        reset();
    }

    #[test]
    fn no_walking_or_jumping_while_a_menu_is_up() {
        let _g = test_lock();
        reset();
        // (forward, strafe, jump, sneak) as `movement_input` gets them, every key held.
        let held = || (1.0f32, -1.0f32, true, true);
        let mut e = held();
        hold_still(&mut e.0, &mut e.1, &mut e.2, &mut e.3);
        assert_eq!(e, held(), "nothing open: the keys go through");
        assert!(!any_open());
        for open in [Menu::Tab, Menu::Dex, Menu::Wardrobe, Menu::Help, Menu::Emotes] {
            match open {
                Menu::Tab => hub::toggle(),
                Menu::Dex => assert!(dex::heard(&["dex", "open", "1", "2"])),
                Menu::Wardrobe => {
                    asked(Menu::Wardrobe);
                    assert!(cosmetics::draw::heard(&["ward", "5", "-", "-", "-", "-"]));
                }
                Menu::Help => guide::open_help(),
                _ => crate::emotes::open_wheel(),
            }
            assert!(any_open(), "{open:?}");
            let mut e = held();
            hold_still(&mut e.0, &mut e.1, &mut e.2, &mut e.3);
            assert_eq!(e, (0.0, 0.0, false, false), "{open:?} holds the Pokémon still");
        }
        reset();
        assert!(!any_open());
    }

    #[test]
    fn held_keys_repeat_after_a_pause_and_speed_up() {
        assert_eq!(repeats_by(0.0), 0, "the press itself is the first step");
        assert_eq!(repeats_by(0.29), 0);
        assert_eq!(repeats_by(0.3), 1, "first repeat at 300 ms");
        let mut last = 0;
        for ms in 0..8000 {
            let n = repeats_by(ms as f32 / 1000.0);
            assert!(n >= last && n - last <= 1, "one step at a time, never back ({ms} ms)");
            last = n;
        }
        // About 8 a second at first, 20 a second once it has sped up.
        let early = repeats_by(0.8) - repeats_by(0.3);
        assert!((4..=6).contains(&early), "{early} in the first half second");
        assert_eq!(repeats_by(6.0) - repeats_by(5.0), 20);
        assert!(repeats_by(3.5) - repeats_by(3.0) > 2 * early - 1, "faster the longer it's held");
    }

    #[test]
    fn a_slow_frame_does_not_jump_down_the_list() {
        assert_eq!(repeat_step(0, 0.1), (0, 0));
        assert_eq!(repeat_step(0, 0.31), (1, 1));
        assert_eq!(repeat_step(1, 0.31), (0, 1));
        let (n, sent) = repeat_step(1, 4.0);
        assert_eq!(n, REPEAT_BURST);
        assert_eq!(sent, repeats_by(4.0), "the rest are dropped, not owed");
    }

    #[test]
    fn only_list_keys_on_a_list_repeat() {
        let _g = test_lock();
        reset();
        pressed("key.keyboard.s");
        assert!(HELD.lock().unwrap().is_none(), "nothing open");
        hub::toggle();
        pressed("key.keyboard.s");
        assert!(HELD.lock().unwrap().as_ref().is_some_and(|h| h.key == "key.keyboard.s" && h.menu == Menu::Tab));
        released("key.keyboard.w");
        assert!(HELD.lock().unwrap().is_some(), "another key's release");
        released("key.keyboard.s");
        assert!(HELD.lock().unwrap().is_none());
        pressed("key.keyboard.down");
        pressed("key.keyboard.x");
        assert!(HELD.lock().unwrap().is_none(), "any other key stops it");
        reset();
        guide::open_help();
        pressed("key.keyboard.w");
        assert!(HELD.lock().unwrap().is_none(), "help has no list");
        reset();
    }
}
