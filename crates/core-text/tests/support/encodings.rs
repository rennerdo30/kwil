use kwil_core_text::encoding::{DisplaySegment, EncodedFile, Encoding, EncodingError, SavePlan};
use kwil_core_text::{Buffer, TextFile};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

// All supported canonical codecs, rather than aliases of the same codec.
pub const LABELS: &[&str] = &[
    "utf-8",
    "utf-16le",
    "utf-16be",
    "utf-32le",
    "utf-32be",
    "ibm866",
    "iso-8859-2",
    "iso-8859-3",
    "iso-8859-4",
    "iso-8859-5",
    "iso-8859-6",
    "iso-8859-7",
    "iso-8859-8",
    "iso-8859-8-i",
    "iso-8859-10",
    "iso-8859-13",
    "iso-8859-14",
    "iso-8859-15",
    "iso-8859-16",
    "koi8-r",
    "koi8-u",
    "macintosh",
    "windows-874",
    "windows-1250",
    "windows-1251",
    "windows-1252",
    "windows-1253",
    "windows-1254",
    "windows-1255",
    "windows-1256",
    "windows-1257",
    "windows-1258",
    "x-mac-cyrillic",
    "gbk",
    "gb18030",
    "big5",
    "euc-jp",
    "shift_jis",
    "euc-kr",
    "x-user-defined",
];

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kwil-property-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // create_new ensures no existing file is overwritten.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        file.write_all(bytes).unwrap();
        Self(path)
    }
    fn open(&self) -> TextFile {
        TextFile::open(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

// Independent UTF-16/32 oracle: std scalar/unit conversion, not core-text's codec.
fn oracle(text: &str, label: &str) -> Option<Vec<u8>> {
    match label {
        "utf-16le" => Some(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
        "utf-16be" => Some(text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
        "utf-32le" => Some(text.chars().flat_map(|c| (c as u32).to_le_bytes()).collect()),
        "utf-32be" => Some(text.chars().flat_map(|c| (c as u32).to_be_bytes()).collect()),
        _ => {
            let codec = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap();
            let (bytes, _, errors) = codec.encode(text);
            (!errors).then(|| bytes.into_owned())
        }
    }
}

pub fn representable_text(bytes: &[u8], label: &str) -> String {
    let decoded = if label.starts_with("utf-") {
        String::from_utf8_lossy(bytes)
    } else {
        encoding_rs::Encoding::for_label(label.as_bytes()).unwrap().decode_without_bom_handling(bytes).0
    };
    // Exercise successful non-ASCII legacy cases instead of only rejection.
    let mut text = String::from("K"); // Disambiguate a leading U+FEFF from a file BOM.
    text.extend(decoded.chars().filter(|c| oracle(&c.to_string(), label).is_some()));
    text
}

pub fn roundtrip(text: &str, label: &str, bom: bool) {
    let encoding = Encoding::for_label(label).unwrap();
    let bom = bom && !encoding.bom_bytes().is_empty();
    let empty = Fixture::new(b"");
    let source = EncodedFile::new_with_encoding(empty.open(), Encoding::UTF8).unwrap();
    let mut buffer = Buffer::empty();
    // Split additions even inside UTF-8 scalars to exercise streaming input.
    for chunk in text.as_bytes().chunks(3) {
        buffer.insert(buffer.len(), chunk).unwrap();
    }
    let plan = SavePlan { encoding, bom, line_ending: None };
    let mut saved = Vec::new();
    let expected = oracle(text, label);
    let result = source.write_to(&buffer.snapshot(), plan, &mut saved);
    let Some(payload) = expected else {
        assert!(matches!(result, Err(EncodingError::Unmappable { .. })), "{label}: {result:?}");
        assert!(saved.is_empty(), "unmappable output must be rejected before writing");
        return;
    };
    result.unwrap();
    let mut expected = if bom { encoding.bom_bytes().to_vec() } else { Vec::new() };
    expected.extend(payload);
    assert_eq!(saved, expected, "encode {label}");
    let fixture = Fixture::new(&saved);
    let file = fixture.open();
    let decoded = EncodedFile::new_with_encoding(file.clone(), encoding).unwrap();
    decoded.build_index().unwrap();
    let range = decoded.decode_range(0..saved.len() as u64).unwrap();
    assert!(!range.had_errors, "decode {label}");
    let actual: String = range
        .segments
        .iter()
        .map(|segment| match segment {
            DisplaySegment::Text(text) => text.as_ref(),
            DisplaySegment::Invalid(_) => panic!("invalid bytes in valid {label}"),
        })
        .collect();
    // Some legacy codecs canonicalize aliases (e.g. Shift_JIS yen/backslash).
    let canonical = match label {
        "utf-16le" | "utf-16be" | "utf-32le" | "utf-32be" => text.to_owned(),
        _ => encoding_rs::Encoding::for_label(label.as_bytes())
            .unwrap()
            .decode_without_bom_handling(&expected[if bom { encoding.bom_bytes().len() } else { 0 }..])
            .0
            .into_owned(),
    };
    assert_eq!(actual, canonical, "decode {label}");
    let raw = Buffer::from_file(file).unwrap();
    let mut preserved = Vec::new();
    decoded.write_to(&raw.snapshot(), decoded.save_plan(), &mut preserved).unwrap();
    assert_eq!(preserved, saved, "untouched bytes {label}");
    // Convert the decoded original through the save path as well.
    let mut utf8 = Vec::new();
    decoded
        .write_to(&raw.snapshot(), SavePlan { encoding: Encoding::UTF8, bom: false, line_ending: None }, &mut utf8)
        .unwrap();
    assert_eq!(utf8, canonical.as_bytes(), "transcode {label}");
}

pub fn preserve_utf8(bytes: &[u8]) {
    let fixture = Fixture::new(bytes);
    let file = fixture.open();
    let encoded = EncodedFile::new_with_encoding(file.clone(), Encoding::UTF8).unwrap();
    let buffer = Buffer::from_file(file).unwrap();
    let mut saved = Vec::new();
    encoded.write_to(&buffer.snapshot(), encoded.save_plan(), &mut saved).unwrap();
    assert_eq!(saved, bytes, "malformed UTF-8 must survive untouched saves");
}
