use super::*;
#[test]
fn cache_failed_publication_keeps_previous_bytes_contract() {
    let root = std::env::temp_dir().join(format!(
        "kwil-theme-write-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("active.bin");
    fs::write(&path, b"previous valid cache").unwrap();
    let result = atomic_write_with(&path, b"replacement", |_, _| {
        Err(std::io::Error::other("fixture atomic replacement failure"))
    });
    assert!(result.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"previous valid cache");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "exclusive temp must be removed on failed publication");
    atomic_write(&path, b"new complete cache").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new complete cache");
    assert!(atomic_write_new(&path, b"must not overwrite").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"new complete cache");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_structural_validation_survives_valid_checksum_contract() {
    let cached = CachedTheme { selection: ThemeSelection::Named(ThemeId::Dark), theme: Theme::builtin(Scheme::Dark) };
    let original = encode(&cached).unwrap();
    let rehash = |bytes: &mut Vec<u8>| {
        let len = bytes.len() - HEADER;
        bytes[10..14].copy_from_slice(&(len as u32).to_le_bytes());
        let check = hash(&bytes[HEADER..]);
        bytes[14..22].copy_from_slice(&check.to_le_bytes());
    };
    // Unlike random corruption, these payloads pass the integrity checksum;
    // only the structural decoder can reject them.
    let mut bad_tag = original.clone();
    bad_tag[HEADER] = 255;
    rehash(&mut bad_tag);
    assert!(decode(&bad_tag).is_err());
    let mut long_name = original.clone();
    long_name[HEADER + 2..HEADER + 4].copy_from_slice(&129u16.to_le_bytes());
    rehash(&mut long_name);
    assert!(decode(&long_name).is_err());
    let mut trailing = original.clone();
    trailing.push(0);
    rehash(&mut trailing);
    assert!(decode(&trailing).is_err());
    let mut too_many_scopes = original.clone();
    let count_offset = HEADER + 2 + 2 + cached.theme.name.len() + 1 + UI_ROLE_COUNT * 4;
    too_many_scopes[count_offset..count_offset + 2].copy_from_slice(&((MAX_SCOPES + 1) as u16).to_le_bytes());
    rehash(&mut too_many_scopes);
    assert!(decode(&too_many_scopes).is_err());
}
