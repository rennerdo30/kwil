//! Off-UI preparation for an explicit user registration request.
//!
//! ROOT must pass the actual host mode, after resolving the existing settings
//! path. No environment lookup, profile fallback, UI callback, worker launch,
//! registry access or default-app mutation happens here. Interactive Windows
//! preparation may create one missing final settings directory, then its private
//! journal child, beneath an existing reviewed ancestor. No recursive creation.
use kwil_core_commands::associations::{AssociationAdapter, AssociationError, Change, Location, StoredValue};
use kwil_view_model::file_type_registration::RegistrationContext;
use std::path::{Path, PathBuf};

#[cfg(any(windows, test))]
#[path = "registration_host_security.rs"]
mod security;
#[cfg(windows)]
#[path = "registration_host_windows.rs"]
mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMode {
    Interactive,
    HiddenHarness,
}

pub type NativeFactory = Box<dyn FnOnce() -> Result<NativeAdapter, AssociationError> + Send>;

/// Consume these fields in FileTypeRegistration::start on the preparation
/// thread, supplying ROOT's RegistrationWake callback. Send only the resulting
/// controller/error to the UI. Keep one controller per resolved settings path.
pub struct Prepared {
    pub journal: PathBuf,
    pub context: RegistrationContext,
    pub factory: NativeFactory,
}

/// Call on a background preparation thread after an explicit user request.
/// Hidden runs fail BEFORE current_exe, token, directory, or registry access.
/// Hidden fixtures must pass their own explicit fake to the generic controller.
pub fn prepare(settings_path: &Path, mode: HostMode) -> Result<Prepared, String> {
    if mode == HostMode::HiddenHarness {
        return Err("Native file-type registration is disabled in hidden test mode".into());
    }
    #[cfg(windows)]
    {
        windows::prepare(settings_path)
    }
    #[cfg(not(windows))]
    {
        let _ = settings_path;
        Err("File-type registration is unavailable: this platform's bundle/MIME context has not been verified".into())
    }
}

/// No public constructor: only successful native preparation can create this.
/// Guards stay on the worker for its entire journal lifetime, including Undo.
pub struct NativeAdapter {
    #[cfg(windows)]
    inner: kwil_core_commands::associations::WindowsAssociations,
    #[cfg(windows)]
    _guards: Vec<std::fs::File>,
    #[cfg(not(windows))]
    _unconstructible: std::convert::Infallible,
}

impl AssociationAdapter for NativeAdapter {
    fn read_bounded(
        &mut self,
        location: &Location,
        budget: &mut kwil_core_commands::associations::ValueBudget,
    ) -> Result<Option<StoredValue>, AssociationError> {
        #[cfg(windows)]
        {
            self.inner.read_bounded(location, budget)
        }
        #[cfg(not(windows))]
        {
            let _ = (location, budget);
            match self._unconstructible {}
        }
    }
    fn owner(&self) -> String {
        #[cfg(windows)]
        {
            self.inner.owner()
        }
        #[cfg(not(windows))]
        {
            match self._unconstructible {}
        }
    }
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
        #[cfg(windows)]
        {
            self.inner.read(location)
        }
        #[cfg(not(windows))]
        {
            let _ = location;
            match self._unconstructible {}
        }
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        #[cfg(windows)]
        {
            self.inner.write(location, value)
        }
        #[cfg(not(windows))]
        {
            let _ = (location, value);
            match self._unconstructible {}
        }
    }
    fn sync_restored(&mut self, changes: &[Change]) -> Result<(), AssociationError> {
        #[cfg(windows)]
        {
            self.inner.sync_restored(changes)
        }
        #[cfg(not(windows))]
        {
            let _ = changes;
            match self._unconstructible {}
        }
    }
    fn notify_changes(&mut self) {
        #[cfg(windows)]
        self.inner.notify_changes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_mode_refuses_even_an_invalid_settings_path_before_io() {
        let error = prepare(Path::new("relative/does-not-exist/settings.toml"), HostMode::HiddenHarness).err().unwrap();
        assert_eq!(error, "Native file-type registration is disabled in hidden test mode");
    }
    #[cfg(not(windows))]
    #[test]
    fn unverified_platform_never_offers_a_native_adapter() {
        let error = prepare(Path::new("/missing/settings.toml"), HostMode::Interactive).err().unwrap();
        assert!(error.contains("bundle/MIME context has not been verified"));
    }
}
