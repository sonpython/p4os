use std::{env, fs, path::PathBuf};

// Every app links with this script under the name `link.x`, which the build
// passes with `-T` (see scripts/build-userapps.sh).
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("link.x", out.join("link.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=link.x");
}
