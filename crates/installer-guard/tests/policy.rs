//! Portable policy/format fixtures. AUTHORED, NOT RUN in the source-only task.
//! Included as unit tests because the production target is cdylib only.
use crate::{
    identity::*,
    model::*,
    policy::*,
    tables::{self, Database, Row},
    *,
};
use std::collections::BTreeMap;

/// ROOT supplies both variables in the explicitly selected test process. The
/// valid fixture is the synthetic backend x64 MSI; the second is its copy with
/// an actual Component.Attributes cell changed from 260 to 256. Neither PE runs.
#[cfg(windows)]
#[test]
#[ignore = "ROOT only: set KWIL_INSTALLER_GUARD_VALID_MSI and KWIL_INSTALLER_GUARD_TAMPERED_MSI to absolute MSI paths"]
fn windows_msi_database_x64_pairing() {
    fn required_path(name: &str) -> std::path::PathBuf {
        std::env::var_os(name)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("explicit ignored test requires process-local {name}"))
    }
    let valid = required_path("KWIL_INSTALLER_GUARD_VALID_MSI");
    let tampered = required_path("KWIL_INSTALLER_GUARD_TAMPERED_MSI");
    assert_ne!(valid, tampered, "the tamper oracle must be a separate MSI");
    let package = crate::windows::validate_database_fixture(&valid)
        .expect("backend x64 MSI must pass the real guard database parser");
    assert_eq!(package.architecture, "x64");
    let error =
        crate::windows::validate_database_fixture(&tampered).expect_err("actual Component.Attributes=256 must refuse");
    assert_eq!(error.code, "standard-table-mismatch");
    assert_eq!(error.resource, "Component");
}

