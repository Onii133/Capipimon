//! Signature moves (slot C), the statuses they apply, and Ditto's Transform.
//!
//! Every playable species has one signature move (`data/signatures.json`, written by
//! `tools/signatures.py`, which also writes `design/SIGNATURE_MOVES.md`). A signature is a kit
//! move (the `move` object, same format as the kits) plus a hitbox form, one or two mechanics
//! from a fixed list, the numbers those mechanics read, and a VFX recipe (a motif drawn on top of
//! the type's look, a second colour, and `grand` for a final stage's bigger version).
//!
//! The move's id is `sig:<species>`, so the client finds the recipe from any `fx` message.
//! Ditto's untransformed kit is Transform on every key (`transform` mechanic).
//!
//! Wire (channel `capipimon:play`, tab-separated, all new; older clients ignore them):
//! - `status <name> <slow|stun|ward|empower|charged> <secs> <value>`: a status on a Pokémon player.
//! - `ditto <name> <1|0>`: that player is a transformed Ditto (HUD and name plate marker).
//! - `morph <name> <secs> <species>`: Ditto's morph goo; the client starts loading the model.
//! - `fx ... zone ...`: a lingering area (`a` = `b` a disc, else a band from a to b).

use super::*;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/sig.rs");

/// Transform reaches this far (blocks), and its cooldown after a miss (seconds).
const TRANSFORM_RANGE: f64 = 16.0;

const TRANSFORM_MISS_CD: f32 = 2.0;

/// Ditto (untransformed) uses Transform from every key; it lives in slot C (index 4).
pub fn plain_ditto(species: &str) -> bool {
    species == "ditto"
}

// ---------------------------------------------------------------- data

/// A signature's numbers for its mechanics (every field optional; the generator fills defaults).
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Params {
    /// multi / combo: hits, and seconds between them.
    pub hits: u32,
    pub every: f32,
    /// chain: extra targets, and how far each jump reaches (blocks).
    pub jumps: u32,
    pub reach: f32,
    /// knock / pull: blocks. launch: upward kick (blocks a tick).
    pub dist: f32,
    pub lift: f32,
    /// zone / pierce trail / ward / empower / slow / charged: seconds it lasts.
    pub secs: f32,
    /// slow: movement multiplier (0.5 = half speed).
    pub slow: f32,
    /// stun: seconds.
    pub stun: f32,
    /// drain: share of the damage dealt healed.
    pub drain: f32,
    /// meteor: seconds from release to the strike.
    pub delay: f32,
    /// fan: degrees between beams.
    pub spread: f32,
    /// zone / trail: share of the power each tick deals.
    pub share: f32,
    /// ward: damage taken multiplier; empower: damage dealt multiplier.
    pub buff: f32,
    /// slow: how long the Slowed lasts, when it isn't `secs`.
    pub slow_secs: f32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Sig {
    /// The move itself, in the kit format (id `sig:<species>`).
    #[serde(rename = "move")]
    pub mv: Move,
    /// beam, cone, dash, burst or ground.
    pub form: String,
    /// One or two of `MECHANICS`.
    pub mech: Vec<String>,
    #[serde(default)]
    pub p: Params,
    /// The VFX motif drawn on top of the type's look (`MOTIFS`).
    pub motif: String,
    /// The motif's colour (linear RGB 0-1).
    pub rgb2: [f32; 3],
    /// A final stage's bigger version.
    #[serde(default)]
    pub grand: bool,
    /// Wind-up before the Speed scaler (spec formula), and Showdown-style priority.
    pub base: f32,
    #[serde(default)]
    pub prio: i32,
    /// Look flags (bite 1, punch 2, slicing 4, pulse 8, bullet 16, wind 32, sound 64, ...).
    #[serde(default)]
    pub flags: u32,
    /// A dash's movement: distance for a 1-block Pokémon, vertical reach, aim (3d, ground, full).
    #[serde(default)]
    pub dash: Option<DashFx>,
    /// What it does, for `/capi sig`.
    #[serde(default)]
    pub desc: String,
}

pub const MECHANICS: &[&str] = &[
    "multi", "combo", "chain", "pull", "knock", "launch", "meteor", "zone", "pierce", "blink", "fan", "drain", "slow", "stun", "ward",
    "empower", "charged", "transform", "vanish", "tether", "field", "strip", "voltage", "storm", "rain", "tempest", "fly",
];

pub const MOTIFS: &[&str] = &[
    "none", "runes", "star", "crescent", "spiral", "pillar", "petals", "shards", "smoke", "hearts", "gears", "scales", "eye", "comet",
    "quake", "vortex", "bubbles", "web", "feathers", "echo", "thorns", "flames", "goo",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SigFile {
    version: u32,
    species: HashMap<String, Sig>,
}

#[cfg(not(feature = "capipimon_server"))]
const SIGNATURES: &str = include_str!("../data/signatures.json");
/// The featured kits' moves with mechanics (GDD 3.9, 8.2), written by `tools/featured_kits.py`.
#[cfg(not(feature = "capipimon_server"))]
const FEATURED: &str = include_str!("../data/featured.json");

fn table() -> &'static HashMap<String, Sig> {
    static ALL: LazyLock<HashMap<String, Sig>> = LazyLock::new(|| match (serde_json::from_str::<SigFile>(SIGNATURES), serde_json::from_str::<SigFile>(FEATURED)) {
        (Ok(f), Ok(g)) if f.version == 1 && g.version == 1 => f
            .species
            .into_iter()
            .chain(g.species)
            .filter(|(_, s)| {
                s.mech.iter().all(|m| MECHANICS.contains(&m.as_str()))
                    && MOTIFS.contains(&s.motif.as_str())
                    && s.mv.cooldown > 0.0
                    && s.mv.range > 0.0
                    && s.mv.width > 0.0
                    && (0.0..=100.0).contains(&s.mv.energy)
            })
            .collect(),
        (Err(e), _) | (_, Err(e)) => {
            log::error!("capipimon: signatures.json or featured.json: {e}");
            HashMap::new()
        }
        _ => HashMap::new(),
    });
    &ALL
}

/// How many signatures loaded (for `/capi kits`).
pub fn count() -> usize {
    table().len()
}

/// A species' signature move.
pub fn signature(species: &str) -> Option<&'static Sig> {
    table().get(species)
}

