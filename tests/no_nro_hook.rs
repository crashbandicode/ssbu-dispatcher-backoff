//! An rlib alongside the cdylib skips LTO and retains Skyline NRO hook imports.

use std::fs;
use std::path::Path;

fn check(path: &Path) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            check(&entry.unwrap().path());
        }
    } else {
        let text = fs::read_to_string(path).unwrap();
        for forbidden in [
            "add_nro_load_hook",
            "add_nro_unload_hook",
            "skyline::nro",
            "libnro_hook",
        ] {
            assert!(
                !text.contains(forbidden),
                "{} contains {forbidden}",
                path.display()
            );
        }
    }
}

#[test]
fn no_nro_hook_provider_in_source_or_manifest() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    check(&root.join("src"));
    check(&root.join("Cargo.toml"));
}

#[test]
fn the_plugin_is_built_as_a_cdylib_only() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .unwrap();
    let crate_types: Vec<_> = manifest
        .lines()
        .filter(|line| line.trim_start().starts_with("crate-type"))
        .collect();
    assert_eq!(crate_types, ["crate-type = [\"cdylib\"]"]);
}
