//! Post-build step: copies the bundle's content pack beside the built game, where `api/src/pack.rs`
//! finds it. A build without a pack (dev, tests) has nothing to copy and reads the content folder.

use std::path::Path;
use std::process::ExitCode;

const FILE: &str = "capipimon-content.pack";

fn main() -> ExitCode {
    let var = |name: &str| std::env::var_os(name).expect(name);
    let from = Path::new(&var("HORIZON_NATIVE_BUNDLE_DATA_DIR")).join(FILE);
    if !from.is_file() {
        println!("no {FILE} in this bundle");
        return ExitCode::SUCCESS;
    }
    let to = Path::new(&var("HORIZON_NATIVE_DATA_DIR")).join(FILE);
    match std::fs::copy(&from, &to) {
        Ok(bytes) => {
            println!("{FILE}: {bytes} bytes");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{} -> {}: {e}", from.display(), to.display());
            ExitCode::FAILURE
        }
    }
}
