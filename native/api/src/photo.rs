//! Photo mode (client only): `/photo` clears the screen for a screenshot.
//!
//! - Every HUD layer is hidden, the game's own and Capipimon's.
//! - The camera flies free (Onii 2026-11-13): W/A/S/D move it the way it looks, Space up, Shift
//!   down, and it turns with the mouse. The Pokémon stays where it stood: its movement, dodge,
//!   guard and move keys do nothing until the photo ends. The mouse still turns the Pokémon on the
//!   spot (the engine has no hook that keeps the look apart from the player's). The camera stays
//!   within `LEASH` blocks of the player.
//! - [1] stands normally, [2] and [3] hold the species' physical and special attack pose. Only this
//!   client sees the pose: nothing is sent to the server, and it gives no advantage.
//! - [G] puts a thin frame with the Capipimon mark on the screen, [X] (or `/photo` again) leaves.
//!
//! It ends by itself on a KO, when a capsule catches you, or on a disconnect.

use super::*;

/// The hint stays up this long, then the screen is clean.
const HINT_SECS: f32 = 5.0;
/// The free camera stays this close to the player (chunks only load around the player).
const LEASH: f64 = 24.0;
/// The free camera's speed, blocks a second.
const FLY_SPEED: f64 = 6.0;

struct Photo {
    at: Instant,
    /// Where the free camera is, from the first frame of the photo.
    cam: Option<[f64; 3]>,
    /// The movement keys, taken from the Pokémon: forward, left, up (each -1..1).
    fly: [f32; 3],
    /// The last camera frame, for how far to move this one.
    last: Option<Instant>,
    /// None: standing; Some(special): the attack pose held.
    pose: Option<bool>,
    frame: bool,
}

impl Photo {
    fn new() -> Self {
        Photo { at: Instant::now(), cam: None, fly: [0.0; 3], last: None, pose: None, frame: false }
    }
}

static PHOTO: Mutex<Option<Photo>> = Mutex::new(None);

pub fn on() -> bool {
    PHOTO.lock().unwrap().is_some()
}

/// The pose this client's own Pokémon holds: `Some(special)`, or none for its normal stance.
pub fn pose() -> Option<bool> {
    PHOTO.lock().unwrap().as_ref().and_then(|p| p.pose)
}

pub fn forget() {
    *PHOTO.lock().unwrap() = None;
}

/// `/photo`: on, or off again.
pub fn command(host: &mut dyn Host, command: &str) -> bool {
    if command.trim_start_matches('/').trim() != "photo" {
        return false;
    }
    let mut p = PHOTO.lock().unwrap();
    if p.is_some() {
        *p = None;
        return true;
    }
    if !host.playing() {
        return true;
    }
    drop(p);
    super::catch::team_photo(host);
    start();
    true
}

/// Photo mode on, closing any other menu (`menus`).
pub fn start() {
    *PHOTO.lock().unwrap() = Some(Photo::new());
    menus::opening(menus::Menu::Photo);
}

/// Photo mode off (another menu is opening).
pub fn stop() {
    *PHOTO.lock().unwrap() = None;
}

/// Keys while the photo is up; true when taken (nothing reaches the moves).
pub fn key(key: &str) -> bool {
    let mut guard = PHOTO.lock().unwrap();
    let Some(p) = guard.as_mut() else { return false };
    match key {
        "key.keyboard.1" => p.pose = None,
        "key.keyboard.2" => p.pose = Some(false),
        "key.keyboard.3" => p.pose = Some(true),
        "key.keyboard.g" => p.frame = !p.frame,
        "key.keyboard.x" => *guard = None,
        // The other move keys and F do nothing during a photo, so a pose isn't broken by a stray cast.
        "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r" | "key.keyboard.z" | "key.keyboard.c" | "key.keyboard.f" | "key.keyboard.4" | "key.keyboard.5" => {}
        // These fly the camera (`movement_input`). Taken here so a double tap isn't a dodge and
        // Shift isn't a guard.
        "key.keyboard.w" | "key.keyboard.a" | "key.keyboard.s" | "key.keyboard.d" | "key.keyboard.space" | "key.keyboard.left.shift" | "key.keyboard.right.shift" => {}
        _ => return false,
    }
    true
}

