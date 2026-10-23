fn main() {
    println!("cargo:rerun-if-changed=../ui-gpu/windows.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let path =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../ui-gpu/windows.manifest");
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", path.display());
    }
}
