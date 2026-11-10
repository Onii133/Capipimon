//! Cosmetics: hats, particle auras and particle trails (GDD: everything is earned by playing,
//! nothing is sold; cosmetics are visual only and never touch combat or hitboxes).
//!
//! The catalogue lives in the content mod (`cosmetics/catalog.json`, hat models, the per-species hat
//! anchors), so a new hat needs a content update and no native rebuild.
//!
//! Ownership, shards and what's worn sit behind this module's own lock, never `SERVER`: gyms, raids
//! and the dungeon capsules call `roll` and `grant` from inside the tick, with `SERVER` already held.
//! The lock order is always `SERVER`, then `STORE`; nothing here locks `SERVER`. Each tick the
//! changes are copied into the player's record in `capipimon-players.json` (`pve.wardrobe`).
//!
//! Wire (channel `capipimon:play`):
//! - server → client: `cosm\t<name>\t<hat|->\t<aura|->\t<trail|->` (one player's loadout),
//!   `ward\t<shards>\t<hat>\t<aura>\t<trail>\t<owned,ids>` (open or refresh), `ward\t-` (close),
//!   `wardnote\t<text>`.
//! - client → server: `ward\topen|close`, `ward\tequip\t<id>`, `ward\tunequip\t<slot or id>`,
//!   `ward\tbuy\t<id>`.

use super::*;
use glam::{Quat, Vec3};
use std::sync::RwLock;

// The server half: private, never published (docs/CLIENT_SERVER_SPLIT.md).
#[cfg(feature = "capipimon_server")]
include!("server/cosmetics.rs");

pub const SHOP_COST: [u32; 3] = [25, 100, 300];

fn rank(r: Rarity) -> usize {
    match r {
        Rarity::Common => 0,
        Rarity::Rare => 1,
        Rarity::Legendary => 2,
    }
}

#[path = "cosmetics_draw.rs"]
pub mod draw;

// ---------------------------------------------------------------- the contract other modules use

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rarity {
    Common,
    Rare,
    Legendary,
}

/// `hat.top_hat`, `aura.petals`, `trail.sparks`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CosmeticId(pub String);

impl CosmeticId {
    pub fn new(id: &str) -> Self {
        Self(id.to_string())
    }
}

pub fn display_name(id: &CosmeticId) -> String {
    catalog().item(&id.0).map_or_else(|| id.0.clone(), |i| i.name.clone())
}

