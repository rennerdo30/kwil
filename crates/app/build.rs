fn main() {
    println!("cargo:rerun-if-changed=../../assets/logo/kwil.ico");
    println!("cargo:rerun-if-changed=../ui-gpu/windows.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let path =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../ui-gpu/windows.manifest");
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let resource = out.join("kwil.res");
        std::fs::write(&resource, resources(include_bytes!("../../assets/logo/kwil.ico"))).unwrap();
        println!("cargo:rustc-link-arg={}", resource.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", path.display());
    }
}

// Standard .res records, consumed by the MSVC/LLD resource linker. No rc.exe,
// subprocess, environment changes or build-time system SDK discovery.
fn align(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}
fn record(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    align(out);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    for value in [0xffff, kind, 0xffff, id] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x1030u16.to_le_bytes());
    out.extend_from_slice(&0x0409u16.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(data);
    align(out);
}
fn block(key: &str, kind: u16, value: &[u8], value_len: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend_from_slice(&value_len.to_le_bytes());
    b.extend_from_slice(&kind.to_le_bytes());
    for c in key.encode_utf16().chain(Some(0)) {
        b.extend_from_slice(&c.to_le_bytes());
    }
    align(&mut b);
    b.extend_from_slice(value);
    for child in children {
        align(&mut b);
        b.extend_from_slice(child);
    }
    let n = b.len() as u16;
    b[..2].copy_from_slice(&n.to_le_bytes());
    b
}
fn string(key: &str, value: &str) -> Vec<u8> {
    let wide: Vec<u8> = value.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
    block(key, 1, &wide, (wide.len() / 2) as u16, &[])
}
fn resources(ico: &[u8]) -> Vec<u8> {
    let count = u16::from_le_bytes(ico[4..6].try_into().unwrap()) as usize;
    let mut out = Vec::new();
    record(&mut out, 0, 0, &[]);
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for index in 0..count {
        let entry = &ico[6 + index * 16..6 + (index + 1) * 16];
        let len = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let start = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let id = (index + 1) as u16;
        record(&mut out, 3, id, &ico[start..start + len]);
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    record(&mut out, 14, 1, &group);
    let strings = block(
        "040904b0",
        1,
        &[],
        0,
        &[
            string("CompanyName", "rennerdo30"),
            string("FileDescription", "Kwil text editor"),
            string("FileVersion", env!("CARGO_PKG_VERSION")),
            string("ProductName", "Kwil"),
            string("ProductVersion", env!("CARGO_PKG_VERSION")),
            string("OriginalFilename", "kwil.exe"),
        ],
    );
    let info = block("StringFileInfo", 1, &[], 0, &[strings]);
    let translation = block("Translation", 0, &[9, 4, 176, 4], 4, &[]);
    let vars = block("VarFileInfo", 1, &[], 0, &[translation]);
    let mut fixed = Vec::new();
    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
    let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap();
    assert!(major <= 65535 && minor <= 65535 && patch <= 65535);
    let ms = (major << 16) | minor;
    let ls = patch << 16;
    for n in [0xfeef04bdu32, 0x10000, ms, ls, ms, ls, 0x3f, 0, 0x40004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&n.to_le_bytes());
    }
    record(&mut out, 16, 1, &block("VS_VERSION_INFO", 0, &fixed, 52, &[info, vars]));
    out
}
