// Q is a move while the player is a Pokémon, so it doesn't drop the held item. The API half says which the
// player is in the mod's setting `view-<process id>` (`pokemon` or `trainer`), the same one frame.rs reads.
fn capipimon_models_drop(game: &mut crate::game::Game<'_>) -> std::ops::ControlFlow<()> {
    let view = crate::mod_api::Host::setting(&*game, "capipimon_models", &format!("view-{}", std::process::id()));
    if view.as_deref() == Some("pokemon") { std::ops::ControlFlow::Break(()) } else { std::ops::ControlFlow::Continue(()) }
}