/// 0xRRGGBB.
pub fn rarity_color(r: Rarity) -> u32 {
    match r {
        Rarity::Common => 0xB8C4D0,
        Rarity::Rare => 0x4FA8FF,
        Rarity::Legendary => 0xFFC23F,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    Hat,
    Aura,
    Trail,
}

/// How an aura's particles move (trails drift the same way behind the player).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Motion {
    #[default]
    Rise,
    Fall,
    Orbit,
    Flicker,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Particle {
    pub sprite: String,
    #[serde(default)]
    pub motion: Motion,
    /// 0xRRGGBB as "RRGGBB"; white when left out.
    #[serde(default)]
    pub tint: Option<String>,
    /// Particles alive at once, for one player up close.
    pub count: u32,
    /// Half width, blocks.
    pub size: f32,
    #[serde(default = "one")]
    pub speed: f32,
    /// A trail's seconds per particle, and the blocks between them.
    #[serde(default = "one")]
    pub life: f32,
    #[serde(default = "spacing")]
    pub spacing: f32,
    /// Full bright, ignoring the world's light.
    #[serde(default = "yes")]
    pub glow: bool,
}

fn one() -> f32 {
    1.0
}
fn spacing() -> f32 {
    0.45
}
fn yes() -> bool {
    true
}

impl Particle {
    pub fn tint_rgb(&self) -> u32 {
        self.tint.as_deref().and_then(|t| u32::from_str_radix(t, 16).ok()).unwrap_or(0xFF_FFFF)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HatRef {
    pub geo: String,
    pub sprite: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub slot: Slot,
    pub rarity: Rarity,
    /// `capsule`, `prestige`, `seal:<gym>`, `raid:<which>`, `cameo:<who>`, `season:<step>`,
    /// `legend:<species>` (an escaped legendary, `legend:other` for the rest).
    pub sources: Vec<String>,
    /// The names this was saved under before there was a catalogue.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub hat: Option<HatRef>,
    #[serde(default)]
    pub particle: Option<Particle>,
}

impl Item {
    /// In the shard shop. Raid, season, seal, cameo and prestige exclusives are only ever earned
    /// by doing those things.
    pub fn capsule(&self) -> bool {
        self.sources.iter().any(|s| s == "capsule")
    }

    /// Can come out of a capsule: anything the shop sells except hats, which are shop-only. Auras
    /// aren't in the shop: they come from Prestige, and Stars from the season track.
    pub fn rollable(&self) -> bool {
        self.capsule() && self.slot != Slot::Hat
    }
}

#[derive(Deserialize)]
struct CatalogFile {
    schema: u32,
    items: Vec<Item>,
}

/// Where a hat sits on one species, from `tools/cosmetics/gen_anchors.py`.
#[derive(Clone, Debug, Deserialize)]
pub struct Anchor {
    /// The bone it rides on.
    pub b: String,
    /// Top of the head in the bone's unposed space (viewport pixels, the same space as `Cube`).
    pub p: [f32; 3],
    /// Scale: head width / 8.
    pub s: f32,
    /// Quaternion [x, y, z, w] standing the hat upright against the bone's rest rotation.
    #[serde(default)]
    pub q: Option<[f32; 4]>,
    /// Extra turn about the hat's up axis, degrees.
    #[serde(default)]
    pub y: f32,
    /// False: found by the body-plan fallback, not a head bone.
    #[serde(default = "yes")]
    pub ok: bool,
}

impl Anchor {
    pub fn rotation(&self) -> Quat {
        let r = self.q.map_or(Quat::IDENTITY, |[x, y, z, w]| Quat::from_xyzw(x, y, z, w));
        r * Quat::from_axis_angle(Vec3::Y, self.y.to_radians())
    }
}

#[derive(Default)]
pub struct Catalog {
    pub items: Vec<Item>,
    by_id: HashMap<String, usize>,
    /// Hat id → its model.
    pub hats: HashMap<String, bedrock::Model>,
    /// Species → hat anchor.
    pub anchors: HashMap<String, Anchor>,
    /// Named groups of hats for the wardrobe's filter, from the optional `sets.json`.
    pub sets: Vec<HatSet>,
}

/// A named group of hats (a unit's collection, the classics), in wardrobe order.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct HatSet {
    pub name: String,
    pub items: Vec<String>,
}

#[derive(Deserialize)]
struct SetsFile {
    schema: u32,
    sets: Vec<HatSet>,
}

/// The wardrobe sets, keeping only ids that are hats in `cat` and putting each hat in one set.
/// A bad or missing file only costs the filter, never the catalogue.
pub fn parse_sets(text: &str, cat: &Catalog) -> Result<Vec<HatSet>, String> {
    let file: SetsFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if file.schema != 1 {
        return Err(format!("unknown schema {}", file.schema));
    }
    let mut seen = HashSet::new();
    let sets = file
        .sets
        .into_iter()
        .map(|s| HatSet {
            items: s.items.into_iter().filter(|id| cat.item(id).is_some_and(|i| i.slot == Slot::Hat) && seen.insert(id.clone())).collect(),
            name: s.name,
        })
        .filter(|s| !s.items.is_empty())
        .collect();
    Ok(sets)
}

use crate::bedrock;

impl Catalog {
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: CatalogFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if file.schema != 1 {
            return Err(format!("unknown schema {}", file.schema));
        }
        let mut by_id = HashMap::new();
        for (i, item) in file.items.iter().enumerate() {
            let want = match item.slot {
                Slot::Hat => "hat.",
                Slot::Aura => "aura.",
                Slot::Trail => "trail.",
            };
            if !item.id.starts_with(want) {
                return Err(format!("{}: a {:?} id must start with {want}", item.id, item.slot));
            }
            if by_id.insert(item.id.clone(), i).is_some() {
                return Err(format!("{}: listed twice", item.id));
            }
            let right = match item.slot {
                Slot::Hat => item.hat.is_some(),
                Slot::Aura | Slot::Trail => item.particle.is_some(),
            };
            if !right {
                return Err(format!("{}: missing its hat or particle section", item.id));
            }
        }
        Ok(Self { items: file.items, by_id, ..Default::default() })
    }

    /// Everything in the cosmetics folder, with `read` giving a file's text from its relative path.
    pub fn load(read: impl Fn(&str) -> Result<String, String>) -> Result<Self, String> {
        let text = read("catalog.json").map_err(|e| format!("catalog.json: {e}"))?;
        let mut cat = Self::parse(&text)?;
        for item in &cat.items {
            let Some(hat) = &item.hat else { continue };
            let geo = read(&hat.geo).map_err(|e| format!("{}: {e}", hat.geo))?;
            let model = bedrock::Model::parse(&geo).map_err(|e| format!("{}: {e}", hat.geo))?;
            cat.hats.insert(item.id.clone(), model);
        }
        let anchors = read("anchors.json").map_err(|e| format!("anchors.json: {e}"))?;
        cat.anchors = serde_json::from_str(&anchors).map_err(|e| format!("anchors.json: {e}"))?;
        if let Ok(text) = read("sets.json") {
            match parse_sets(&text, &cat) {
                Ok(sets) => cat.sets = sets,
                Err(e) => log::warn!("capipimon: sets.json ignored: {e}"),
            }
        }
        Ok(cat)
    }

    pub fn item(&self, id: &str) -> Option<&Item> {
        self.by_id.get(id).map(|&i| &self.items[i])
    }

    /// The wardrobe set a hat belongs to.
    pub fn set_of(&self, id: &str) -> Option<&str> {
        self.sets.iter().find(|s| s.items.iter().any(|i| i == id)).map(|s| s.name.as_str())
    }

    /// The id saved under an old display name (`Season 3 Ribbon` matches `Ribbon`).
    fn by_alias(&self, name: &str) -> Option<&Item> {
        let bare = name.strip_prefix("Season ").and_then(|r| r.split_once(' ')).map_or(name, |(_, n)| n);
        self.items.iter().find(|i| i.aliases.iter().any(|a| a == name || a == bare))
    }
}

static CATALOG: LazyLock<RwLock<Arc<Catalog>>> = LazyLock::new(Default::default);
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn catalog() -> Arc<Catalog> {
    CATALOG.read().unwrap().clone()
}

/// Reads the catalogue once per process, on whichever side comes first.
pub fn install(host: &dyn Host) {
    if INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    let folder = crate::content_folder(host);
    let read = |file: &str| crate::pack::read(&folder, &format!("cosmetics/{file}")).ok_or_else(|| "missing".to_string());
    match Catalog::load(read) {
        Ok(cat) => {
            log::info!("capipimon: {} cosmetics, {} hat anchors", cat.items.len(), cat.anchors.len());
            *CATALOG.write().unwrap() = Arc::new(cat);
        }
        Err(e) => log::error!("capipimon: cosmetics didn't load: {e}"),
    }
}

use std::sync::atomic::Ordering;