/// The signature a move id belongs to (`sig:<species>`), or Ditto's Transform.
pub fn of(id: &str) -> Option<&'static Sig> {
    if id == "transform" {
        return Some(transform_sig());
    }
    id.strip_prefix("sig:").and_then(signature)
}

/// Whether the signature move `id` has mechanic `m` (the client dresses some mechanics up).
pub fn has_mech(id: &str, m: &str) -> bool {
    of(id).is_some_and(|s| s.mech.iter().any(|x| x == m))
}

/// Ditto's only move (GDD 3.7 archetype 15, hand-tuned; Onii 2026-11-08: Ditto plays with
/// Transform only). A short beam that copies the first Pokémon it touches.
pub fn transform_sig() -> &'static Sig {
    static T: LazyLock<Sig> = LazyLock::new(|| Sig {
        mv: Move {
            id: "transform".into(),
            name: "Transform".into(),
            kind: "normal".into(),
            category: Category::Special,
            power: 0.0,
            shape: Shape::Beam,
            range: TRANSFORM_RANGE as f32,
            width: 1.2,
            cooldown: TRANSFORM_MISS_CD,
            energy: 0.0,
            windup: 0.0,
            anim: "special".into(),
            particle: "minecraft:witch".into(),
            status: None,
            filler: false,
            form: None,
            cone: 0.0,
            tele: None,
            charges: 0,
            slow: None,
            busy: 0.0,
            ground_wave: false,
            knock: None,
            support: None,
        },
        form: "beam".into(),
        mech: vec!["transform".into()],
        p: Params::default(),
        motif: "goo".into(),
        rgb2: [0.75, 0.45, 1.0],
        grand: false,
        // Status Shot floor 0.25 s (spec), x the Speed scaler at release.
        base: 0.25,
        prio: 0,
        flags: 0,
        dash: None,
        desc: "Copies the first Pokemon it touches (16 b): species, types, stats except HP, moves and signature.".into(),
    });
    &T
}

/// The look of a signature move for the effects pass: accent colour (linear RGB) and
/// `motif index + 32 * grand` (MOTIFS order; 0 none). None for every other move.
pub fn look(id: &str) -> Option<[f32; 4]> {
    let s = of(id)?;
    let m = MOTIFS.iter().position(|x| *x == s.motif).unwrap_or(0) as f32;
    Some([s.rgb2[0], s.rgb2[1], s.rgb2[2], m + if s.grand { 32.0 } else { 0.0 }])
}

