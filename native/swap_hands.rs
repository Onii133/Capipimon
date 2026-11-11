// F doesn't swap hands while the player is a Pokémon (the server refuses it too). Same `view-<process id>`
// setting as drop.rs and frame.rs.
fn capipimon_models_swap_hands(game: &mut crate::game::Game<'_>) -> std::ops::ControlFlow<()> {
    let view = crate::mod_api::Host::setting(&*game, "capipimon_models", &format!("view-{}", std::process::id()));
    if view.as_deref() == Some("pokemon") { std::ops::ControlFlow::Break(()) } else { std::ops::ControlFlow::Continue(()) }
}
