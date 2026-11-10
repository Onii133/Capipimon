// Runs as every title screen is built: no splash, including the date ones and "<NAME> IS YOU".
fn capipimon_models_no_splash(title: &mut crate::title::Title) {
    title.splash = None;
    title.splash_you = false;
}
