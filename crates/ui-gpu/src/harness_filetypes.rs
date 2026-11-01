//! Finite hidden-window file-type lifecycle using only an in-memory registry.
use super::*;
use kwil_core_commands::{
    associations::{
        AssociationAdapter, AssociationError, AssociationPlatform, Change, DefaultChoice, Location, StoredValue,
        ValueBudget,
    },
    file_types::{Extension, Preset, Selection},
};
use kwil_view_model::file_type_registration::{
    FileTypeRegistration, RegistrationContext, RegistrationLimits, RegistrationStatus,
};

#[derive(Default)]
struct Registry(Vec<(Location, StoredValue)>);
impl AssociationAdapter for Registry {
    fn owner(&self) -> String {
        "kw72-hidden-fake".into()
    }
    fn read(&mut self, key: &Location) -> Result<Option<StoredValue>, AssociationError> {
        self.read_bounded(key, &mut ValueBudget::new(kwil_core_commands::associations::MAX_VALUE_BYTES))
    }
    fn read_bounded(
        &mut self,
        key: &Location,
        budget: &mut ValueBudget,
    ) -> Result<Option<StoredValue>, AssociationError> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, value)| budget.copy(value.kind, &value.bytes)).transpose()
    }
    fn write(&mut self, key: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        self.0.retain(|(k, _)| k != key);
        if let Some(value) = value {
            self.0.push((key.clone(), value.clone()));
        }
        Ok(())
    }
    fn sync_restored(&mut self, _: &[Change]) -> Result<(), AssociationError> {
        Ok(())
    }
}

pub(super) fn setup(app: &mut App) {
    assert!(app.opts.hidden);
    let root = PathBuf::from(std::env::var_os("KWIL_WORKSPACE_ROOT").expect("isolated fixture"));
    assert!(root.is_absolute());
    let proxy = app.proxy.clone();
    let mut service = FileTypeRegistration::start(
        root.join("fake-filetypes-journal"),
        RegistrationContext {
            platform: AssociationPlatform::Windows,
            executable: root.join("kwil.exe"),
            content_types: Default::default(),
            default_choice: DefaultChoice::KeepCurrent,
            limits: RegistrationLimits::RootAdopted256,
        },
        || Ok(Registry::default()),
        std::sync::Arc::new(move || {
            let _ = proxy.send_event(UserEvent::RegistrationReady);
        }),
    )
    .unwrap();
    service.status().unwrap();
    app.file_types = Some(super::super::filetypes_ui::Page { service, ui: Default::default() });
    app.dispatch_settings("view.settings");
    app.settings_view.hosted = true;
    app.apply(Damage { full: true, ..Default::default() });
}

pub(super) fn register(app: &mut App) {
    let page = app.file_types.as_mut().unwrap();
    *page.service.selection_mut() = Selection::new(Preset::PlainTextOnly);
    page.click(1); // Same action as the page's Register selected button.
    app.apply(Damage { full: true, ..Default::default() });
}

pub(super) fn unregister(app: &mut App) {
    app.file_types.as_mut().unwrap().click(3);
    app.apply(Damage { full: true, ..Default::default() });
}

pub(super) fn check(app: &mut App, registered: bool) {
    let service = &app.file_types.as_ref().unwrap().service;
    assert!(!matches!(service.registration_status(), RegistrationStatus::Pending | RegistrationStatus::Failed { .. }));
    let actual = service.registered().expect("fresh OS inspection completed");
    for extension in ["txt", "text", "log"] {
        assert_eq!(actual.contains(&Extension::parse(extension).unwrap()), registered);
    }
    if !registered {
        assert!(service.selection().extensions().is_empty(), "no stale checkboxes");
    }
    println!("kw72 file-types registered={registered}; explicit fake registry, no native writes");
}
