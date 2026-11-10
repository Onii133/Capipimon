//! Models and cosmetics as one generated file, for registry installs.
//!
//! A registry `.hmod` is a single archive, so nothing can read the content mod's loose folders the
//! way a dev install or a server's copy does, and a native bundle can't hold 1,800 files.
//! `tools/package/build_packages.py` joins them into `capipimon-content.pack`, and the
//! `content_pack` post-build step puts it in this build's `native-data` folder. Without one, files
//! come from the content mod's folder as before.
//!
//! Format: `MAGIC`, the index's length (u64 LE), the index as JSON (`{"path": [offset, length]}`,
//! offsets counted from the end of the index), then the files one after another.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAGIC: &[u8; 8] = b"CAPIPAK1";
const FILE: &str = "capipimon-content.pack";

struct Pack {
    path: PathBuf,
    /// Where the files start.
    body: u64,
    entries: HashMap<String, (u64, u64)>,
}

impl Pack {
    fn open(path: &Path) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let mut head = [0u8; 16];
        file.read_exact(&mut head).map_err(|e| e.to_string())?;
        if &head[..8] != MAGIC {
            return Err("not a content pack".into());
        }
        let len = u64::from_le_bytes(head[8..].try_into().unwrap());
        let mut index = vec![0u8; len as usize];
        file.read_exact(&mut index).map_err(|e| e.to_string())?;
        let entries = serde_json::from_slice(&index).map_err(|e| e.to_string())?;
        Ok(Self { path: path.to_path_buf(), body: 16 + len, entries })
    }

    /// Opens the file each time, so model loading threads never wait on each other.
    fn read(&self, name: &str) -> Option<String> {
        let &(offset, len) = self.entries.get(name)?;
        let mut file = File::open(&self.path).ok()?;
        file.seek(SeekFrom::Start(self.body + offset)).ok()?;
        let mut bytes = vec![0u8; len as usize];
        file.read_exact(&mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    }
}

/// This build's pack, if it has one.
fn installed() -> Option<&'static Pack> {
    static PACK: OnceLock<Option<Pack>> = OnceLock::new();
    PACK.get_or_init(|| {
        let path = horizon_client::native_data::built(crate::MOD_ID)?.join(FILE);
        if !path.is_file() {
            return None;
        }
        Pack::open(&path)
            .inspect(|p| log::info!("capipimon: content pack with {} files", p.entries.len()))
            .inspect_err(|e| log::error!("capipimon: content pack {} unusable: {e}", path.display()))
            .ok()
    })
    .as_ref()
}

/// The text of `name` (`cosmetics/catalog.json`): from the pack, else from `folder`, the content
/// mod's folder.
pub fn read(folder: &Path, name: &str) -> Option<String> {
    from(installed(), folder, name)
}

fn from(pack: Option<&Pack>, folder: &Path, name: &str) -> Option<String> {
    pack.and_then(|p| p.read(name)).or_else(|| std::fs::read_to_string(folder.join(name)).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder, and a pack in it holding `files`.
    fn pack_of(tag: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("capi-pack-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut index, mut body) = (HashMap::new(), Vec::new());
        for (name, text) in files {
            index.insert(*name, (body.len() as u64, text.len() as u64));
            body.extend_from_slice(text.as_bytes());
        }
        let index = serde_json::to_vec(&index).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(index.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&index);
        bytes.extend_from_slice(&body);
        let path = dir.join(FILE);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_each_file_back() {
        let (_dir, path) = pack_of("read", &[("a/one.json", "first"), ("b/two.json", "sécond")]);
        let pack = Pack::open(&path).unwrap();
        assert_eq!(pack.read("a/one.json").as_deref(), Some("first"));
        assert_eq!(pack.read("b/two.json").as_deref(), Some("sécond"));
        assert_eq!(pack.read("c/none.json"), None);
    }

    #[test]
    fn the_folder_answers_what_the_pack_lacks() {
        let (dir, path) = pack_of("fall", &[("both.json", "from pack")]);
        std::fs::write(dir.join("both.json"), "from folder").unwrap();
        std::fs::write(dir.join("only.json"), "folder only").unwrap();
        let pack = Pack::open(&path).unwrap();
        assert_eq!(from(Some(&pack), &dir, "both.json").as_deref(), Some("from pack"));
        assert_eq!(from(Some(&pack), &dir, "only.json").as_deref(), Some("folder only"));
        assert_eq!(from(None, &dir, "both.json").as_deref(), Some("from folder"));
    }

    #[test]
    fn refuses_a_file_that_is_not_a_pack() {
        let (_dir, path) = pack_of("bad", &[]);
        std::fs::write(&path, b"definitely not a pack file").unwrap();
        assert!(Pack::open(&path).is_err());
    }
}
