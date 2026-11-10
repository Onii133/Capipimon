// Capipimon, in the game each frame (GDD 3.1): while the player is a Pokémon the camera is third
// person, the inventory stays shut and the number keys don't pick a hotbar slot (they're moves).
// Leaving (reset, or a KO that ends as a trainer) gives the view the player had back.
//
// The API half says which it is in the mod's setting `view-<process id>` (`pokemon` or
// `trainer`), written only when it changes; it's read here every 10 frames.
fn capipimon_models_frame(game: &mut crate::game::Game<'_>) {
    use std::sync::Mutex;
    struct State {
        frames: u32,
        step: f64,
        pokemon: bool,
        before: Option<crate::game::perspective::CameraType>,
        slot: Option<usize>,
        regrab: Option<std::time::Instant>,
    }
    static STATE: Mutex<State> = Mutex::new(State { frames: 0, step: 0.6, pokemon: false, before: None, slot: None, regrab: None });
    let Ok(mut s) = STATE.lock() else { return };
    s.frames = s.frames.wrapping_add(1);
    // Windows drops the cursor clip behind the game's back (another app calling ClipCursor, UAC,
    // a display change), and Horizon only repairs that on Linux, so the pointer wanders off a
    // focused game. Only while playing with the mouse captured: menus, chat and pause release
    // the capture, and alt-tab clears `focused`. Release, centre, lock: something else can
    // re-lock an escaped pointer where it stands (outside the window), and a warp alone can't
    // leave that 1x1 clip. Looking is raw mouse input, so none of this touches the view.
    #[cfg(windows)]
    if game.screen == crate::game::Screen::Playing && game.focused && game.captured && !game.options.offscreen {
        let due = s.regrab.is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(250));
        if due {
            s.regrab = Some(std::time::Instant::now());
            if let (Some(window), Some(mode)) = (&game.window, game.capture_mode) {
                let size = window.inner_size();
                let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
                let _ = window.set_cursor_position(winit::dpi::PhysicalPosition::new(f64::from(size.width) / 2.0, f64::from(size.height) / 2.0));
                let _ = window.set_cursor_grab(mode);
                window.set_cursor_visible(false);
            }
        }
    }
    // A Pokémon walks through berry bushes: the next move forgets the bush's slowdown.
    if s.pokemon {
        if let Some(m) = game.player.as_mut() {
            let v = &mut m.stuck_multiplier;
            if (v.x - f64::from(0.8_f32)).abs() < 1.0e-6 && (v.y - 0.75).abs() < 1.0e-6 && (v.z - f64::from(0.8_f32)).abs() < 1.0e-6 {
                (v.x, v.y, v.z) = (0.0, 0.0, 0.0);
            }
        }
    }
    if s.frames % 10 == 1 {
        let key = format!("view-{}", std::process::id());
        let view = crate::mod_api::Host::setting(&*game, "capipimon_models", &key);
        // Step height for this form. Re-applied if it drifts: a respawn makes a new player.
        let step = crate::mod_api::Host::setting(&*game, "capipimon_models", &format!("step-{}", std::process::id()));
        s.step = step.and_then(|v| v.parse().ok()).unwrap_or(0.6);
        if let Some(m) = game.player.as_mut() {
            if m.attribute("minecraft:step_height").is_ok_and(|v| (v - s.step).abs() > 0.01) {
                let want = s.step;
                if let Some(key) = m.attributes.key("minecraft:step_height") {
                    if let Ok(Some(handle)) = m.attributes.get_instance(Some(key)) {
                        let _ = m.attributes.edit(&handle, |attribute, observer| attribute.set_base_value(want, observer));
                        let _ = m.refresh_attributes();
                    }
                }
            }
        }
        let pokemon = view.as_deref() == Some("pokemon");
        if pokemon != s.pokemon {
            s.pokemon = pokemon;
            if pokemon {
                s.before = Some(game.camera_type);
                s.slot = Some(game.selected);
                game.camera_type = crate::game::perspective::CameraType::ThirdPersonBack;
            } else {
                if let Some(before) = s.before.take() {
                    game.camera_type = before;
                }
                s.slot = None;
            }
        }
    }
    if !s.pokemon {
        return;
    }
    // Number keys: moves, not hotbar slots. The slot stays where it was (the next tick tells the
    // server again if a key got through first).
    let _ = std::mem::take(&mut game.hotbar_presses);
    if let Some(slot) = s.slot {
        if game.selected != slot {
            game.selected = slot;
        }
    }
    // E: a move, not the inventory.
    if game.screen == crate::game::Screen::Inventory {
        game.close_container();
        game.screen = crate::game::Screen::Playing;
        game.capture(true);
    } else if game.screen == crate::game::Screen::Creative {
        if let Some(c) = game.carried.take() {
            let _ = game.give(c);
        }
        game.screen = crate::game::Screen::Playing;
        game.capture(true);
    }
}
