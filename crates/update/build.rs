fn main() {
    // The release asset name depends on the target triple.
    println!("cargo:rustc-env=KWIL_TARGET={}", std::env::var("TARGET").unwrap());
}
