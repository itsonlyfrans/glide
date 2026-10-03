fn main() {
    println!("cargo:rustc-check-cfg=cfg(glide_release)");
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rustc-cfg=glide_release");
    }
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"),
    )
    .join("../glide-platform-win/Glide.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=glided=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=glided=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
