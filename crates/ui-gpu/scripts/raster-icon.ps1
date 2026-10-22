# Rasterize the existing Kwil SVG; no runtime SVG/image dependency.
param([string]$ToolRoot = ('E:/Development/_cache/kwil-icon-tool-' + [Guid]::NewGuid()))
$ErrorActionPreference='Stop'
$repo=(Resolve-Path "$PSScriptRoot/../../..").Path
New-Item -ItemType Directory -Path "$ToolRoot/src" -Force | Out-Null
@'
[package]
name = "kwil-icon-raster"
version = "0.0.0"
edition = "2024"
[dependencies]
resvg = { version = "=0.48.1", default-features = false }
[workspace]
'@ | Set-Content "$ToolRoot/Cargo.toml"
@'
fn main() {
    let args:Vec<_>=std::env::args().collect();
    let data=std::fs::read(&args[1]).unwrap();
    let tree=resvg::usvg::Tree::from_data(&data,&resvg::usvg::Options::default()).unwrap();
    let mut pixels=resvg::tiny_skia::Pixmap::new(32,32).unwrap();
    resvg::render(&tree,resvg::tiny_skia::Transform::from_scale(32./512.,32./512.),&mut pixels.as_mut());
    let mut rgba=pixels.data().to_vec();
    for p in rgba.chunks_exact_mut(4) {let a=p[3] as u32;if a>0{for c in &mut p[..3] {*c=((*c as u32*255+a/2)/a).min(255) as u8;}}}
    std::fs::write(&args[2],rgba).unwrap();
}
'@ | Set-Content "$ToolRoot/src/main.rs"
$env:CARGO_TARGET_DIR='E:/Development/_cache/cargo-target/kwil-kw-11-renderer'
cargo run --manifest-path "$ToolRoot/Cargo.toml" --offline -j 4 -- "$repo/assets/logo/kwil-icon-small.svg" "$repo/crates/ui-gpu/src/icon.rgba"
if ($LASTEXITCODE -ne 0) { throw 'Icon rasterization failed' }