#[test]
fn actual_adapter_token_seam_requests_exact_known_folder_rights_for_both_candidates() {
    // This is the same seam that supplies desired access to OpenProcessToken
    // and OpenThreadToken, independently of the resource InventoryApi fixture.
    // Regressing its mask to TOKEN_QUERY (0x0008) fails both the exact-mask
    // assertion and the non-null known-folder token requirement below.
    fn known_folder(token: (&str, u32)) -> Result<()> {
        require(token.1 & 0x000c == 0x000c, "token-context", "known-folder token lacks QUERY or IMPERSONATE")
    }
    assert!(known_folder(("query-only", 0x0008)).is_err());
    for has_thread in [true, false] {
        let calls = std::cell::RefCell::new(Vec::new());
        let token = crate::token_access::effective_token(
            |access| {
                calls.borrow_mut().push(("process", access));
                Ok(("process", access))
            },
            |access| {
                calls.borrow_mut().push(("thread", access));
                Ok(has_thread.then_some(("thread", access)))
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec![("process", 0x000c), ("thread", 0x000c)]);
        assert_eq!(token.0, if has_thread { "thread" } else { "process" });
        known_folder(token).unwrap();
    }
}

#[test]
fn actual_adapter_token_seam_never_falls_back_after_a_thread_access_error() {
    let opened = std::cell::Cell::new(0);
    let error = crate::token_access::effective_token(
        |access| {
            assert_eq!(access, 0x000c);
            opened.set(opened.get() + 1);
            Ok("process")
        },
        |access| {
            assert_eq!(access, 0x000c);
            opened.set(opened.get() + 1);
            Err(Refusal::new("token-context", "thread access denied"))
        },
    )
    .unwrap_err();
    assert_eq!(opened.get(), 2);
    assert_eq!(error.code, "token-context");
    assert_eq!(error.resource, "thread access denied");
}

fn put16(b: &mut [u8], at: usize, n: u16) {
    b[at..at + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}
fn pe_fixture(delay: Option<&str>) -> Vec<u8> {
    let mut b = vec![0; 4096];
    b[..2].copy_from_slice(b"MZ");
    put32(&mut b, 60, 128);
    b[128..132].copy_from_slice(b"PE\0\0");
    put16(&mut b, 132, 0x8664);
    put16(&mut b, 134, 1);
    put16(&mut b, 148, 240);
    put16(&mut b, 150, 0x2000);
    put16(&mut b, 152, 0x20b);
    put32(&mut b, 260, 16);
    put32(&mut b, 264, 0x1200);
    put32(&mut b, 268, 0x100); // exports
    put32(&mut b, 272, 0x1100);
    put32(&mut b, 276, 40); // normal imports + null
    put32(&mut b, 400, 0xe00);
    put32(&mut b, 404, 0x1000);
    put32(&mut b, 408, 0xe00);
    put32(&mut b, 412, 0x200);
    put32(&mut b, 0x30c, 0x1180);
    b[0x380..0x38d].copy_from_slice(b"KERNEL32.dll\0");
    put32(&mut b, 0x414, 1);
    put32(&mut b, 0x418, 1);
    put32(&mut b, 0x41c, 0x1280);
    put32(&mut b, 0x420, 0x1284);
    put32(&mut b, 0x424, 0x1288);
    put32(&mut b, 0x480, 0x1300);
    put32(&mut b, 0x484, 0x12a0);
    b[0x4a0..0x4b0].copy_from_slice(b"KwilPreflightV1\0");
    b[0x500] = 0xc3;
    if let Some(dll) = delay {
        put32(&mut b, 368, 0x1400);
        put32(&mut b, 372, 64);
        put32(&mut b, 0x600, 1);
        put32(&mut b, 0x604, 0x1480);
        b[0x680..0x680 + dll.len()].copy_from_slice(dll.as_bytes());
    }
    b
}
struct Bytes(Vec<u8>);
impl pe::ReadAt for Bytes {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, target: &mut [u8]) -> Result<()> {
        require(target.len() <= 65536, "test-window", "window too large")?;
        let bytes = self
            .0
            .get(offset as usize..offset as usize + target.len())
            .ok_or_else(|| Refusal::new("invalid-pe", "fixture bounds"))?;
        target.copy_from_slice(bytes);
        Ok(())
    }
}
fn package(ver: &str) -> Package {
    let mut supplied = Vec::new();
    for (root, name) in REQUIRED_FILES.into_iter().map(|n| ("I", n)).chain(std::iter::once(("P", "Kwil.lnk"))) {
        let data = if root == "P" {
            shortcut().to_vec()
        } else if name == "kwil.no-update" {
            NO_UPDATE.to_vec()
        } else {
            name.as_bytes().to_vec()
        };
        let mut row = Resource(std::array::from_fn(|_| "-".to_owned()));
        row.0[1] = "F".into();
        row.0[2] = root.into();
        row.0[3] = name.into();
        row.0[6] = data.len().to_string();
        row.0[7] = sha256(&data);
        supplied.push(row);
    }
    let resources = reconstruct("x64", &supplied).unwrap();
    let pe = pe_fixture(None);
    let imports = pe::inspect(&mut Bytes(pe.clone()), "x64").unwrap();
    let mut p = Package {
        architecture: "x64".into(),
        msi_version: ver.into(),
        release: None,
        commit: "a".repeat(40),
        target: "x86_64-pc-windows-msvc".into(),
        guard_hash: sha256(&pe),
        inventory_hash: sha256(&inventory_bytes(&resources).unwrap()),
        build_id: String::new(),
        product_code: String::new(),
        package_code: "{11111111-2222-4333-8444-555555555555}".into(),
        imports,
        resources,
    };
    p.build_id = sha256(&p.build_bytes().unwrap());
    p.product_code = product(&p.architecture, &p.msi_version, &p.build_id).unwrap();
    p
}
fn install_session() -> Session {
    Session { action: "INSTALL".into(), main_action: 3, reinstall_mode: "omus".into(), ..Session::default() }
}
fn repair_session() -> Session {
    Session { installed: true, reinstall: "ALL".into(), reinstall_mode: "amus".into(), ..install_session() }
}
fn uninstall_session() -> Session {
    Session { installed: true, remove: "ALL".into(), main_action: 2, ..install_session() }
}

#[test]
fn rfc_uuid5_network_order_vector() {
    assert_eq!(
        uuid5("{6BA7B810-9DAD-11D1-80B4-00C04FD430C8}", b"www.widgets.com").unwrap(),
        "{21F7F8DE-8051-5B89-8680-0195EF798B6A}"
    );
    assert!(guid_bytes("{6ba7b810-9dad-11d1-80b4-00c04fd430c8}").is_err());
}
#[test]
fn canonical_decimal_and_msi_version_refuse_overflow_and_aliases() {
    for s in ["", "00", "+1", "-1", " 1", "1 ", "18446744073709551616"] {
        assert!(decimal(s, u64::MAX).is_err(), "{s}");
    }
    for s in ["1.2", "1.2.3.4", "01.2.3", "256.0.0", "0.256.0", "0.0.65536", "1.2.3+abc", "1.2.3-rc1"] {
        assert!(version(s).is_err(), "{s}");
    }
    assert_eq!(version("255.255.65535").unwrap(), [255, 255, 65535]);
}
#[test]
fn exact_schema_model_and_hashes_reject_every_single_cell_mutation() {
    let p = package("1.2.3");
    p.validate().unwrap();
    assert_eq!(p.resources.iter().filter(|r| r.kind() == "F").count(), 8);
    assert_eq!(p.resources.iter().filter(|r| r.kind() == "R").count(), 11);
    assert_eq!(p.resources.iter().filter(|r| r.kind() == "K").count(), 10);
    for row in 0..29 {
        for column in 0..13 {
            let mut broken = p.clone();
            broken.resources[row].0[column].push('x');
            assert!(broken.validate().is_err(), "row {row} column {column}");
        }
    }
    let mut duplicate = p.clone();
    duplicate.resources[28] = duplicate.resources[27].clone();
    assert!(duplicate.validate().is_err());
}
#[test]
fn exact_marker_and_shortcut_are_not_arbitrary_payload_hashes() {
    let p = package("1.2.3");
    for name in ["kwil.no-update", "Kwil.lnk"] {
        let mut broken = p.clone();
        broken.resources.iter_mut().find(|r| r.relative() == name).unwrap().0[7] = "0".repeat(64);
        // Rehashing the inventory cannot legitimize a different fixed resource.
        broken.inventory_hash = sha256(&inventory_bytes(&broken.resources).unwrap());
        broken.build_id = sha256(&broken.build_bytes().unwrap());
        broken.product_code = product("x64", &broken.msi_version, &broken.build_id).unwrap();
        assert!(broken.validate().is_err());
    }
    let link = shortcut();
    assert_eq!(&link[0..4], &76u32.to_le_bytes());
    assert_eq!(&link[76..80], &788u32.to_le_bytes());
    assert_eq!(&link[864..], &[0; 4]);
    assert_eq!(&link[20..24], &0x02000200u32.to_le_bytes());
}
#[test]
fn only_the_exact_owner_family_literal_permits_braces() {
    let p = package("1.2.3");
    let owner = p.resources.iter().find(|r| r.name() == "n:Owner").unwrap();
    let bytes = registry_bytes(owner, "E:\\Local\\Programs\\Kwil\\").unwrap();
    let expected = owner.data().encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    assert_eq!(bytes, expected);
    assert!(owner.data().starts_with(FAMILY));
    for suffix in ["{other}", "$other", "${I}", "}"] {
        let mut bad = p.clone();
        bad.resources.iter_mut().find(|r| r.name() == "n:Owner").unwrap().0[5].push_str(suffix);
        assert!(bad.validate().is_err());
    }
    let mut bad = p.clone();
    bad.resources.iter_mut().find(|r| r.data() == "Open with Kwil").unwrap().0[5] = "Open with {Kwil}".into();
    assert!(bad.validate().is_err());
}
#[test]
fn packaging_random_code_does_not_create_a_guard_build_identity_cycle() {
    let p = package("1.2.3");
    let mut repack = p.clone();
    repack.package_code = "{AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE}".into();
    repack.validate().unwrap();
    assert_eq!(p.build_bytes().unwrap(), repack.build_bytes().unwrap());
    assert_eq!(p.product_code, repack.product_code);
    assert_eq!(operation(&repack, Some(&p), &repair_session()).unwrap_err().code, "package-mismatch");
    assert_eq!(operation(&repack, Some(&p), &uninstall_session()).unwrap_err().code, "package-mismatch");
}
#[test]
fn install_upgrade_repair_uninstall_authority_is_independent_of_session_claims() {
    let p = package("1.2.3");
    let newer = package("1.2.4");
    let s = install_session();
    assert_eq!(operation(&p, None, &s).unwrap(), Operation::Install);
    assert_eq!(operation(&newer, Some(&p), &s).unwrap(), Operation::Upgrade);
    assert_eq!(operation(&p, Some(&p), &repair_session()).unwrap(), Operation::Repair);
    assert_eq!(operation(&p, Some(&p), &uninstall_session()).unwrap(), Operation::Uninstall);
    assert!(operation(&p, None, &uninstall_session()).is_err());
    let mut fake = s.clone();
    fake.installed = true;
    assert!(operation(&p, None, &fake).is_err());
    let mut normal_i = s.clone();
    normal_i.installed = true;
    assert_eq!(operation(&p, Some(&p), &normal_i).unwrap_err().code, "repair-required");
    let mut fake_state = repair_session();
    fake_state.main_action = 1;
    assert!(operation(&p, Some(&p), &fake_state).is_err());
    let mut fake_action = repair_session();
    fake_action.action = "ADMIN".into();
    assert!(operation(&p, Some(&p), &fake_action).is_err());
}
#[test]
fn r2_omus_grants_no_repair_authority_and_other_modes_refuse() {
    let p = package("1.2.3");
    for mode in ["", "omus"] {
        let mut s = install_session();
        s.reinstall_mode = mode.into();
        assert!(operation(&p, None, &s).is_ok());
        let mut s = uninstall_session();
        s.reinstall_mode = mode.into();
        assert!(operation(&p, Some(&p), &s).is_ok());
    }
    for mode in ["", "omus", "AMUS", "amus ", "a", "vamus"] {
        let mut s = repair_session();
        s.reinstall_mode = mode.into();
        assert!(operation(&p, Some(&p), &s).is_err());
    }
    for mode in ["amus", "OMUS", "vamus"] {
        let mut s = install_session();
        s.reinstall_mode = mode.into();
        assert!(operation(&p, None, &s).is_err());
    }
}
#[test]
fn numeric_version_and_architecture_switches_refuse() {
    let p = package("1.10.0");
    let older = package("1.9.9");
    assert!(operation(&older, Some(&p), &install_session()).is_err());
    let mut equal = p.clone();
    equal.product_code = "{AAAAAAAA-BBBB-5CCC-8DDD-EEEEEEEEEEEE}".into();
    assert!(operation(&equal, Some(&p), &install_session()).is_err());
    let mut arm = package("1.11.0");
    arm.architecture = "arm64".into();
    assert!(operation(&arm, Some(&p), &install_session()).is_err());
}
#[test]
fn predecessor_detection_requires_exact_guid_and_maintenance_empty() {
    let p = package("1.2.3");
    let mut s = install_session();
    s.older = p.product_code.clone();
    detection(Operation::Upgrade, Some(&p), &s).unwrap();
    for value in [
        format!("{};{}", p.product_code, p.product_code),
        p.product_code.to_ascii_lowercase(),
        format!("{} ", p.product_code),
        String::new(),
    ] {
        s.older = value;
        assert!(detection(Operation::Upgrade, Some(&p), &s).is_err());
    }
    s.older = p.product_code.clone();
    assert!(detection(Operation::Uninstall, Some(&p), &s).is_err());
    s.older.clear();
    s.equal = p.product_code.clone();
    assert!(detection(Operation::Install, None, &s).is_err());
}
#[test]
fn effective_identity_feature_and_rollback_overrides_refuse() {
    let p = package("1.2.3");
    let clean: BTreeMap<_, _> = tables::properties(&p).into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    tables::effective_properties(&p, |key| Ok(clean.get(key).cloned().unwrap_or_default())).unwrap();
    for key in [
        "DISABLEROLLBACK",
        "RollbackDisabled",
        "ALLUSERS",
        "TRANSFORMS",
        "PATCH",
        "ADDLOCAL",
        "COMPADDLOCAL",
        "REBOOT",
        "KWIL_BUILD_ID",
        "ProductCode",
    ] {
        let mut values = clean.clone();
        values.insert(key.into(), "1".into());
        assert_eq!(
            tables::effective_properties(&p, |key| Ok(values.get(key).cloned().unwrap_or_default())).unwrap_err().code,
            "session-override",
            "{key}"
        );
    }
}

struct InventoryApi {
    seen: BTreeMap<String, Observation>,
    portable: bool,
    busy: bool,
    failure: Option<Refusal>,
    calls: Vec<String>,
}
impl InventoryApi {
    fn empty() -> Self {
        Self { seen: BTreeMap::new(), portable: false, busy: false, failure: None, calls: Vec::new() }
    }
    fn original(p: &Package) -> Self {
        let mut api = Self::empty();
        for r in &p.resources {
            let value = match r.kind() {
                "F" => Observation::File { size: decimal(r.size(), 536870912).unwrap(), sha256: r.hash().to_owned() },
                "K" => Observation::Key,
                "R" => Observation::Value {
                    value_type: 1,
                    bytes: registry_bytes(r, "E:\\Local\\Programs\\Kwil\\").unwrap(),
                },
                _ => panic!("fixture kind"),
            };
            api.seen.insert(r.identity(), value);
        }
        api
    }
}
impl Resources for InventoryApi {
    fn observe(&mut self, r: &Resource) -> Result<Observation> {
        self.calls.push(r.identity());
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(self.seen.get(&r.identity()).cloned().unwrap_or(Observation::Absent))
    }
    fn portable_marker_absent(&mut self) -> Result<bool> {
        Ok(!self.portable)
    }
    fn in_use(&mut self, _: &Package) -> Result<()> {
        require(!self.busy, "in-use", "fixture running pair")
    }
    fn install_root(&self) -> &str {
        "E:\\Local\\Programs\\Kwil\\"
    }
}
#[test]
fn every_preexisting_file_key_and_value_is_foreign_without_registration() {
    let p = package("1.2.3");
    let owned = InventoryApi::original(&p);
    for r in &p.resources {
        let mut api = InventoryApi::empty();
        api.seen.insert(r.identity(), owned.seen[&r.identity()].clone());
        assert_eq!(resources(&p, None, &mut api).unwrap_err().code, "foreign-resource", "{}", r.identity());
    }
}
#[test]
fn missing_owned_resources_are_repairable_and_changed_present_resources_refuse() {
    let p = package("1.2.3");
    resources(&p, Some(&p), &mut InventoryApi::empty()).unwrap();
    for r in p.resources.iter().filter(|r| r.kind() != "K") {
        let mut api = InventoryApi::original(&p);
        let bad = if r.kind() == "F" {
            Observation::File { size: 0, sha256: sha256(b"changed") }
        } else {
            Observation::Value { value_type: 1, bytes: vec![0, 0] }
        };
        api.seen.insert(r.identity(), bad);
        assert!(resources(&p, Some(&p), &mut api).is_err(), "{}", r.identity());
    }
    let value = p.resources.iter().find(|r| r.kind() == "R").unwrap();
    let mut api = InventoryApi::original(&p);
    api.seen.insert(
        value.identity(),
        Observation::Value { value_type: 2, bytes: registry_bytes(value, api.install_root()).unwrap() },
    );
    assert!(resources(&p, Some(&p), &mut api).is_err());
}
#[test]
fn upgrades_compare_original_bytes_and_new_claims_need_individual_absence() {
    let p = package("1.2.3");
    let mut next = package("1.2.4");
    let f = next.resources.iter_mut().find(|r| r.relative() == "kwil.exe").unwrap();
    f.0[7] = sha256(b"new version");
    resources(&next, Some(&p), &mut InventoryApi::original(&p)).unwrap();
    for kind in ["F", "K", "R"] {
        let mut prior = p.clone();
        let removed = prior.resources.iter().position(|r| r.kind() == kind).unwrap();
        let newly_claimed = prior.resources.remove(removed);
        let mut api = InventoryApi::original(&p);
        assert_eq!(
            resources(&next, Some(&prior), &mut api).unwrap_err().code,
            "foreign-resource",
            "{}",
            newly_claimed.identity()
        );
    }
}
#[test]
fn empty_foreign_values_and_empty_keys_still_collide() {
    let p = package("1.2.3");
    for (kind, value) in [("K", Observation::Key), ("R", Observation::Value { value_type: 1, bytes: vec![0, 0] })] {
        let row = p.resources.iter().find(|r| r.kind() == kind).unwrap();
        let mut api = InventoryApi::empty();
        api.seen.insert(row.identity(), value);
        assert_eq!(resources(&p, None, &mut api).unwrap_err().code, "foreign-resource");
    }
}
#[test]
fn changed_component_portable_marker_and_running_pair_are_not_bypasses() {
    let p = package("1.2.3");
    let mut next = package("1.2.4");
    next.resources[0].0[8].push('x');
    assert!(resources(&next, Some(&p), &mut InventoryApi::original(&p)).is_err());
    let mut api = InventoryApi::empty();
    api.portable = true;
    assert_eq!(resources(&p, None, &mut api).unwrap_err().code, "portable-marker");
    let mut api = InventoryApi::original(&p);
    api.busy = true;
    assert_eq!(resources(&p, Some(&p), &mut api).unwrap_err().code, "in-use");
}
#[test]
fn unknown_siblings_values_subkeys_settings_and_recovery_are_not_observed() {
    let p = package("1.2.3");
    let mut api = InventoryApi::original(&p);
    for identity in
        ["I:notes.txt", "H:software\\kwil:user-value", "H:software\\kwil\\unknown:#key", "settings", "recovery"]
    {
        api.seen.insert(identity.into(), Observation::Key);
    }
    resources(&p, Some(&p), &mut api).unwrap();
    assert!(api.calls.iter().all(|id| p.resources.iter().any(|r| r.identity() == *id)));
}
#[test]
fn missing_or_unstable_api_evidence_is_never_absence_or_success() {
    let p = package("1.2.3");
    for code in ["filesystem-api", "unsafe-path", "unsafe-registry", "unstable-evidence", "deadline", "limit"] {
        let mut api = InventoryApi::empty();
        api.failure = Some(Refusal::new(code, "injected observation failure"));
        assert_eq!(resources(&p, None, &mut api).unwrap_err().code, code);
        assert_eq!(resources(&p, Some(&p), &mut api).unwrap_err().code, code);
    }
    assert!(tables::effective_properties(&p, |_| Err(Refusal::new("msi-api", "property query failed"))).is_err());
}
#[test]
fn parsing_and_expansion_caps_are_rejections_before_large_reads() {
    struct Oversize;
    impl pe::ReadAt for Oversize {
        fn len(&self) -> u64 {
            16_777_217
        }
        fn read_at(&mut self, _: u64, _: &mut [u8]) -> Result<()> {
            panic!("oversize PE must refuse before reading")
        }
    }
    assert_eq!(pe::inspect(&mut Oversize, "x64").unwrap_err().code, "limit");
    let p = package("1.2.3");
    let mut rows = vec![p.resources[0].clone(); 513];
    assert!(inventory_bytes(&rows).is_err());
    rows.truncate(1);
    rows[0].0[5] = "X".repeat(1_048_576);
    assert!(inventory_bytes(&rows).is_err());
    let shell = p.resources.iter().find(|r| r.data().contains("${I}")).unwrap();
    assert_eq!(registry_bytes(shell, &format!("E:\\{}\\", "x".repeat(4096))).unwrap_err().code, "limit");
    let mut oversized = p.clone();
    oversized.resources.iter_mut().find(|r| r.relative() == "kwil.exe").unwrap().0[6] = "536870913".into();
    assert!(oversized.validate().is_err());
    let mut total = p.clone();
    for r in total
        .resources
        .iter_mut()
        .filter(|r| r.kind() == "F" && ["kwil.exe", "kwil.com", "LICENSE"].contains(&r.relative()))
    {
        r.0[6] = "536870912".into();
    }
    assert!(total.validate().is_err());
}
#[test]
fn pe_normal_delay_imports_machine_static_runtime_and_sole_export_are_checked() {
    assert_eq!(
        pe::inspect(&mut Bytes(pe_fixture(Some("user32.dll"))), "x64").unwrap(),
        vec![
            Import { load: "D".into(), dll: "user32.dll".into() },
            Import { load: "N".into(), dll: "kernel32.dll".into() }
        ]
    );
    assert!(pe::inspect(&mut Bytes(pe_fixture(None)), "arm64").is_err());
    for dll in ["vcruntime140.dll", "msvcp140.dll", "msvcr100.dll", "ucrtbase.dll", "api-ms-win-crt-stdio-l1-1-0.dll"] {
        assert!(pe::inspect(&mut Bytes(pe_fixture(Some(dll))), "x64").is_err(), "{dll}");
    }
    for (offset, value) in [(0x414, 2), (0x418, 0), (0x480, 0x1201), (0x484, 0xffffffff), (272, 0xffffffff)] {
        let mut b = pe_fixture(None);
        put32(&mut b, offset, value);
        assert!(pe::inspect(&mut Bytes(b), "x64").is_err(), "offset {offset}");
    }
    let mut b = pe_fixture(None);
    b[0x4a0] = b'X';
    assert!(pe::inspect(&mut Bytes(b), "x64").is_err());
    let mut b = pe_fixture(None);
    put32(&mut b, 276, 20);
    assert!(pe::inspect(&mut Bytes(b), "x64").is_err());
}

fn repeated_import_pe(count: usize, last: Option<&str>, delay: Option<&str>) -> Vec<u8> {
    let mut b = pe_fixture(delay);
    b.resize(0x2200, 0);
    put32(&mut b, 400, 0x2000);
    put32(&mut b, 408, 0x2000);
    put32(&mut b, 272, 0x1600);
    put32(&mut b, 276, ((count + 1) * 20) as u32);
    b[0x1800..0x180d].copy_from_slice(b"kernel32.dll\0");
    if let Some(text) = last {
        b[0x1900..0x1900 + text.len()].copy_from_slice(text.as_bytes());
    }
    for i in 0..count {
        let rva = if i == count - 1 && last.is_some() {
            0x2700
        } else if i % 2 == 0 {
            0x1180 // original uppercase name
        } else {
            0x2600 // distinct lowercase spelling and physical name
        };
        put32(&mut b, 0x800 + i * 20 + 12, rva);
    }
    b
}

#[test]
fn pe_physical_case_aliases_form_an_exact_canonical_set() {
    for (architecture, machine) in [("x64", 0x8664), ("arm64", 0xaa64)] {
        let mut b = repeated_import_pe(3, None, Some("kernel32.dll"));
        put16(&mut b, 132, machine);
        assert_eq!(
            pe::inspect(&mut Bytes(b), architecture).unwrap(),
            vec![
                Import { load: "D".into(), dll: "kernel32.dll".into() },
                Import { load: "N".into(), dll: "kernel32.dll".into() }
            ]
        );
    }
    let pair = Import { load: "N".into(), dll: "kernel32.dll".into() };
    // Only physical PE evidence is normalized; supplied ledger duplicates refuse.
    assert!(model::validate_imports(&[pair.clone(), pair]).is_err());
}

#[test]
fn pe_repeated_descriptors_cannot_bypass_the_physical_cap() {
    assert_eq!(
        pe::inspect(&mut Bytes(repeated_import_pe(128, None, None)), "x64").unwrap(),
        vec![Import { load: "N".into(), dll: "kernel32.dll".into() }]
    );
    for (count, delay) in [(129, None), (128, Some("kernel32.dll"))] {
        let error = pe::inspect(&mut Bytes(repeated_import_pe(count, None, delay)), "x64").unwrap_err();
        assert_eq!(error.code, "limit");
        assert_eq!(error.resource, "normal/delay imports");
    }
}

#[test]
fn pe_later_entries_after_duplicates_are_still_validated() {
    for (text, code, resource) in [
        ("kernel32.dll/", "invalid-metadata", "guard import"),
        ("UCRTBASE.dll", "dynamic-runtime", "ucrtbase.dll"),
        ("api-ms-win-crt-runtime-l1-1-0.dll", "dynamic-runtime", "api-ms-win-crt-runtime-l1-1-0.dll"),
    ] {
        let error = pe::inspect(&mut Bytes(repeated_import_pe(3, Some(text), None)), "x64").unwrap_err();
        assert_eq!(error.code, code, "{text}");
        assert_eq!(error.resource, resource, "{text}");
    }
    let mut b = repeated_import_pe(3, None, None);
    put32(&mut b, 0x800 + 2 * 20 + 12, 0xfffffff);
    assert_eq!(pe::inspect(&mut Bytes(b), "x64").unwrap_err().code, "invalid-pe");
}

type FixtureSchema = (Vec<(String, String)>, Vec<String>);

#[derive(Clone)]
struct DbFixture {
    rows: BTreeMap<String, (Vec<String>, Vec<Row>)>,
    schema: BTreeMap<String, FixtureSchema>,
    summary: BTreeMap<u32, String>,
    guard: Vec<u8>,
    icon: (u64, String),
}
impl Database for DbFixture {
    fn column_info(&self, table: &str) -> Result<Vec<(String, String)>> {
        if let Some((columns, _)) = self.schema.get(table) {
            return Ok(columns.clone());
        }
        let (columns, _) = self.rows.get(table).ok_or_else(|| Refusal::new("fixture-db", table))?;
        Ok(columns.iter().map(|name| (name.clone(), "S0".into())).collect())
    }
    fn tables(&self) -> Result<Vec<String>> {
        Ok(self.rows.keys().cloned().collect())
    }
    fn rows(&self, table: &str, columns: &[&str], cap: usize) -> Result<Vec<Row>> {
        let (actual, rows) = self.rows.get(table).ok_or_else(|| Refusal::new("fixture-db", table))?;
        require(
            actual.iter().map(String::as_str).eq(columns.iter().copied()) && rows.len() <= cap,
            "fixture-db",
            table,
        )?;
        Ok(rows.clone())
    }
    fn custom_schema(&self, table: &str, columns: &[(&str, &str)], keys: &[&str]) -> Result<()> {
        let (actual, primary) = self.schema.get(table).ok_or_else(|| Refusal::new("fixture-schema", table))?;
        require(
            actual.iter().map(|(n, t)| (n.as_str(), t.as_str())).eq(columns.iter().copied())
                && primary.iter().map(String::as_str).eq(keys.iter().copied()),
            "fixture-schema",
            table,
        )
    }
    fn summary(&self, key: u32) -> Result<String> {
        self.summary.get(&key).cloned().ok_or_else(|| Refusal::new("fixture-summary", key.to_string()))
    }
    fn stream_hash(&self, table: &str, key: &str, cap: u64) -> Result<(u64, String)> {
        let evidence = match (table, key) {
            ("Binary", "KwilPreflight") => (self.guard.len() as u64, sha256(&self.guard)),
            ("Icon", "KwilIcon") => self.icon.clone(),
            _ => return Err(Refusal::new("fixture-stream", key)),
        };
        require(evidence.0 <= cap, "limit", "fixture stream")?;
        Ok(evidence)
    }
    fn guard_evidence(&self, arch: &str, abi: u32) -> Result<Vec<Import>> {
        match abi {
            1 => pe::inspect(&mut Bytes(self.guard.clone()), arch),
            2 => pe::inspect_v2(&mut Bytes(self.guard.clone()), arch),
            _ => Err(Refusal::new("schema-unsupported", "fixture ABI")),
        }
    }
}
fn sql(values: &[Option<&str>]) -> Row {
    values.iter().map(|s| s.map(str::to_owned)).collect()
}
fn database(p: &Package) -> DbFixture {
    let package_columns: &[(&str, &str)] =
        if p.release.is_some() { &tables::PACKAGE_COLUMNS } else { &tables::LEGACY_PACKAGE_COLUMNS };
    let mut db = DbFixture {
        rows: BTreeMap::new(),
        schema: BTreeMap::new(),
        summary: [(7, "x64;1033".into()), (9, p.package_code.clone()), (14, "500".into()), (15, "10".into())].into(),
        guard: {
            let mut image = pe_fixture(None);
            if p.release.is_some() {
                image[0x4ae] = b'2';
            }
            image
        },
        icon: {
            let r = p.resources.iter().find(|r| r.relative() == "kwil.ico").unwrap();
            (decimal(r.size(), 536870912).unwrap(), r.hash().into())
        },
    };
    for (table, columns, keys) in [
        ("KwilPackage", package_columns, &["Id"][..]),
        ("KwilResource", tables::RESOURCE_COLUMNS.as_slice(), &["Id"][..]),
        ("KwilGuardImport", tables::IMPORT_COLUMNS.as_slice(), &["Load", "Dll"][..]),
        ("Binary", &[("Name", "s72"), ("Data", "v0")][..], &["Name"][..]),
        ("Icon", &[("Name", "s72"), ("Data", "v0")][..], &["Name"][..]),
    ] {
        db.schema.insert(
            table.into(),
            (
                columns.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
                keys.iter().map(|s| s.to_string()).collect(),
            ),
        );
    }
    let mut add = |table: &str, columns: &[&str], rows: Vec<Row>| {
        db.rows.insert(table.into(), (columns.iter().map(|s| s.to_string()).collect(), rows));
    };
    let schema = if p.release.is_some() { "2" } else { "1" };
    let mut package_values = vec![Some("1"), Some(schema), Some("1"), Some(FAMILY), Some(p.architecture.as_str())];
    if let Some(identity) = &p.release {
        package_values.extend([Some(identity.release_version.as_str()), Some(identity.channel.as_str())]);
    }
    package_values.extend([
        Some(p.msi_version.as_str()),
        Some(p.commit.as_str()),
        Some(p.target.as_str()),
        Some(schema),
        Some(p.guard_hash.as_str()),
        Some(p.inventory_hash.as_str()),
        Some(p.build_id.as_str()),
        Some(p.product_code.as_str()),
        Some(p.package_code.as_str()),
        Some("29"),
    ]);
    add("KwilPackage", &package_columns.iter().map(|(name, _)| *name).collect::<Vec<_>>(), vec![sql(&package_values)]);
    add(
        "KwilResource",
        &tables::RESOURCE_COLUMNS.map(|(n, _)| n),
        p.resources.iter().map(|r| r.0.iter().map(|s| Some(s.clone())).collect()).collect(),
    );
    add("KwilGuardImport", &["Load", "Dll"], p.imports.iter().map(|i| sql(&[Some(&i.load), Some(&i.dll)])).collect());
    add(
        "Property",
        &["Property", "Value"],
        tables::properties(p).iter().map(|(k, v)| sql(&[Some(k), Some(v)])).collect(),
    );
    add(
        "LaunchCondition",
        &["Condition", "Description"],
        vec![sql(&[Some("NOT RollbackDisabled AND NOT DISABLEROLLBACK"), Some("Enable rollback.")])],
    );
    add(
        "Directory",
        &["Directory", "Directory_Parent", "DefaultDir"],
        vec![
            sql(&[Some("TARGETDIR"), None, Some("SourceDir")]),
            sql(&[Some("LocalAppDataFolder"), Some("TARGETDIR"), Some(".")]),
            sql(&[Some("INSTALLPROGRAMS"), Some("LocalAppDataFolder"), Some("PROGRA~1|Programs")]),
            sql(&[Some("INSTALLDIR"), Some("INSTALLPROGRAMS"), Some("Kwil")]),
            sql(&[Some("ProgramMenuFolder"), Some("TARGETDIR"), Some(".")]),
        ],
    );
    add(
        "Feature",
        &["Feature", "Feature_Parent", "Title", "Description", "Display", "Level", "Directory_", "Attributes"],
        vec![sql(&[
            Some("Main"),
            None,
            Some("Kwil"),
            Some("Kwil"),
            Some("1"),
            Some("1"),
            Some("INSTALLDIR"),
            Some("0"),
        ])],
    );
    let files: Vec<_> = p.resources.iter().filter(|r| r.kind() == "F").collect();
    let values: Vec<_> = p.resources.iter().filter(|r| r.kind() == "R").collect();
    add(
        "File",
        &["File", "Component_", "FileName", "FileSize", "Version", "Language", "Attributes", "Sequence"],
        files
            .iter()
            .enumerate()
            .map(|(i, r)| {
                sql(&[
                    Some(r.file_key()),
                    Some(r.component()),
                    Some(&short_long(r)),
                    Some(r.size()),
                    None,
                    None,
                    Some("16384"),
                    Some(&(i + 1).to_string()),
                ])
            })
            .collect(),
    );
    add(
        "Registry",
        &["Registry", "Root", "Key", "Name", "Value", "Component_"],
        values
            .iter()
            .map(|r| {
                sql(&[
                    Some(r.registry_key()),
                    Some("1"),
                    Some(r.relative()),
                    if r.name() == "@" { None } else { Some(&r.name()[2..]) },
                    Some(&r.data().replace("${I}", "[INSTALLDIR]")),
                    Some(r.component()),
                ])
            })
            .collect(),
    );
    add(
        "Component",
        &["Component", "ComponentId", "Directory_", "Attributes", "Condition", "KeyPath"],
        values
            .iter()
            .map(|r| {
                let file = files.iter().find(|f| f.component() == r.component());
                let identity = file.map(|f| f.identity()).unwrap_or_else(|| r.identity());
                let (_, guid) = component(&p.architecture, if file.is_some() { "F" } else { "R" }, &identity).unwrap();
                sql(&[
                    Some(r.component()),
                    Some(&guid),
                    Some(if file.is_some_and(|f| f.root() == "P") { "ProgramMenuFolder" } else { "INSTALLDIR" }),
                    Some("260"),
                    None,
                    Some(r.registry_key()),
                ])
            })
            .collect(),
    );
    add(
        "FeatureComponents",
        &["Feature_", "Component_"],
        values.iter().map(|r| sql(&[Some("Main"), Some(r.component())])).collect(),
    );
    add(
        "RemoveFile",
        &["FileKey", "Component_", "FileName", "DirProperty", "InstallMode"],
        vec![sql(&[Some("RemoveInstallDir"), Some(files[0].component()), None, Some("INSTALLDIR"), Some("2")])],
    );
    add(
        "Media",
        &["DiskId", "LastSequence", "DiskPrompt", "Cabinet", "VolumeLabel", "Source"],
        vec![sql(&[Some("1"), Some("8"), None, Some("#kwil.cab"), None, None])],
    );
    add("Binary", &["Name"], vec![sql(&[Some("KwilPreflight")])]);
    add("Icon", &["Name"], vec![sql(&[Some("KwilIcon")])]);
    add(
        "CustomAction",
        &["Action", "Type", "Source", "Target"],
        vec![
            sql(&[Some("KwilPreflight"), Some("1"), Some("KwilPreflight"), Some(p.guard_export())]),
            sql(&[Some("SetKwilArpLocation"), Some("51"), Some("ARPINSTALLLOCATION"), Some("[INSTALLDIR]")]),
        ],
    );
    add(
        "Upgrade",
        &["UpgradeCode", "VersionMin", "VersionMax", "Language", "Attributes", "Remove", "ActionProperty"],
        vec![
            sql(&[Some(FAMILY), None, Some(&p.msi_version), None, Some("1"), Some("ALL"), Some("KWIL_OLDER")]),
            sql(&[
                Some(FAMILY),
                Some(&p.msi_version),
                Some(&p.msi_version),
                None,
                Some("770"),
                None,
                Some("KWIL_EQUAL"),
            ]),
            sql(&[Some(FAMILY), Some(&p.msi_version), None, None, Some("2"), None, Some("KWIL_NEWER")]),
        ],
    );
    add("Error", &["Error", "Message"], vec![sql(&[Some("25001"), Some("Kwil installer preflight: [2]. [3]")])]);
    let actions = [
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("SetKwilArpLocation", 1010),
        ("MigrateFeatureStates", 1200),
        ("InstallValidate", 1400),
        ("KwilPreflight", 1490),
        ("InstallInitialize", 1500),
        ("RemoveExistingProducts", 1501),
        ("ProcessComponents", 1600),
        ("UnpublishFeatures", 1800),
        ("RemoveRegistryValues", 2600),
        ("RemoveFiles", 3500),
        ("InstallFiles", 4000),
        ("WriteRegistryValues", 5000),
        ("RegisterUser", 6000),
        ("RegisterProduct", 6100),
        ("PublishFeatures", 6300),
        ("PublishProduct", 6400),
        ("InstallFinalize", 6600),
    ];
    add(
        "InstallExecuteSequence",
        &["Action", "Condition", "Sequence"],
        actions
            .iter()
            .map(|(a, n)| {
                sql(&[
                    Some(a),
                    if ["KwilPreflight", "SetKwilArpLocation"].contains(a) { Some("1") } else { None },
                    Some(&n.to_string()),
                ])
            })
            .collect(),
    );
    add(
        "_Validation",
        &[
            "Table",
            "Column",
            "Nullable",
            "MinValue",
            "MaxValue",
            "KeyTable",
            "KeyColumn",
            "Category",
            "Set",
            "Description",
        ],
        vec![],
    );
    let mut declarations = Vec::new();
    for table in db.rows.keys() {
        for (column, ty) in db.column_info(table).unwrap() {
            declarations.push(sql(&[
                Some(table),
                Some(&column),
                Some(if ty.as_bytes()[0].is_ascii_uppercase() { "Y" } else { "N" }),
                None,
                None,
                None,
                None,
                Some("Text"),
                None,
                None,
            ]));
        }
    }
    db.rows.get_mut("_Validation").unwrap().1 = declarations;
    db
}
#[test]
fn typed_database_stream_summary_and_standard_table_tampering_refuse() {
    let p = package("1.2.3");
    let clean = database(&p);
    tables::read_legacy_cached(&clean).unwrap();
    for (table, column, value) in [
        ("Component", 3, "4"),
        ("Component", 5, "F000001"),
        ("Registry", 1, "2"),
        ("Registry", 4, "#bad"),
        ("File", 2, "kwil.exe"),
        ("Directory", 2, "Elsewhere"),
        ("CustomAction", 1, "65"),
        ("Upgrade", 4, "2"),
        ("Feature", 5, "0"),
        ("LaunchCondition", 0, "1"),
        ("RemoveFile", 2, "*"),
    ] {
        let mut broken = clean.clone();
        broken.rows.get_mut(table).unwrap().1[0][column] = Some(value.into());
        assert!(tables::read_legacy_cached(&broken).is_err(), "{table} {column}");
    }
    let mut broken = clean.clone();
    broken.summary.insert(9, "{AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE}".into());
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.guard[0x500] ^= 1;
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.schema.get_mut("KwilResource").unwrap().0[5].1 = "S0".into();
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.rows.insert("KwilOther".into(), (vec![], vec![]));
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.rows.insert("Environment".into(), (vec![], vec![]));
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.rows.get_mut("_Validation").unwrap().1.pop();
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    broken.rows.get_mut("InstallExecuteSequence").unwrap().1.push(sql(&[Some("DisableRollback"), None, Some("1401")]));
    assert!(tables::read_legacy_cached(&broken).is_err());
    let mut broken = clean.clone();
    let rows = &mut broken.rows.get_mut("InstallExecuteSequence").unwrap().1;
    rows.iter_mut().find(|r| r[0].as_deref() == Some("KwilPreflight")).unwrap()[1] = Some("NOT Installed".into());
    assert!(tables::read_legacy_cached(&broken).is_err());
}

// Schema-2 controls are authored source only; ROOT owns fail-first execution.
fn release_package(input: &str) -> Package {
    let identity = release_identity(input).unwrap();
    let mut p = package("0.0.1");
    p.msi_version = identity.msi_version.clone();
    p.release = Some(identity);
    let mut guard = pe_fixture(None);
    guard[0x4ae] = b'2';
    p.guard_hash = sha256(&guard);
    p.build_id = sha256(&p.build_bytes().unwrap());
    p.product_code = p.expected_product().unwrap();
    p
}
#[test]
fn schema2_release_boundaries_numeric_order_and_exact_ascii() {
    for (input, numeric) in [
        ("0.1.0-beta.1", "0.0.1"),
        ("0.1.0-beta.9", "0.0.9"),
        ("0.1.0-beta.10", "0.0.10"),
        ("0.1.0-beta.65535", "0.0.65535"),
        ("0.1.0", "0.1.0"),
    ] {
        assert_eq!(release_identity(input).unwrap().msi_version, numeric);
    }
    for input in [
        "",
        "0.0.1",
        "0.1.1",
        "0.1.0-beta.0",
        "0.1.0-beta.01",
        "0.1.0-beta.65536",
        "0.1.0-rc.1",
        "0.1.0-beta.1+meta",
        "0.1.0+meta",
        "0.1.0-beta.١",
        "0.1.0-beta.1\n",
        "0.1.0\r\n",
        " 0.1.0",
        "0.1.0 ",
    ] {
        assert!(release_identity(input).is_err(), "{input:?}");
    }
    assert!(
        release_package("0.1.0-beta.9").version_numbers().unwrap()
            < release_package("0.1.0-beta.10").version_numbers().unwrap()
    );
    assert!(
        release_package("0.1.0-beta.65535").version_numbers().unwrap()
            < release_package("0.1.0").version_numbers().unwrap()
    );
}
#[test]
fn schema2_fixed_canonical_vectors_are_independent_of_production_encoders() {
    let mut p = release_package("0.1.0-beta.10");
    p.guard_hash = "b".repeat(64);
    p.resources.clear(); // canonical primitive, not an installable package
    p.build_id = sha256(&p.build_bytes().unwrap());
    assert_eq!(
        hex(&p.build_bytes().unwrap()),
        "4b57423201000000320100000031260000007b33443943324336462d374335392d344435342d393242342d3042413145433630333437417d030000007836340d000000302e312e302d626574612e3130040000006265746106000000302e302e31302800000061616161616161616161616161616161616161616161616161616161616161616161616161616161160000007838365f36342d70632d77696e646f77732d6d7376630100000032400000006262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626201000000010000004e0c0000006b65726e656c33322e646c6c080000004b57523100000000"
    );
    assert_eq!(p.build_id, "cb271f49ebab129086e7586a18105aa74f59b9f270f2391412a7a607b3162324");
    assert_eq!(p.expected_product().unwrap(), "{BCAF26CE-222B-5CFC-841D-1DAD00D90B20}");
}
#[test]
fn schema2_component_owner_and_shortcut_bytes_remain_legacy_exact() {
    let legacy = package("0.1.0");
    for input in ["0.1.0-beta.1", "0.1.0-beta.65535", "0.1.0"] {
        let active = release_package(input);
        assert_eq!(active.resources, legacy.resources);
        assert_eq!(inventory_bytes(&active.resources).unwrap(), inventory_bytes(&legacy.resources).unwrap());
    }
}
#[test]
fn schema2_active_and_cached_readers_are_strict_and_mappings_survive_rehash() {
    let legacy = package("0.1.0");
    assert!(tables::read(&database(&legacy)).is_err());
    tables::read_legacy_cached(&database(&legacy)).unwrap();
    tables::read_cached(&database(&legacy)).unwrap();
    let active = release_package("0.1.0-beta.2");
    tables::read(&database(&active)).unwrap();
    tables::read_cached(&database(&active)).unwrap();
    assert!(tables::read_legacy_cached(&database(&active)).is_err());
    for (column, value) in [(1, "1"), (2, "2"), (10, "1"), (5, "0.0.2"), (6, "stable"), (7, "0.1.0")] {
        let mut db = database(&active);
        db.rows.get_mut("KwilPackage").unwrap().1[0][column] = Some(value.into());
        assert!(tables::read(&db).is_err(), "column {column}");
    }
    let mut bad = active.clone();
    bad.release.as_mut().unwrap().channel = "stable".into();
    assert!(bad.build_bytes().is_err()); // even a caller attempting to recompute canonical hashes must refuse
    let mut bad = active.clone();
    bad.msi_version = "0.1.0".into();
    bad.release.as_mut().unwrap().msi_version = "0.1.0".into();
    assert!(bad.build_bytes().is_err()); // consistent duplicate cells cannot override the derived mapping
    // Recompute every identity from the wrong export's bytes. The ABI reader,
    // rather than an incidental stale hash, must still reject the V1 image.
    let mut wrong_guard = active.clone();
    wrong_guard.guard_hash = sha256(&pe_fixture(None));
    wrong_guard.build_id = sha256(&wrong_guard.build_bytes().unwrap());
    wrong_guard.product_code = wrong_guard.expected_product().unwrap();
    let mut wrong_export = database(&wrong_guard);
    wrong_export.guard[0x4ae] = b'1';
    let refusal = tables::read(&wrong_export).unwrap_err();
    assert_eq!(refusal.code, "invalid-pe");
    assert_eq!(refusal.resource, "ABI export name");
    let mut legacy_wrong_guard = legacy.clone();
    let mut image = pe_fixture(None);
    image[0x4ae] = b'2';
    legacy_wrong_guard.guard_hash = sha256(&image);
    legacy_wrong_guard.build_id = sha256(&legacy_wrong_guard.build_bytes().unwrap());
    legacy_wrong_guard.product_code = legacy_wrong_guard.expected_product().unwrap();
    let mut wrong_export = database(&legacy_wrong_guard);
    wrong_export.guard[0x4ae] = b'2';
    let refusal = tables::read_cached(&wrong_export).unwrap_err();
    assert_eq!(refusal.code, "invalid-pe");
    assert_eq!(refusal.resource, "ABI export name");
    for (name, value) in [
        ("ProductName", "Kwil"),
        ("ProductVersion", "0.1.0"),
        ("KWIL_RELEASE_VERSION", "0.1.0"),
        ("KWIL_CHANNEL", "stable"),
        ("KWIL_SCHEMA", "1"),
        ("KWIL_GUARD_ABI", "1"),
    ] {
        let mut db = database(&active);
        let property =
            db.rows.get_mut("Property").unwrap().1.iter_mut().find(|row| row[0].as_deref() == Some(name)).unwrap();
        property[1] = Some(value.into());
        assert!(tables::read(&db).is_err(), "{name}");
        let expected = tables::properties(&active).into_iter().collect::<BTreeMap<_, _>>();
        assert!(
            tables::effective_properties(&active, |key| Ok(if key == name {
                value.into()
            } else {
                expected.get(key).cloned().unwrap_or_default()
            }))
            .is_err(),
            "session {name}"
        );
    }
}
#[test]
fn schema2_legacy_numeric_maintenance_and_replacement_authority_are_unchanged() {
    let legacy = package("0.1.0");
    for input in ["0.1.0-beta.1", "0.1.0-beta.65535", "0.1.0"] {
        assert_eq!(
            operation(&release_package(input), Some(&legacy), &install_session()).unwrap_err().code,
            "version-conflict"
        );
    }
    assert_eq!(operation(&legacy, Some(&legacy), &repair_session()).unwrap(), Operation::Repair);
    assert_eq!(operation(&legacy, Some(&legacy), &uninstall_session()).unwrap(), Operation::Uninstall);
    let mut repack = legacy.clone();
    repack.package_code = "{22222222-2222-4333-8444-555555555555}".into();
    assert_eq!(operation(&repack, Some(&legacy), &repair_session()).unwrap_err().code, "package-mismatch");
    let before = release_package("0.1.0-beta.9");
    let after = release_package("0.1.0-beta.10");
    assert_eq!(operation(&before, Some(&before), &repair_session()).unwrap(), Operation::Repair);
    assert_eq!(operation(&before, Some(&before), &uninstall_session()).unwrap(), Operation::Uninstall);
    let mut repack = before.clone();
    repack.package_code = "{22222222-2222-4333-8444-555555555555}".into();
    for session in [repair_session(), uninstall_session()] {
        assert_eq!(operation(&repack, Some(&before), &session).unwrap_err().code, "package-mismatch");
    }
    assert_eq!(operation(&after, Some(&before), &install_session()).unwrap(), Operation::Upgrade);
    assert_eq!(operation(&before, Some(&after), &install_session()).unwrap_err().code, "version-conflict");
    let mut other_arch = after.clone();
    other_arch.architecture = "arm64".into();
    assert_eq!(operation(&other_arch, Some(&before), &install_session()).unwrap_err().code, "architecture-conflict");
}

#[test]
fn foreign_values_are_reported_directly_even_when_the_containing_key_is_foreign() {
    let package = package("1.2.3");
    for value in package.resources.iter().filter(|row| row.kind() == "R") {
        let key = package.resources.iter().find(|row| row.kind() == "K" && row.relative() == value.relative()).unwrap();
        for observation in [
            Observation::Value { value_type: 1, bytes: vec![0, 0] },
            Observation::Value { value_type: 4, bytes: 7u32.to_le_bytes().to_vec() },
        ] {
            let mut api = InventoryApi::empty();
            api.seen.insert(key.identity(), Observation::Key);
            api.seen.insert(value.identity(), observation);
            let before = api.seen.clone();
            let refusal = resources(&package, None, &mut api).unwrap_err();
            assert_eq!(refusal.code, "foreign-resource");
            assert_eq!(
                refusal.resource,
                value.identity(),
                "a containing key must not mask the value-specific collision"
            );
            assert_eq!(api.seen, before, "refusal is read-only");
        }
    }
}