/// Players with a status now, for the effects pass to follow: (name, aura kind 1 stun, 2 slow,
/// 3 ward, 4 empower, 5 charged, seconds left).
pub fn auras() -> Vec<(String, u32, f32)> {
    let c = SIGC.lock().unwrap();
    let now = Instant::now();
    let mut out = Vec::new();
    for (name, m) in &c.st {
        for (kind, k) in [("stun", 1), ("slow", 2), ("ward", 3), ("empower", 4), ("charged", 5)] {
            if let Some((until, _)) = m.get(kind) {
                let left = until.saturating_duration_since(now).as_secs_f32();
                if left > 0.0 {
                    out.push((name.clone(), k, left));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- client

#[derive(Default)]
pub struct SigClient {
    /// player name -> status kind -> (until, value).
    st: HashMap<String, HashMap<String, (Instant, f32)>>,
    /// Players who are transformed Dittos.
    dittos: HashSet<String>,
}

pub static SIGC: LazyLock<Mutex<SigClient>> = LazyLock::new(Default::default);

/// Server messages this module reads (none are consumed: the effects read `status` and `morph` too).
pub fn heard(host: &mut dyn Host, f: &[&str]) {
    if let Some(species) = note(f) {
        // Start loading the copied species' model now, so it's ready when the goo clears.
        let _ = crate::has_model(host, &species);
    }
}

/// Reads one server message into the client state; a `morph` returns the species to preload.
fn note(f: &[&str]) -> Option<String> {
    match f {
        ["status", name, kind, secs, value, ..] => {
            let secs = secs.parse::<f32>().unwrap_or(0.0).clamp(0.0, 30.0);
            let v = value.parse::<f32>().unwrap_or(0.0);
            let mut c = SIGC.lock().unwrap();
            c.st.entry(name.to_string()).or_default().insert(kind.to_string(), (Instant::now() + std::time::Duration::from_secs_f32(secs), v));
        }
        ["ditto", name, on, ..] => {
            let mut c = SIGC.lock().unwrap();
            if *on == "1" {
                c.dittos.insert(name.to_string());
            } else {
                c.dittos.remove(*name);
            }
        }
        ["species", name, species] if *species == "-" || plain_ditto(species) => {
            let mut c = SIGC.lock().unwrap();
            c.dittos.remove(*name);
            if *species == "-" {
                c.st.remove(*name);
            }
        }
        ["morph", _, _, species, ..] => return Some(species.to_string()),
        _ => {}
    }
    None
}

/// Whether this player is a transformed Ditto (marker on the plate and HUD).
pub fn is_ditto(name: &str) -> bool {
    SIGC.lock().unwrap().dittos.contains(name)
}

/// A status on a player now, and its value.
pub fn status(name: &str, kind: &str) -> Option<f32> {
    let c = SIGC.lock().unwrap();
    c.st.get(name).and_then(|m| m.get(kind)).filter(|(until, _)| Instant::now() < *until).map(|(_, v)| *v)
}

/// This client's own movement multiplier from statuses (slow, stun).
pub fn move_mult(me: &str) -> f32 {
    if status(me, "stun").is_some() {
        return 0.1;
    }
    status(me, "slow").map_or(1.0, |v| v.clamp(0.1, 1.3)) * status(me, "trim").map_or(1.0, |v| v.clamp(0.5, 1.0))
}

/// The statuses on a player now, for the HUD: (label, seconds left, colour).
pub fn statuses_of(name: &str) -> Vec<(&'static str, f32, u32)> {
    let c = SIGC.lock().unwrap();
    let Some(m) = c.st.get(name) else { return Vec::new() };
    let mut out = Vec::new();
    for (kind, label, col) in [
        ("stun", "STUN", 0xFFFF_D23Fu32),
        ("slow", "SLOW", 0xFF7F_A8D8),
        ("ward", "WARD", 0xFF5F_E0E0),
        ("empower", "POWER", 0xFFFF_7A3F),
        ("charged", "CHARGED", 0xFFF7_D02C),
        ("par", "PARALYZED", 0xFFF7_D02C),
        ("brn", "BURNED", 0xFFFF_7A3F),
        ("psn", "POISONED", 0xFFB9_7FD8),
        ("tox", "BADLY POISONED", 0xFF9B_4FD0),
        ("slp", "DROWSY", 0xFFB0_B8E0),
        ("frz", "FROZEN", 0xFF9F_E8F5),
        ("cnf", "CONFUSED", 0xFFFF_9BD0),
        ("bnd", "BOUND", 0xFFC9_A06A),
        ("wet", "WET", 0xFF5F_A8F0),
        ("atk", "ATK", 0xFFE0_8060),
        ("def", "DEF", 0xFF80_C080),
        ("spa", "SP.ATK", 0xFFD0_80E0),
        ("spd", "SP.DEF", 0xFF80_C0E0),
    ] {
        if let Some((until, v)) = m.get(kind) {
            let left = until.saturating_duration_since(Instant::now()).as_secs_f32();
            if left > 0.0 {
                // A stat stage says which way; a "slow" over 1 is really a Speed boost.
                let label = match (kind, *v) {
                    ("atk", x) if x > 0.0 => "ATK UP",
                    ("atk", _) => "ATK DOWN",
                    ("def", x) if x > 0.0 => "DEF UP",
                    ("def", _) => "DEF DOWN",
                    ("spa", x) if x > 0.0 => "SP.ATK UP",
                    ("spa", _) => "SP.ATK DOWN",
                    ("spd", x) if x > 0.0 => "SP.DEF UP",
                    ("spd", _) => "SP.DEF DOWN",
                    ("slow", x) if x > 1.0 => "FAST",
                    _ => label,
                };
                out.push((label, left, col));
            }
        }
    }
    out
}

pub fn forget_client() {
    *SIGC.lock().unwrap() = SigClient::default();
}

