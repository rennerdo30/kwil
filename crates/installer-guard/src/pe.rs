//! Bounded PE32+ inspection of the MSI Binary stream (never the loaded image).
use crate::{
    Refusal, Result,
    model::{Import, validate_imports},
    require,
};

pub trait ReadAt {
    fn len(&self) -> u64;
    /// At most 64 KiB per request. Adapters check the monotonic deadline.
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<()>;
}
fn read<const N: usize>(input: &mut impl ReadAt, offset: u64) -> Result<[u8; N]> {
    let range = file_range(offset, N as u64, input.len())?;
    let mut bytes = [0; N];
    input.read_at(range.raw, &mut bytes)?;
    Ok(bytes)
}
fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3], b[at + 4], b[at + 5], b[at + 6], b[at + 7]])
}
#[derive(Clone, Copy)]
struct Section {
    rva: u64,
    backed: u64,
    raw: u64,
}

/// One completely admitted physical span. Fixed metadata tables must be mapped
/// once at their full declared extent, BEFORE reading or iterating their rows.
#[derive(Clone, Copy)]
struct PhysicalRange {
    raw: u64,
    size: u64,
}
impl PhysicalRange {
    fn at(self, displacement: u64, width: u64) -> Result<u64> {
        require(displacement.checked_add(width).is_some_and(|end| end <= self.size), "invalid-pe", "PE range bounds")?;
        self.raw.checked_add(displacement).ok_or_else(|| Refusal::new("invalid-pe", "file offset"))
    }
}
fn file_range(raw: u64, size: u64, length: u64) -> Result<PhysicalRange> {
    require(raw.checked_add(size).is_some_and(|end| end <= length), "invalid-pe", "raw bounds")?;
    Ok(PhysicalRange { raw, size })
}
fn backing_section(sections: &[Section], rva: u64, size: u64) -> Result<&Section> {
    let end = rva.checked_add(size).ok_or_else(|| Refusal::new("invalid-pe", "RVA overflow"))?;
    let mut matches = sections.iter().filter(|s| rva >= s.rva && s.rva.checked_add(s.backed).is_some_and(|n| end <= n));
    let s = matches.next().ok_or_else(|| Refusal::new("invalid-pe", "RVA outside backed section"))?;
    require(matches.next().is_none(), "invalid-pe", "ambiguous section mapping")?;
    // A partial overlap would also change the mapping of an individual table
    // element. Reject it up front instead of weakening the former per-row check.
    require(
        !sections.iter().any(|other| {
            !std::ptr::eq(s, other)
                && other.backed != 0
                && other.rva < end
                && other.rva.checked_add(other.backed).is_some_and(|other_end| rva < other_end)
        }),
        "invalid-pe",
        "ambiguous section mapping",
    )?;
    Ok(s)
}
fn rva_range(sections: &[Section], rva: u64, size: u64, length: u64) -> Result<PhysicalRange> {
    let s = backing_section(sections, rva, size)?;
    let raw = s.raw.checked_add(rva - s.rva).ok_or_else(|| Refusal::new("invalid-pe", "file offset"))?;
    file_range(raw, size, length)
}
fn offset(sections: &[Section], rva: u64, size: u64, length: u64) -> Result<u64> {
    Ok(rva_range(sections, rva, size, length)?.raw)
}

struct NameBytes {
    raw: u64,
    len: usize,
    bytes: [u8; 129],
}
impl NameBytes {
    fn byte(&self, at: u64) -> Option<u8> {
        let index = usize::try_from(at.checked_sub(self.raw)?).ok()?;
        self.bytes.get(..self.len)?.get(index).copied()
    }
}

fn name(input: &mut impl ReadAt, sections: &[Section], rva: u64, previous: &mut Option<NameBytes>) -> Result<String> {
    // Admission is deliberately narrower than arbitrary loader-compatible PE:
    // the whole name INCLUDING NUL must stay in this one physical section.
    // Compute the bound before any read; never remap a boundary byte to a far
    // section just to discover that the name extends beyond this mapping.
    let section = backing_section(sections, rva, 1)?;
    let remaining = section.backed - (rva - section.rva);
    let raw = offset(sections, rva, 1, input.len())?;
    let mut bytes = [0; 129];
    let mut out = String::with_capacity(128);
    for i in 0..=128u64 {
        require(i < remaining, "invalid-pe", "name crosses physical section")?;
        let at = offset(
            sections,
            rva.checked_add(i).ok_or_else(|| Refusal::new("invalid-pe", "name overflow"))?,
            1,
            input.len(),
        )?;
        // Starts are ordered, but overlapping names can straddle a 64 KiB
        // cache boundary. Reuse their already-read suffix instead of rewinding
        // the sequential source. New physical reads remain forward ordered.
        let b = if let Some(byte) = previous.as_ref().and_then(|last| last.byte(at)) {
            byte
        } else {
            read::<1>(input, at)?[0]
        };
        bytes[i as usize] = b;
        if b == 0 {
            *previous = Some(NameBytes { raw, len: i as usize + 1, bytes });
            return Ok(out);
        }
        require(i < 128 && b.is_ascii() && (32..127).contains(&b), "invalid-pe", "import/export name")?;
        out.push(b as char);
    }
    Err(Refusal::new("invalid-pe", "unterminated name"))
}

