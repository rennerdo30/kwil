use crate::{Refusal, Result, require};
use sha2::{Digest, Sha256};

pub const FAMILY: &str = "{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}";
const PRODUCT_NS: &str = "{918F53A7-7A40-5E54-96EB-E2FD9B0C605B}";
const COMPONENT_NS: &str = "{E596D49A-3F36-5A1A-89C8-7D052FDE6C47}";
pub const INVENTORY_LIMIT: usize = 1_048_576;

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}
pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
pub fn is_hex(s: &str, length: usize) -> bool {
    s.len() == length && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn decimal(s: &str, max: u64) -> Result<u64> {
    require(
        !s.is_empty() && s.len() <= 20 && (s == "0" || !s.starts_with('0')) && s.bytes().all(|b| b.is_ascii_digit()),
        "invalid-metadata",
        "canonical decimal",
    )?;
    let n = s.parse::<u64>().map_err(|_| Refusal::new("limit", "integer overflow"))?;
    require(n <= max, "limit", "integer range")?;
    Ok(n)
}
pub fn version(s: &str) -> Result<[u32; 3]> {
    require(s.len() <= 20, "invalid-metadata", "version length")?;
    let mut parts = s.split('.');
    let mut out = [0; 3];
    for (i, max) in [255, 255, 65_535].into_iter().enumerate() {
        out[i] = decimal(parts.next().ok_or_else(|| Refusal::new("invalid-metadata", "version"))?, max)? as u32;
    }
    require(parts.next().is_none(), "invalid-metadata", "version component count")?;
    Ok(out)
}
pub fn lp(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let new_len = out
        .len()
        .checked_add(4)
        .and_then(|n| n.checked_add(s.len()))
        .ok_or_else(|| Refusal::new("limit", "canonical bytes"))?;
    require(new_len <= INVENTORY_LIMIT, "limit", "canonical bytes")?;
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

pub fn guid_bytes(s: &str) -> Result<[u8; 16]> {
    require(s.len() == 38 && s.starts_with('{') && s.ends_with('}'), "invalid-metadata", "GUID format")?;
    let mut out = [0; 16];
    let mut index = 0;
    let mut high = None;
    for (i, b) in s.bytes().enumerate().skip(1).take(36) {
        if [9, 14, 19, 24].contains(&i) {
            require(b == b'-', "invalid-metadata", "GUID separator")?;
            continue;
        }
        let n = match b {
            b'0'..=b'9' => b - b'0',
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(Refusal::new("invalid-metadata", "GUID uppercase hexadecimal")),
        };
        if let Some(h) = high.take() {
            require(index < 16, "invalid-metadata", "GUID length")?;
            out[index] = h * 16 + n;
            index += 1;
        } else {
            high = Some(n);
        }
    }
    require(index == 16 && high.is_none(), "invalid-metadata", "GUID length")?;
    Ok(out)
}
fn guid_text(bytes: &[u8; 16]) -> String {
    let h = hex(bytes).to_ascii_uppercase();
    format!("{{{}-{}-{}-{}-{}}}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}
pub fn package_guid(s: &str) -> Result<()> {
    let b = guid_bytes(s)?;
    require(b[6] >> 4 == 4 && b[8] & 0xc0 == 0x80, "invalid-metadata", "PackageCode UUIDv4")
}

// SHA-1 is used exclusively for RFC UUIDv5 name derivation, never ownership
// content/authentication. Input is bounded canonical component/product names.
fn sha1_name(bytes: &[u8]) -> Result<[u8; 20]> {
    require(bytes.len() <= 4096, "limit", "UUID name")?;
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    let mut h = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.into_iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0; 20];
    for (chunk, value) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    Ok(out)
}
pub fn uuid5(namespace: &str, name: &[u8]) -> Result<String> {
    require(name.len() <= 4000, "limit", "UUID name")?;
    let mut bytes = guid_bytes(namespace)?.to_vec();
    bytes.extend_from_slice(name);
    let hash = sha1_name(&bytes)?;
    let mut id = [0; 16];
    id.copy_from_slice(&hash[..16]);
    id[6] = (id[6] & 15) | 0x50;
    id[8] = (id[8] & 63) | 0x80;
    Ok(guid_text(&id))
}
pub fn component(arch: &str, kind: &str, identity: &str) -> Result<(String, String)> {
    let mut bytes = b"KWC1".to_vec();
    for s in [arch, kind, identity, "1"] {
        lp(&mut bytes, s)?;
    }
    let guid = uuid5(COMPONENT_NS, &bytes)?;
    let id = format!("C{}", guid.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>());
    Ok((id, guid))
}
pub fn product(arch: &str, version: &str, build: &str) -> Result<String> {
    let mut bytes = b"KWP1".to_vec();
    for s in [arch, version, build] {
        lp(&mut bytes, s)?;
    }
    uuid5(PRODUCT_NS, &bytes)
}