/// During a photo the movement keys fly the camera, and the Pokémon stands still.
pub fn movement_input(event: &mut MovementInputEvent) {
    steer(&mut event.forward, &mut event.strafe, &mut event.jump, &mut event.sneak);
}

fn steer(forward: &mut f32, strafe: &mut f32, jump: &mut bool, sneak: &mut bool) {
    let mut guard = PHOTO.lock().unwrap();
    let Some(p) = guard.as_mut() else { return };
    p.fly = [*forward, *strafe, f32::from(u8::from(*jump)) - f32::from(u8::from(*sneak))];
    (*forward, *strafe, *jump, *sneak) = (0.0, 0.0, false, false);
}

/// Which way the free camera moves for `fly` (forward, left, up) when it faces `yaw` and `pitch`
/// (Minecraft degrees: yaw 0 looks to +z, pitch 90 looks down). At most one block per unit.
fn fly_dir(fly: [f32; 3], yaw: f32, pitch: f32) -> [f64; 3] {
    let (y, p) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    let look = [-y.sin() * p.cos(), -p.sin(), y.cos() * p.cos()];
    let left = [y.cos(), 0.0, y.sin()];
    let (f, l, u) = (f64::from(fly[0]), f64::from(fly[1]), f64::from(fly[2]));
    let v = [look[0] * f + left[0] * l, look[1] * f + u, look[2] * f + left[2] * l];
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1.0 { [v[0] / len, v[1] / len, v[2] / len] } else { v }
}

/// `at`, pulled back to within `r` of `centre`.
fn leash(at: [f64; 3], centre: [f64; 3], r: f64) -> [f64; 3] {
    let d = [at[0] - centre[0], at[1] - centre[1], at[2] - centre[2]];
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if len <= r {
        return at;
    }
    let k = r / len;
    [centre[0] + d[0] * k, centre[1] + d[1] * k, centre[2] + d[2] * k]
}

/// Moves the free camera. Runs after the shoulder camera has placed it, which is where it starts.
pub fn camera_pose(event: &mut CameraPoseEvent<'_>) {
    let mut guard = PHOTO.lock().unwrap();
    let Some(p) = guard.as_mut() else { return };
    let here = *event.position;
    let dt = p.last.map_or(0.0, |t| t.elapsed().as_secs_f64().min(0.1));
    p.last = Some(Instant::now());
    let cam = p.cam.unwrap_or(here);
    let d = fly_dir(p.fly, event.yaw, event.pitch);
    let step = FLY_SPEED * dt;
    let cam = leash([cam[0] + d[0] * step, cam[1] + d[1] * step, cam[2] + d[2] * step], here, LEASH);
    p.cam = Some(cam);
    *event.position = cam;
}

/// Where the frame's lines go on a `w` x `h` screen: (x, y, w, h) of the four bars.
fn frame_bars(w: f32, h: f32) -> [(f32, f32, f32, f32); 4] {
    let (m, t) = (8.0, 1.0);
    [(m, m, w - 2.0 * m, t), (m, h - m - t, w - 2.0 * m, t), (m, m, t, h - 2.0 * m), (w - m - t, m, t, h - 2.0 * m)]
}

