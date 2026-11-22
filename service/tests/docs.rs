//! The build notes name the Rust version the crate needs: someone who
//! follows them must not hit a build failure.

use std::path::Path;

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_documented_rust_version_is_the_one_cargo_requires() {
    let manifest = read("Cargo.toml");
    let required = manifest
        .lines()
        .find_map(|l| l.strip_prefix("rust-version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("Cargo.toml names a rust-version");
    for doc in ["../TECHNICAL.md", "README.md"] {
        let text = read(doc);
        let named: Vec<&str> = text
            .match_indices("Rust ")
            .filter_map(|(at, _)| text[at + 5..].split(|c: char| !(c.is_ascii_digit() || c == '.')).next())
            .filter(|v| v.contains('.') && v.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .collect();
        assert!(!named.is_empty(), "{doc} names no Rust version");
        for v in named {
            assert_eq!(v, required, "{doc} says Rust {v}, Cargo.toml requires {required}");
        }
    }
}