struct NameReference {
    raw: u64,
    rva: u64,
    load: Option<&'static str>, // None is the single required ABI export name.
}

fn export_name_reference(input: &mut impl ReadAt, sections: &[Section], optional: &[u8; 240]) -> Result<NameReference> {
    let export_rva = u32le(optional, 112) as u64;
    let export_size = u32le(optional, 116) as u64;
    require(export_rva != 0 && export_size >= 40, "invalid-pe", "export directory")?;
    let directory = rva_range(sections, export_rva, export_size, input.len())?;
    let exports = read::<40>(input, directory.at(0, 40)?)?;
    require(u32le(&exports, 20) == 1 && u32le(&exports, 24) == 1, "invalid-pe", "sole ABI export")?;
    // Read the three fixed-size pointer/ordinal cells in physical order, then
    // defer the string with all import strings. Never jump to a DLL/export name
    // between descriptor records or return to these cells after far names.
    // Counts above are exactly one, so these are the COMPLETE EAT, name-pointer
    // and ordinal arrays, not just a probe of their first element.
    let mut cells = [
        (rva_range(sections, u32le(&exports, 28) as u64, 4, input.len())?, 0, 4),
        (rva_range(sections, u32le(&exports, 32) as u64, 4, input.len())?, 1, 4),
        (rva_range(sections, u32le(&exports, 36) as u64, 2, input.len())?, 2, 2),
    ];
    cells.sort_unstable_by_key(|cell| cell.0.raw);
    let mut values = [0; 3];
    for (range, index, width) in cells {
        let mut bytes = [0; 4];
        input.read_at(range.at(0, width as u64)?, &mut bytes[..width])?;
        values[index] = if width == 2 { u16le(&bytes, 0) as u32 } else { u32le(&bytes, 0) };
    }
    require(values[2] == 0, "invalid-pe", "export ordinal")?;
    let target = values[0] as u64;
    require(
        target != 0 && !(export_rva..export_rva + export_size).contains(&target),
        "invalid-pe",
        "forwarded/missing ABI",
    )?;
    offset(sections, target, 1, input.len())?;
    let rva = values[1] as u64;
    Ok(NameReference { raw: offset(sections, rva, 1, input.len())?, rva, load: None })
}