/// Draws the photo's screen; true when a photo is up (the rest of the HUD is skipped).
pub fn hud(host: &mut dyn Host, b: &mut HudBatch) -> bool {
    // A fainted player's camera belongs to the KO view, and a caught one needs the struggle bar.
    let down = CLIENT.lock().unwrap().ko.is_some();
    let caught = catch::struggling();
    let (age, frame) = {
        let mut guard = PHOTO.lock().unwrap();
        if down || caught {
            *guard = None;
        }
        let Some(p) = guard.as_ref() else { return false };
        (p.at.elapsed().as_secs_f32(), p.frame)
    };
    let (w, h) = (b.width, b.height);
    if frame {
        for (x, y, bw, bh) in frame_bars(w, h) {
            rect(host, b, x, y, bw, bh, 0xE0FF_FFFF);
        }
        // The mark: a capsule and the name, bottom left inside the frame.
        let (x, y) = (20.0, h - 24.0);
        emblem(host, b, x, y + 4.0, 5.0);
        host.text(b, "CAPIPIMON", [x + 9.0, y], 0xFFFFFF, true);
    }
    if age < HINT_SECS {
        let hint = "Photo  [WASD] camera  [1] stand  [2] [3] pose  [G] frame  [X] done";
        guide::keyed_centred(host, b, hint, w / 2.0, h - 44.0, 0xFFFFFF);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_stays_inside_the_screen() {
        for (x, y, w, h) in frame_bars(320.0, 180.0) {
            assert!(x >= 0.0 && y >= 0.0 && x + w <= 320.0 && y + h <= 180.0 && w > 0.0 && h > 0.0);
        }
    }

    #[test]
    fn the_camera_flies_the_way_it_looks() {
        let close = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(close(fly_dir([1.0, 0.0, 0.0], 0.0, 0.0), [0.0, 0.0, 1.0]), "W at yaw 0 goes to +z");
        assert!(close(fly_dir([1.0, 0.0, 0.0], 90.0, 0.0), [-1.0, 0.0, 0.0]), "yaw 90 looks to -x");
        assert!(close(fly_dir([0.0, 1.0, 0.0], 0.0, 0.0), [1.0, 0.0, 0.0]), "A at yaw 0 goes to +x");
        assert!(close(fly_dir([1.0, 0.0, 0.0], 0.0, 90.0), [0.0, -1.0, 0.0]), "looking down, W goes down");
        assert!(close(fly_dir([0.0, 0.0, -1.0], 0.0, 0.0), [0.0, -1.0, 0.0]), "Shift goes down");
        let diag = fly_dir([1.0, 1.0, 0.0], 0.0, 0.0);
        assert!(((diag[0] * diag[0] + diag[2] * diag[2]).sqrt() - 1.0).abs() < 1e-6, "no faster on a diagonal");
        assert!(close(fly_dir([0.0; 3], 33.0, 12.0), [0.0; 3]));
    }

    #[test]
    fn the_camera_stays_near_the_player() {
        assert_eq!(leash([3.0, 0.0, 4.0], [0.0; 3], 10.0), [3.0, 0.0, 4.0]);
        let far = leash([30.0, 0.0, 40.0], [0.0; 3], 10.0);
        assert!((far[0] - 6.0).abs() < 1e-9 && (far[2] - 8.0).abs() < 1e-9);
    }

    // One test for everything that uses the shared PHOTO, so parallel tests don't race on it.
    #[test]
    fn photo_keys_and_movement() {
        let _g = menus::test_lock();
        forget();
        let mut e = (1.0f32, -1.0f32, true, false);
        steer(&mut e.0, &mut e.1, &mut e.2, &mut e.3);
        assert_eq!(e, (1.0, -1.0, true, false), "no photo: the Pokémon walks");
        assert!(!key("key.keyboard.2"));
        *PHOTO.lock().unwrap() = Some(Photo::new());
        steer(&mut e.0, &mut e.1, &mut e.2, &mut e.3);
        assert_eq!(e, (0.0, 0.0, false, false), "photo: the Pokémon stands still");
        assert_eq!(PHOTO.lock().unwrap().as_ref().unwrap().fly, [1.0, -1.0, 1.0], "and the camera gets the keys");
        assert!(key("key.keyboard.3"));
        assert_eq!(pose(), Some(true));
        assert!(key("key.keyboard.1"));
        assert_eq!(pose(), None);
        assert!(key("key.keyboard.w") && key("key.keyboard.left.shift") && key("key.keyboard.f"), "no dodge, guard or use");
        assert!(!key("key.keyboard.t"));
        assert!(key("key.keyboard.x"));
        assert!(!on());
    }
}
