//! Pictures of every species for menus: a head icon (the PC box) and the whole body (the summary),
//! which turns all the way round on the spot through the angles rendered of it.
//!
//! Rendered offline from the models themselves, each on the idle it plays in game, by
//! `tools/cosmetics/portraits.py` into GUI textures (`textures/gui/pc_heads_<n>.png`,
//! `portraits_<n>.png`), with `cobblemon-models/portraits.json` giving each species' cell. Drawn
//! with `host.sprite`, so nothing is rendered at run time. A species without a picture (a content
//! folder from before them, a model added since) gets `false` and the caller draws its old block.

use super::*;
use horizon_client::mod_api::HudBatch;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct Sheets {
    heads: Vec<String>,
    head_px: f32,
    head_cols: usize,
    head_per_sheet: usize,
    bodies: Vec<String>,
    body_px: f32,
    body_cols: usize,
    per_sheet: usize,
    /// The body's turns, the front first then further to its right, each turn's sheets as
    /// `bodies` lays them out. The turns to its left are these mirrored.
    #[serde(default)]
    turns: Vec<Vec<String>>,
    /// Each turn's angle, degrees (0 the front, -180 its back). Turns reaching its back make a
    /// full turn; fewer (an older content folder) a sway out to each side and back.
    #[serde(default)]
    turn_degrees: Vec<f32>,
    cells: HashMap<String, usize>,
}

/// How long the summary shows each angle of a full turn (8 angles: a turn in 4.8 s).
const TURN_STEP_MS: u128 = 600;
/// How long the summary shows each step of the sway (an older content folder).
const SWAY_STEP_MS: u128 = 420;

/// Step `step` of a full turn through `turns` turns (the front .. its back): which turn, and
/// whether mirrored. From the front round by its right to its back, then on round by its left,
/// whose angles are its right's mirrored (5 turns: 8 angles, 45 degrees apart).
fn turntable(turns: usize, step: u128) -> (usize, bool) {
    let k = turns.max(1);
    let steps: Vec<(usize, bool)> = (0..k).map(|i| (i, false)).chain((1..k.saturating_sub(1)).rev().map(|i| (i, true))).collect();
    steps[(step % steps.len() as u128) as usize]
}

/// Step `step` of the sway through `turns` turns: which turn, and whether mirrored. From the
/// front out to its right and back, then out to its left and back (3 turns: 8 steps).
fn sway(turns: usize, step: u128) -> (usize, bool) {
    let k = turns.max(1);
    let out = |m: bool| (0..k).chain((1..k.saturating_sub(1)).rev()).map(move |i| (i, m && i > 0));
    let steps: Vec<(usize, bool)> = out(false).chain(out(true)).collect();
    steps[(step % steps.len() as u128) as usize]
}

/// Which turn the summary shows `ms` after it first drew one, and whether mirrored.
fn turn_at(s: &Sheets, ms: u128) -> (usize, bool) {
    let full = s.turns.len() > 2 && s.turn_degrees.len() == s.turns.len() && s.turn_degrees.last().is_some_and(|d| d.abs() >= 179.0);
    if full { turntable(s.turns.len(), ms / TURN_STEP_MS) } else { sway(s.turns.len(), ms / SWAY_STEP_MS) }
}

fn sheets(host: &dyn Host) -> &'static Sheets {
    static SHEETS: OnceLock<Sheets> = OnceLock::new();
    SHEETS.get_or_init(|| {
        let text = pack::read(&crate::content_folder(host), "cobblemon-models/portraits.json").unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    })
}

/// The cell of `species`, else of its base species (`<base>_<form>`).
fn cell(s: &Sheets, species: &str) -> Option<usize> {
    s.cells.get(species).or_else(|| species.split_once('_').and_then(|(base, _)| s.cells.get(base))).copied()
}

/// `species`' head into `dst` (`[x, y, w, h]`, GUI px). False when there is no picture.
pub fn head(host: &mut dyn Host, b: &mut HudBatch, species: &str, dst: [f32; 4]) -> bool {
    let s = sheets(host);
    let Some(n) = cell(s, species) else { return false };
    let (sheet, at) = (n / s.head_per_sheet.max(1), n % s.head_per_sheet.max(1));
    let Some(name) = s.heads.get(sheet) else { return false };
    let cols = s.head_cols.max(1);
    let src = [(at % cols) as f32 * s.head_px, (at / cols) as f32 * s.head_px, s.head_px, s.head_px];
    host.sprite(b, name, src, dst)
}

/// `species`' whole body into `dst`, turning round on the spot (an older content folder: swaying
/// side to side); facing front when its turns are missing. False when there is no picture.
pub fn body(host: &mut dyn Host, b: &mut HudBatch, species: &str, dst: [f32; 4]) -> bool {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let s = sheets(host);
    let Some(n) = cell(s, species) else { return false };
    let (sheet, at) = (n / s.per_sheet.max(1), n % s.per_sheet.max(1));
    let cols = s.body_cols.max(1);
    let (x, y, px) = ((at % cols) as f32 * s.body_px, (at / cols) as f32 * s.body_px, s.body_px);
    let (turn, mirror) = turn_at(s, START.get_or_init(std::time::Instant::now).elapsed().as_millis());
    if let Some(name) = s.turns.get(turn).and_then(|t| t.get(sheet)) {
        // A negative width reads the cell right to left: the turn to the other side.
        let src = if mirror { [x + px, y, -px, px] } else { [x, y, px, px] };
        if host.sprite(b, name, src, dst) {
            return true;
        }
    }
    let Some(name) = s.bodies.get(sheet) else { return false };
    host.sprite(b, name, [x, y, px, px], dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_sways_out_to_each_side_and_back() {
        let steps: Vec<_> = (0..9).map(|i| sway(3, i)).collect();
        let want = [(0, false), (1, false), (2, false), (1, false), (0, false), (1, true), (2, true), (1, true), (0, false)];
        assert_eq!(steps, want);
        // One turn (an old content folder): always the front.
        assert!((0..5).all(|i| sway(1, i) == (0, false)));
        assert!((0..5).all(|i| sway(0, i) == (0, false)));
    }

    #[test]
    fn the_body_turns_all_the_way_round() {
        let steps: Vec<_> = (0..9).map(|i| turntable(5, i)).collect();
        let want = [(0, false), (1, false), (2, false), (3, false), (4, false), (3, true), (2, true), (1, true), (0, false)];
        assert_eq!(steps, want);
        assert!((0..5).all(|i| turntable(1, i) == (0, false)));
        assert!((0..5).all(|i| turntable(0, i) == (0, false)));
    }

    #[test]
    fn turns_reaching_the_back_make_a_full_turn_else_a_sway() {
        let names = |n: usize| (0..n).map(|i| vec![format!("t{i}")]).collect::<Vec<_>>();
        let full = Sheets { turns: names(5), turn_degrees: vec![0.0, -45.0, -90.0, -135.0, -180.0], ..Default::default() };
        assert_eq!(turn_at(&full, 4 * TURN_STEP_MS), (4, false));
        assert_eq!(turn_at(&full, 5 * TURN_STEP_MS), (3, true));
        let old = Sheets { turns: names(3), turn_degrees: vec![0.0, -30.0, -60.0], ..Default::default() };
        assert_eq!(turn_at(&old, 5 * SWAY_STEP_MS), (1, true));
        // Turns without their angles: the sway, never a turn to a back that isn't there.
        let bare = Sheets { turns: names(5), ..Default::default() };
        assert_eq!(turn_at(&bare, 5 * SWAY_STEP_MS), sway(5, 5));
    }
}
