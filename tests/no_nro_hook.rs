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
