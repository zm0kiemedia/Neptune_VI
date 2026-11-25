//! Fingerprint the inputs to the shader translator for its disk cache.

use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFESTS: [&str; 4] = [
    "Cargo.toml",
    "build.rs",
    "../../Cargo.toml",
    "../../Cargo.lock",
];

fn main() {
    println!("cargo:rerun-if-changed=src");
    for path in MANIFESTS {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=RUSTC");

    // Cargo runs build scripts in the crate directory. Hash only relative
    // file names so identical builds can share a cache across checkouts.
    let mut files: Vec<_> = MANIFESTS.into_iter().map(PathBuf::from).collect();
    sources(Path::new("src"), &mut files);
    let mut files: Vec<_> = files
        .into_iter()
        .map(|p| (p.to_string_lossy().replace('\\', "/"), p))
        .collect();
    files.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut hash = blake3::Hasher::new_derive_key("VIctory shader translator build identity v1");
    let compiler = Command::new(std::env::var_os("RUSTC").expect("Cargo provides RUSTC"))
        .arg("-vV")
        .output()
        .expect("query rustc version");
    assert!(compiler.status.success(), "rustc -vV failed");
    field(&mut hash, "rustc", &compiler.stdout);
    for name in ["TARGET", "CARGO_CFG_TARGET_FEATURE"] {
        let value = std::env::var(name).unwrap_or_default();
        field(&mut hash, name, value.as_bytes());
    }
    let mut features: Vec<_> = std::env::vars_os()
        .filter_map(|(name, _)| {
            let name = name.into_string().ok()?;
            name.starts_with("CARGO_FEATURE_").then_some(name)
        })
        .collect();
    features.sort_unstable();
    for feature in features {
        field(&mut hash, "feature", feature.as_bytes());
    }
    for (name, path) in files {
        let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {name}: {error}"));
        field(&mut hash, &name, &bytes);
    }
    println!(
        "cargo:rustc-env=VICTORY_SHADER_SOURCE_HASH={}",
        hash.finalize().to_hex()
    );
}

fn field(hash: &mut blake3::Hasher, name: &str, value: &[u8]) {
    for bytes in [name.as_bytes(), value] {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
}

fn sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if entry.file_type().expect("read source file type").is_dir() {
            sources(&path, files);
        } else {
            files.push(path);
        }
    }
}
