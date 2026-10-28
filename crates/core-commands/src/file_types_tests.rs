use super::*;
use crate::associations::{
    self, AssociationAdapter, AssociationError, AssociationPlatform, AssociationRequest, DefaultChoice, FileType,
    Location, StoredValue,
};

#[test]
fn file_types_catalog_is_broad_unique_and_contains_owner_named_extensions() {
    let _admission_case = crate::associations::test_admission_guard();
    let all = Selection::new(Preset::AllTextTypes);
    assert!((150..=250).contains(&catalog().len()));
    assert_eq!(catalog().len(), all.extensions().len());
    for name in [
        "txt", "text", "log", "md", "markdown", "rst", "java", "c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx",
        "json", "jsonl", "ndjson", "toml", "yaml", "yml", "xml", "html", "css", "ts", "rs", "cs", "pyi", "sql", "ini",
        "cfg", "conf", "cmake", "mk", "gradle",
    ] {
        assert!(all.contains(&Extension::parse(name).unwrap()), "missing {name}");
    }
    for binary in ["exe", "dll", "zip", "gz", "png", "jpg", "pdf", "docx", "class", "jar", "wasm"] {
        assert!(!all.contains(&Extension::parse(binary).unwrap()), "binary/archive claim {binary}");
    }
    for group in groups() {
        assert!(catalog().iter().any(|entry| entry.group == *group));
    }
}
#[test]
fn file_types_scripts_system_types_and_binary_types_cannot_be_selected_or_planned() {
    let _admission_case = crate::associations::test_admission_guard();
    struct NoNativeAccess;
    impl AssociationAdapter for NoNativeAccess {
        fn owner(&self) -> String {
            panic!("unsafe batch reached adapter ownership")
        }
        fn read(&mut self, _: &Location) -> Result<Option<StoredValue>, AssociationError> {
            panic!("unsafe batch read native state")
        }
        fn read_bounded(
            &mut self,
            _: &Location,
            _: &mut crate::associations::ValueBudget,
        ) -> Result<Option<StoredValue>, AssociationError> {
            panic!("unsafe batch read bounded native state")
        }
        fn write(&mut self, _: &Location, _: Option<&StoredValue>) -> Result<(), AssociationError> {
            panic!("unsafe batch wrote native state")
        }
    }
    for denied in UNSAFE_REGISTRATION_EXTENSIONS {
        assert!(!catalog().iter().any(|entry| entry.extension.eq_ignore_ascii_case(denied)), "offered {denied}");
        assert!(!registration_allowed(&denied.to_ascii_uppercase()));
        let mut selection = Selection::new(Preset::PlainTextOnly);
        let before = selection.clone();
        assert_eq!(
            selection.add_custom(&format!(".{}", denied.to_ascii_uppercase())),
            Err(ExtensionError::UnsafeAssociation)
        );
        assert_eq!(selection, before);
        let request = AssociationRequest {
            platform: AssociationPlatform::Windows,
            executable: std::env::temp_dir().join("kwil-refusal.exe"),
            default_choice: DefaultChoice::KeepCurrent,
            types: vec![
                FileType { extension: "txt".into(), content_type: None },
                FileType { extension: denied.to_ascii_uppercase(), content_type: None },
            ],
        };
        assert!(associations::plan(&mut NoNativeAccess, &request).is_err(), "planned {denied}");
    }
}
#[test]
fn file_types_presets_and_custom_are_explicit_and_deduplicate_case() {
    let _admission_case = crate::associations::test_admission_guard();
    let plain = Selection::new(Preset::PlainTextOnly);
    assert_eq!(plain.extensions().iter().map(Extension::as_str).collect::<Vec<_>>(), ["log", "text", "txt"]);
    let common = Selection::new(Preset::CommonTextConfig);
    assert!(common.contains(&Extension::parse("toml").unwrap()));
    assert!(!common.contains(&Extension::parse("cpp").unwrap()));
    let mut custom = Selection::default();
    assert!(custom.extensions().is_empty());
    custom.add_custom(".TXT").unwrap();
    custom.add_custom("txt").unwrap();
    assert_eq!(custom.extensions().len(), 1);
    for bad in ["", ".", "*.txt", "txt,rs", "tar.gz", "../rs", "a/b", "a\\b", " rs", "rs ", "é", "rs\n"] {
        assert!(custom.add_custom(bad).is_err(), "accepted {bad:?}");
    }
    assert_eq!(custom.extensions().len(), 1);
    assert!(custom.add_custom(&"a".repeat(65)).is_err());
}
#[test]
fn file_types_capacity_256_and_257_refusal_preserves_selection() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut selected = Selection::default();
    for index in 0..256 {
        selected.add_custom(&format!("type{index}")).unwrap();
    }
    let before = selected.clone();
    assert_eq!(selected.add_custom("extra"), Err(ExtensionError::TooMany));
    assert_eq!(selected, before);
    selected.add_custom("TYPE0").unwrap();
    assert_eq!(selected.extensions().len(), 256);
}
#[test]
fn file_types_actual_path_suffix_and_extensionless_build_hints() {
    let _admission_case = crate::associations::test_admission_guard();
    assert_eq!(current_extension(Path::new("folder/real.File.JSON")).unwrap().as_str(), "json");
    assert_eq!(current_extension(Path::new("folder/Makefile")), Err(ExtensionError::NoExtension));
    assert_eq!(current_extension(Path::new("folder/.env")), Err(ExtensionError::NoExtension));
    assert!(filename_hints().contains(&"Makefile"));
}

#[derive(Default)]
struct Fake {
    values: Vec<(Location, StoredValue)>,
    writes: usize,
}
impl AssociationAdapter for Fake {
    fn owner(&self) -> String {
        "filetypes-test-user".into()
    }
    fn read(&mut self, key: &Location) -> Result<Option<StoredValue>, AssociationError> {
        Ok(self.values.iter().find(|(location, _)| location == key).map(|(_, value)| value.clone()))
    }
    fn read_bounded(
        &mut self,
        key: &Location,
        budget: &mut crate::associations::ValueBudget,
    ) -> Result<Option<StoredValue>, AssociationError> {
        self.values
            .iter()
            .find(|(location, _)| location == key)
            .map(|(_, value)| budget.copy(value.kind, &value.bytes))
            .transpose()
    }
    fn write(&mut self, key: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        self.writes += 1;
        self.values.retain(|(location, _)| location != key);
        if let Some(value) = value {
            self.values.push((key.clone(), value.clone()));
        }
        Ok(())
    }
}
#[test]
fn file_types_named_presets_use_real_plan_without_any_write() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut fake = Fake::default();
    for preset in [Preset::PlainTextOnly, Preset::CommonTextConfig] {
        let selected = Selection::new(preset);
        let request = AssociationRequest {
            platform: AssociationPlatform::Windows,
            executable: std::env::temp_dir().join("kwil-test.exe"),
            default_choice: DefaultChoice::KeepCurrent,
            types: selected
                .extensions()
                .iter()
                .map(|extension| FileType { extension: extension.as_str().into(), content_type: None })
                .collect(),
        };
        let plan = associations::plan(&mut fake, &request).unwrap();
        assert_eq!(plan.changes().len(), selected.extensions().len() * 3);
        assert_eq!(fake.writes, 0);
        let mut receipt = associations::apply(&mut fake, &plan).unwrap();
        let repeated = associations::plan(&mut fake, &request).unwrap();
        assert!(repeated.changes().is_empty());
        assert!(associations::rollback(&mut fake, &mut receipt).is_empty());
        assert!(fake.values.is_empty());
        fake.writes = 0;
    }
}