/// Exact legacy ABI evidence, used only for cached schema-1 packages.
pub fn inspect(input: &mut impl ReadAt, architecture: &str) -> Result<Vec<Import>> {
    inspect_expected(input, architecture, "KwilPreflightV1")
}
pub fn inspect_v2(input: &mut impl ReadAt, architecture: &str) -> Result<Vec<Import>> {
    inspect_expected(input, architecture, "KwilPreflightV2")
}
fn inspect_expected(input: &mut impl ReadAt, architecture: &str, expected_export: &str) -> Result<Vec<Import>> {
    require(input.len() > 0 && input.len() <= 16_777_216, "limit", "guard Binary size")?;
    let dos = read::<64>(input, 0)?;
    require(&dos[..2] == b"MZ", "invalid-pe", "DOS signature")?;
    let pe = u32le(&dos, 60) as u64;
    let coff = read::<24>(input, pe)?;
    require(&coff[..4] == b"PE\0\0", "invalid-pe", "PE signature")?;
    let machine = match architecture {
        "x64" => 0x8664,
        "arm64" => 0xaa64,
        _ => return Err(Refusal::new("architecture-conflict", "PE architecture")),
    };
    require(
        u16le(&coff, 4) == machine && u16le(&coff, 22) & 0x2000 != 0,
        "invalid-pe",
        "guard machine/DLL characteristic",
    )?;
    let count = u16le(&coff, 6) as usize;
    // Windows' PE loader supports at most 96 sections.
    require(count > 0 && count <= 96 && u16le(&coff, 20) >= 240, "invalid-pe", "section count/optional header")?;
    let optional_range = file_range(pe + 24, u16le(&coff, 20) as u64, input.len())?;
    let optional = read::<240>(input, optional_range.at(0, 240)?)?;
    require(u16le(&optional, 0) == 0x20b && u32le(&optional, 108) >= 14, "invalid-pe", "PE32+ directories")?;
    let image_base = u64le(&optional, 24);
    let section_table = pe + 24 + u16le(&coff, 20) as u64;
    let section_bytes =
        (count as u64).checked_mul(40).ok_or_else(|| Refusal::new("invalid-pe", "section table size"))?;
    let section_range = file_range(section_table, section_bytes, input.len())?;
    let mut sections = Vec::with_capacity(count);
    for i in 0..count {
        let s = read::<40>(input, section_range.at((i as u64) * 40, 40)?)?;
        let section = Section {
            rva: u32le(&s, 12) as u64,
            backed: u32le(&s, 8).min(u32le(&s, 16)) as u64,
            raw: u32le(&s, 20) as u64,
        };
        require(
            section.raw.checked_add(section.backed).is_some_and(|n| n <= input.len()),
            "invalid-pe",
            "section raw bounds",
        )?;
        sections.push(section);
    }
    // Fixed-table audit: all RVA-backed walks below use complete rva_range
    // admissions (normal/delay directories, export directory, EAT/name-pointer/
    // ordinal arrays). Header/section-table walks use complete file_range spans.
    // Strings retain their separately bounded single-section admission/cache;
    // thunk/IAT tables are not traversed by this DLL-name evidence parser.
    let mut directories = Vec::with_capacity(2);
    for (directory, record_size, load) in [(1, 20, "N"), (13, 32, "D")] {
        let rva = u32le(&optional, 112 + directory * 8) as u64;
        let size = u32le(&optional, 116 + directory * 8) as u64;
        if rva == 0 && size == 0 {
            continue;
        }
        require(rva != 0 && size >= record_size, "invalid-pe", "import directory")?;
        // The entire declared normal/delay directory must have ONE physical
        // mapping. Mapping only the first descriptor admits replaying layouts.
        directories.push((rva_range(&sections, rva, size, input.len())?, record_size, load));
    }
    directories.sort_unstable_by_key(|directory| directory.0.raw);
    // Admit both import directories before any RVA metadata I/O. The only
    // retained descriptor data are <=128 names plus the one ABI reference.
    let mut names = Vec::with_capacity(129);
    names.push(export_name_reference(input, &sections, &optional)?);
    let mut import_count = 0;
    for (range, record_size, load) in directories {
        let mut terminated = false;
        // 128 imports plus one explicit null descriptor. Never scan an
        // attacker-sized directory up to its arbitrary declared byte count.
        for i in 0..=128u64 {
            require((i + 1) * record_size <= range.size, "invalid-pe", "unterminated import directory")?;
            let at = range.at(i * record_size, record_size)?;
            let mut descriptor = [0u8; 32];
            input.read_at(at, &mut descriptor[..record_size as usize])?;
            if descriptor[..record_size as usize].iter().all(|b| *b == 0) {
                terminated = true;
                break;
            }
            require(import_count < 128, "limit", "normal/delay imports")?;
            let mut name_rva = u32le(&descriptor, if load == "N" { 12 } else { 4 }) as u64;
            if load == "D" {
                require(u32le(&descriptor, 0) & !1 == 0, "invalid-pe", "delay descriptor attributes")?;
                if u32le(&descriptor, 0) & 1 == 0 {
                    name_rva =
                        name_rva.checked_sub(image_base).ok_or_else(|| Refusal::new("invalid-pe", "delay VA"))?;
                }
            }
            names.push(NameReference {
                raw: offset(&sections, name_rva, 1, input.len())?,
                rva: name_rva,
                load: Some(load),
            });
            import_count += 1;
        }
        require(terminated, "limit", "import directory terminator")?;
    }
    names.sort_unstable_by_key(|reference| reference.raw);
    let mut imports = Vec::with_capacity(import_count);
    let mut previous = None;
    for reference in names {
        let text = name(input, &sections, reference.rva, &mut previous)?;
        if let Some(load) = reference.load {
            let import = Import { load: load.to_owned(), dll: text.to_ascii_lowercase() };
            // Validate EACH physical descriptor's name/runtime policy before
            // canonical set deduplication. The descriptor cap above is unchanged.
            validate_imports(std::slice::from_ref(&import))?;
            imports.push(import);
        } else {
            require(text == expected_export, "invalid-pe", "ABI export name")?;
        }
    }
    imports.sort();
    // Repeated linker descriptors (including case aliases) are one ledger pair;
    // normal and delay remain distinct. Metadata validation still rejects duplicates.
    imports.dedup();
    validate_imports(&imports)?;
    Ok(imports)
}
