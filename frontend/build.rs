use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={timestamp}");
    println!("cargo:rerun-if-changed=build.rs");
    // Any source change re-runs trunk, which re-runs this script per crate build;
    // emitting rerun-if-changed=src keeps the timestamp stable within one build.
    println!("cargo:rerun-if-changed=src");
}
