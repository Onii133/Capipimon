//! Photo mode (client only): `/photo` clears the screen for a screenshot.
//!
//! - Every HUD layer is hidden, the game's own and Capipimon's.
//! - The camera stays where it was when the photo started, so you can step into the shot. It still
//!   turns with the mouse: the engine gives a mod the camera's position, not its direction. Pick
//!   the view with F5 before `/photo`.
//! - [1] stands normally, [2] and [3] hold the species' physical and special attack pose. Only this
//!   client sees the pose: nothing is sent to the server, and it gives no advantage.
//! - [G] puts a thin frame with the Capipimon mark on the screen, [X] (or `/photo` again) leaves.
//!
//! It ends by itself on a KO, when a capsule catches you, on a disconnect, or when you walk too far from the camera.

use super::*;

/// The hint stays up this long, then the screen is clean.
const HINT_SECS: f32 = 5.0;
/// Walking this far from the held camera ends the photo.
const LEASH: f64 = 24.0;

struct Photo {
    at: Instant,
    /// Where the camera is held, from the first frame of the photo.
    cam: Option<[f64; 3]>,
    /// None: standing; Some(special): the attack pose held.
    pose: Option<bool>,
    frame: bool,
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
    *p = Some(Photo { at: Instant::now(), cam: None, pose: None, frame: false });
    true
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
        // The other move keys do nothing during a photo, so a pose isn't broken by a stray cast.
        "key.keyboard.q" | "key.keyboard.e" | "key.keyboard.r" | "key.keyboard.c" | "key.keyboard.4" | "key.keyboard.5" => {}
        _ => return false,
    }
    true
}

/// Holds the camera where the photo started. Runs after the shoulder camera has placed it.
pub fn camera_pose(event: &mut CameraPoseEvent<'_>) {
    let mut guard = PHOTO.lock().unwrap();
    let Some(p) = guard.as_mut() else { return };
    let here = *event.position;
    match p.cam {
        None => p.cam = Some(here),
        Some(cam) => {
            let d = [here[0] - cam[0], here[1] - cam[1], here[2] - cam[2]];
            if (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() > LEASH {
                *guard = None;
            } else {
                *event.position = cam;
            }
        }
    }
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
    let caught = OB_CLIENT.lock().unwrap().struggle.is_some();
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
        ball(host, b, x, y + 4.0, 5.0);
        host.text(b, "CAPIPIMON", [x + 9.0, y], 0xFFFFFF, true);
    }
    if age < HINT_SECS {
        let hint = "Photo  [1] stand  [2] [3] pose  [G] frame  [X] done";
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
    fn keys_only_matter_while_a_photo_is_up() {
        forget();
        assert!(!key("key.keyboard.2"));
        *PHOTO.lock().unwrap() = Some(Photo { at: Instant::now(), cam: None, pose: None, frame: false });
        assert!(key("key.keyboard.3"));
        assert_eq!(pose(), Some(true));
        assert!(key("key.keyboard.1"));
        assert_eq!(pose(), None);
        assert!(!key("key.keyboard.w"), "walking still works");
        assert!(key("key.keyboard.x"));
        assert!(!on());
    }
}
