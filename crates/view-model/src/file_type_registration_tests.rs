//! Draft focused tests. Only a fake adapter; no native constructor is referenced.
//! ROOT must run with an isolated scratch root and bounded external timeout.
use super::*;
use kwil_core_commands::{associations::StoredValue, file_types::Preset};
use std::{
    fs,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct FakeState {
    values: Vec<(Location, StoredValue)>,
    writes: usize,
    fail_from: Option<usize>,
    read_gate: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    dropped_on: Option<thread::ThreadId>,
}
struct Fake(Arc<Mutex<FakeState>>);
impl Drop for Fake {
    fn drop(&mut self) {
        self.0.lock().unwrap().dropped_on = Some(thread::current().id());
    }
}
impl AssociationAdapter for Fake {
    fn owner(&self) -> String {
        "filetype-registration-fake".into()
    }
    fn read(&mut self, location: &Location) -> Result<Option<StoredValue>, AssociationError> {
        self.read_bounded(
            location,
            &mut kwil_core_commands::associations::ValueBudget::new(kwil_core_commands::associations::MAX_VALUE_BYTES),
        )
    }
    fn read_bounded(
        &mut self,
        location: &Location,
        budget: &mut kwil_core_commands::associations::ValueBudget,
    ) -> Result<Option<StoredValue>, AssociationError> {
        let gate = self.0.lock().unwrap().read_gate.take();
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            release
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| AssociationError("fake read gate timed out".into()))?;
        }
        let state = self.0.lock().unwrap();
        state
            .values
            .iter()
            .find(|(key, _)| key == location)
            .map(|(_, value)| budget.copy(value.kind, &value.bytes))
            .transpose()
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if state.fail_from.is_some_and(|from| state.writes >= from) {
            return Err(AssociationError("fake apply/rollback failure".into()));
        }
        state.values.retain(|(key, _)| key != location);
        if let Some(value) = value {
            state.values.push((location.clone(), value.clone()));
        }
        Ok(())
    }
    fn sync_restored(&mut self, _: &[kwil_core_commands::associations::Change]) -> Result<(), AssociationError> {
        Ok(())
    }
}
struct Harness {
    session: Option<FileTypeRegistration>,
    fake: Arc<Mutex<FakeState>>,
    wake: mpsc::Receiver<()>,
    journal: PathBuf,
}
impl Harness {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let journal = std::env::temp_dir().join(format!(
            "kwil-filetypes-fake-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&journal).unwrap();
        #[cfg(unix)]
        let journal = journal.canonicalize().unwrap();
        let fake = Arc::new(Mutex::new(FakeState::default()));
        let owned = fake.clone();
        let (send, wake) = mpsc::channel();
        let context = RegistrationContext {
            platform: AssociationPlatform::Windows,
            executable: journal.join("kwil-test.exe"),
            content_types: BTreeMap::new(),
            default_choice: DefaultChoice::KeepCurrent,
            limits: RegistrationLimits::Base128,
        };
        let session = FileTypeRegistration::start(
            journal.clone(),
            context,
            move || Ok(Fake(owned)),
            Arc::new(move || {
                let _ = send.send(());
            }),
        )
        .unwrap();
        Self { session: Some(session), fake, wake, journal }
    }
    fn session(&mut self) -> &mut FileTypeRegistration {
        self.session.as_mut().unwrap()
    }
    fn event(&mut self) {
        // Bounded notification-driven dispatch, including a retirement-capacity wake.
        // Not a timer/poll loop. At most three notifications for this one request.
        for _ in 0..3 {
            self.wake.recv_timeout(Duration::from_secs(2)).unwrap();
            if self.session().worker_ready() {
                return;
            }
        }
        panic!("no addressed registration event");
    }
    fn preview_plain(&mut self) -> ReviewId {
        *self.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
        self.session().preview().unwrap();
        self.event();
        match self.session().registration_status() {
            RegistrationStatus::Ready { review } => review.clone(),
            other => panic!("{other:?}"),
        }
    }
    fn preview_one(&mut self, extension: &str) -> ReviewId {
        *self.session().selection_mut() = Selection::default();
        self.session().selection_mut().add_custom(extension).unwrap();
        self.session().preview().unwrap();
        self.event();
        self.session().reviewed_preview().unwrap().review.clone()
    }
    /// Desired bytes/locations come from the actual planner, not a duplicated
    /// test implementation of the native Windows command or registry encoding.
    fn required_windows_values(&mut self, extension: &str) -> Vec<(Location, StoredValue)> {
        let request = AssociationRequest {
            platform: AssociationPlatform::Windows,
            executable: self.journal.join("kwil-test.exe"),
            types: vec![FileType { extension: extension.into(), content_type: None }],
            default_choice: DefaultChoice::KeepCurrent,
        };
        let mut adapter = Fake(self.fake.clone());
        kwil_core_commands::associations::plan(&mut adapter, &request)
            .unwrap()
            .changes()
            .iter()
            .map(|change| (change.location.clone(), change.after.clone().unwrap()))
            .collect()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop().shutdown().unwrap();
        }
        fs::remove_dir_all(&self.journal).unwrap(); // only this harness's freshly created isolated root
    }
}
#[test]
fn registration_preview_skip_and_default_do_not_mutate() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    assert_eq!(h.session().registration_status(), &RegistrationStatus::Idle);
    h.preview_plain();
    assert_eq!(h.fake.lock().unwrap().writes, 0);
    h.session().skip().unwrap();
    h.event();
    assert_eq!(h.session().registration_status(), &RegistrationStatus::Cancelled);
    assert_eq!(h.fake.lock().unwrap().writes, 0);
}

#[test]
fn registration_adopted_all_offered_is_one_reviewed_transaction_and_restores_exact_original() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    h.session().state_mut().context.limits = RegistrationLimits::RootAdopted256;
    let (location, _) = h.required_windows_values("txt").remove(0);
    let original = StoredValue { kind: 42, bytes: vec![255, 0, 17, 0] };
    h.fake.lock().unwrap().values.push((location.clone(), original.clone()));
    *h.session().selection_mut() = Selection::new(Preset::AllTextTypes);
    let offered = kwil_core_commands::file_types::catalog().len();
    assert!(offered > 128);
    assert_eq!(h.session().selection().extensions().len(), offered);
    h.session().preview().unwrap();
    h.event();
    let review = h.session().reviewed_preview().unwrap();
    assert_eq!(review.extensions.len(), offered);
    let token = review.review.clone();
    assert_eq!(h.fake.lock().unwrap().writes, 0);
    h.session().register_selected(token, None).unwrap();
    h.event();
    let journal = match h.session().registration_status() {
        RegistrationStatus::Applied { journal_id } => *journal_id,
        other => panic!("{other:?}"),
    };
    assert_eq!(h.fake.lock().unwrap().values.len(), offered * 3);
    assert_eq!(
        fs::read_dir(&h.journal)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension().is_some_and(|ext| ext == "intent"))
            .count(),
        1
    );
    h.session().unregister(journal).unwrap();
    h.event();
    assert_eq!(h.fake.lock().unwrap().values, vec![(location, original)]);
}
#[test]
fn registration_explicit_accept_repeat_and_unregister_use_actual_durable_worker() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let review = h.preview_plain();
    h.session().register_selected(review.clone(), None).unwrap();
    h.event();
    let journal_id = match h.session().registration_status() {
        RegistrationStatus::Applied { journal_id } => *journal_id,
        other => panic!("{other:?}"),
    };
    assert!(h.journal.join(format!("{journal_id:020}.intent")).is_file());
    assert!(h.session().extension_status().iter().all(|row| row.outcome == ExtensionOutcome::ReceiptApplied));
    let writes = h.fake.lock().unwrap().writes;
    h.session().register_selected(review.clone(), None).unwrap();
    h.event();
    assert_eq!(h.fake.lock().unwrap().writes, writes);
    h.session().preview().unwrap();
    h.event(); // repeat preview reports retained status too
    assert_eq!(h.fake.lock().unwrap().writes, writes);
    h.session().selection_mut().add_custom("custom").unwrap();
    assert_eq!(h.session().preview().unwrap_err().kind, ActionErrorKind::ExtendNotAdopted { journal_id });
    h.session().unregister(journal_id).unwrap();
    h.event();
    assert!(h.fake.lock().unwrap().values.is_empty());
    assert_eq!(h.session().registration_status(), &RegistrationStatus::Unregistered { journal_id });
}
#[test]
fn registration_partial_failure_retains_receipt_and_exact_extension_inspection() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let review = h.preview_plain();
    // Apply first three values (log), then one text value; fail the next and rollback.
    h.fake.lock().unwrap().fail_from = Some(5);
    h.session().register_selected(review.clone(), None).unwrap();
    h.event();
    let journal_id = match h.session().registration_status() {
        RegistrationStatus::Failed { recovery: Some(id), .. } => *id,
        other => panic!("{other:?}"),
    };
    assert!(h.journal.join(format!("{journal_id:020}.intent")).exists());
    assert!(h.session().extension_status().iter().all(|row| row.outcome == ExtensionOutcome::NeedsInspection));
    h.session().status().unwrap();
    h.event();
    let rows = h.session().extension_status();
    assert_eq!(rows[0].extension.as_str(), "log");
    assert_eq!(rows[0].applied, 3);
    assert_eq!(rows[1].extension.as_str(), "text");
    assert_eq!(rows[1].applied, 1);
    assert_eq!(rows[1].before, 2);
    assert_eq!(rows[1].outcome, ExtensionOutcome::PartiallyApplied);
    assert_eq!(rows[2].outcome, ExtensionOutcome::RestoredVisible);
    h.fake.lock().unwrap().fail_from = None;
    h.session().unregister(journal_id).unwrap();
    h.event();
    assert!(h.fake.lock().unwrap().values.is_empty());
}
#[test]
fn registration_failure_cancellation_and_protected_defaults_are_distinct() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    h.session().state_mut().context.default_choice = DefaultChoice::MakeDefault;
    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    h.session().preview().unwrap();
    h.event();
    let RegistrationStatus::Failed { message, recovery: None } = h.session().registration_status() else {
        panic!("protected default was not rejected")
    };
    assert!(message.contains("protected defaults"));
    assert_eq!(h.fake.lock().unwrap().writes, 0);
    assert!(h.session().reviewed_preview().is_none());
}
#[test]
fn registration_stale_selection_and_native_current_file_capture_are_rejected() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let review = h.preview_plain();
    h.session().selection_mut().add_custom("rs").unwrap();
    assert_eq!(h.session().register_selected(review.clone(), None).unwrap_err().kind, ActionErrorKind::StaleReview);
    let old = CurrentFile::capture(DocumentIdentity { tab: 7, incarnation: 1 }, &h.journal.join("real.RS")).unwrap();
    h.session().preview_current(old.clone()).unwrap();
    h.event();
    let review = h.session().reviewed_preview().unwrap().review.clone();
    let renamed = CurrentFile::capture(old.identity(), &h.journal.join("real.py")).unwrap();
    let recycled = CurrentFile::capture(DocumentIdentity { tab: 7, incarnation: 2 }, old.native_path()).unwrap();
    for stale in [renamed, recycled] {
        assert_eq!(
            h.session().register_selected(review.clone(), Some(stale)).unwrap_err().kind,
            ActionErrorKind::StaleCurrentFile
        );
    }
    assert_eq!(h.fake.lock().unwrap().writes, 0);
    h.session().register_selected(review.clone(), Some(old)).unwrap();
    h.event();
    assert_eq!(h.session().extension_status()[0].extension.as_str(), "rs");
}
#[test]
fn registration_pending_preserves_whole_current_file_action_ownership() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    h.session().status().unwrap();
    let current = CurrentFile::capture(DocumentIdentity { tab: 3, incarnation: 9 }, &h.journal.join("a.ts")).unwrap();
    let error = h.session().preview_current(current.clone()).unwrap_err();
    assert_eq!(error.kind, ActionErrorKind::Busy);
    assert_eq!(
        error.action,
        RegistrationAction::Preview { extensions: vec![current.extension().clone()], current: Some(current) }
    );
    h.event();
}
#[test]
fn registration_base_capacity_and_unknown_mime_are_explicit_gates() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    *h.session().selection_mut() = Selection::new(Preset::AllTextTypes);
    assert!(matches!(
        h.session().preview().unwrap_err().kind,
        ActionErrorKind::UnadoptedCapacity { selected, limit: 128 }
            if selected == kwil_core_commands::file_types::catalog().len()
    ));
    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    h.session().state_mut().context.platform =
        AssociationPlatform::Linux { data_home: h.journal.join("data"), config_home: h.journal.join("config") };
    assert!(
        matches!(h.session().preview().unwrap_err().kind, ActionErrorKind::MissingContentTypes(types) if types.len() == 3)
    );
    assert_eq!(h.fake.lock().unwrap().writes, 0);
}
#[test]
fn registration_retirement_does_not_wait_for_pending_adapter_on_ui() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let (entered, ready) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    h.fake.lock().unwrap().read_gate = Some((entered, resume));
    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    h.session().preview().unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let retirement = h.session.take().unwrap().stop();
    // A blocking stop cannot return before the gate's timeout and will make this
    // send fail. No elapsed-time estimate or busy loop is used.
    release.send(()).unwrap();
    retirement.shutdown().unwrap();
    assert_ne!(h.fake.lock().unwrap().dropped_on, Some(thread::current().id()));
}
#[test]
fn registration_restart_retains_unknown_membership_receipt_and_additional_type_gate() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let review = h.preview_plain();
    h.session().register_selected(review.clone(), None).unwrap();
    h.event();
    h.session.take().unwrap().stop().shutdown().unwrap();
    let (send, wake) = mpsc::channel();
    let fake = h.fake.clone();
    let context = RegistrationContext {
        platform: AssociationPlatform::Windows,
        executable: h.journal.join("kwil-test.exe"),
        content_types: BTreeMap::new(),
        default_choice: DefaultChoice::KeepCurrent,
        limits: RegistrationLimits::Base128,
    };
    h.session = Some(
        FileTypeRegistration::start(
            h.journal.clone(),
            context,
            move || Ok(Fake(fake)),
            Arc::new(move || {
                let _ = send.send(());
            }),
        )
        .unwrap(),
    );
    h.wake = wake;
    h.session().status().unwrap();
    h.event();
    assert!(matches!(h.session().registration_status(), RegistrationStatus::RetainedRecovery { journal_id: 1, .. }));
    assert!(h.session().extension_status().is_empty(), "no invented durable selection metadata");
    h.session().selection_mut().add_custom("extra").unwrap();
    assert_eq!(h.session().preview().unwrap_err().kind, ActionErrorKind::ExtendNotAdopted { journal_id: 1 });
    h.session().unregister(1).unwrap();
    h.event();
    assert!(h.fake.lock().unwrap().values.is_empty());
}
#[cfg(unix)]
#[test]
fn registration_non_unicode_native_suffix_is_never_lossily_registered() {
    let _admission_case = registration_admission_case();
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let path = PathBuf::from(OsString::from_vec(b"/tmp/native.\xff".to_vec()));
    assert_eq!(
        CurrentFile::capture(DocumentIdentity { tab: 1, incarnation: 1 }, &path),
        Err(ActionErrorKind::Extension(ExtensionError::NonUnicode))
    );
    let path = PathBuf::from(OsString::from_vec(b"/tmp/native-\xff.rs".to_vec()));
    let current = CurrentFile::capture(DocumentIdentity { tab: 1, incarnation: 1 }, &path).unwrap();
    assert_eq!(current.native_path(), path);
    assert_eq!(current.extension().as_str(), "rs");
}

#[cfg(windows)]
#[test]
fn registration_windows_native_surrogate_path_preserved_but_suffix_rejected() {
    let _admission_case = registration_admission_case();
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    let mut units: Vec<u16> = "E:\\source-only\\native-".encode_utf16().collect();
    units.push(0xd800);
    units.extend(".RS".encode_utf16());
    let path = PathBuf::from(OsString::from_wide(&units));
    let current = CurrentFile::capture(DocumentIdentity { tab: 1, incarnation: 2 }, &path).unwrap();
    assert_eq!(current.native_path(), path);
    assert_eq!(current.extension().as_str(), "rs");
    let mut units: Vec<u16> = "E:\\source-only\\native.".encode_utf16().collect();
    units.push(0xd800);
    assert_eq!(
        CurrentFile::capture(DocumentIdentity { tab: 1, incarnation: 2 }, &PathBuf::from(OsString::from_wide(&units))),
        Err(ActionErrorKind::Extension(ExtensionError::NonUnicode))
    );
}

#[test]
fn registration_full_and_closed_return_entire_action_without_consuming_review() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let review = h.preview_plain();
    for queue in [SubmitError::Full, SubmitError::Closed] {
        h.session().state_mut().submit_error = Some(queue);
        let error = h.session().register_selected(review.clone(), None).unwrap_err();
        assert_eq!(error.kind, ActionErrorKind::Queue(queue));
        assert_eq!(error.action, RegistrationAction::RegisterSelected { review: review.clone(), current: None });
        assert_eq!(h.session().registration_status(), &RegistrationStatus::Ready { review: review.clone() });
        let current =
            CurrentFile::capture(DocumentIdentity { tab: 3, incarnation: 4 }, &h.journal.join("actual.ts")).unwrap();
        let error = h.session().preview_current(current.clone()).unwrap_err();
        assert_eq!(
            error.action,
            RegistrationAction::Preview { extensions: vec![current.extension().clone()], current: Some(current) }
        );
        assert!(h.session().state().pending.is_none());
        assert_eq!(h.fake.lock().unwrap().writes, 0);
    }
    h.session().state_mut().submit_error = None;
    h.session().register_selected(review.clone(), None).unwrap();
    h.event();
    assert!(matches!(h.session().registration_status(), RegistrationStatus::Applied { .. }));
}

#[test]
fn registration_replaced_controller_rejects_retained_review_with_same_generation_and_whole_action() {
    let _admission_case = registration_admission_case();
    let mut first = Harness::new();
    let retained = first.preview_one("txt");
    let action = RegistrationAction::RegisterSelected { review: retained.clone(), current: None };
    // Complete the first service's retirement before constructing the replacement.
    // The retained review/action must keep its controller identity alive.
    first.session.take().unwrap().stop().shutdown().unwrap();
    assert_eq!(first.fake.lock().unwrap().writes, 0);
    let mut replacement = Harness::new();
    let current = replacement.preview_one("rs");
    assert_eq!(retained.generation, current.generation, "both services deliberately start at generation one");
    let error = replacement.session().register_selected(retained.clone(), None).unwrap_err();
    assert_eq!(error.kind, ActionErrorKind::StaleReview);
    assert_eq!(error.action, action, "rejection must return the exact retained capability/action");
    assert!(replacement.session().state().pending.is_none());
    assert_eq!(replacement.session().registration_status(), &RegistrationStatus::Ready { review: current.clone() });
    assert_eq!(replacement.session().extension_status()[0].extension.as_str(), "rs");
    assert_eq!(replacement.fake.lock().unwrap().writes, 0, "stale txt acceptance must never apply replacement rs");
    assert_eq!(first.fake.lock().unwrap().writes, 0);
    assert_ne!(retained, current, "equal counters cannot identify different controllers");
}

#[test]
fn registration_review_generation_exhaustion_refuses_without_wrapping_or_consuming_action() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    h.session().state_mut().next_review = u64::MAX - 1;
    let last = h.preview_one("txt");
    assert_eq!(last.generation, u64::MAX);
    let expected =
        RegistrationAction::Preview { extensions: h.session().selection().extensions().to_vec(), current: None };
    let error = h.session().preview().unwrap_err();
    assert_eq!(error.kind, ActionErrorKind::Exhausted);
    assert_eq!(error.action, expected);
    assert_eq!(h.session().state().next_review, u64::MAX);
    assert_eq!(h.session().registration_status(), &RegistrationStatus::Ready { review: last });
    assert_eq!(h.fake.lock().unwrap().writes, 0);
}

#[test]
fn registration_missing_unchanged_command_never_becomes_available_from_receipt_inspection() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let required = h.required_windows_values("txt");
    assert_eq!(required.len(), 3);
    let command = required
        .iter()
        .find(|(location, _)| {
            matches!(location,
        Location::WindowsValue { subkey, name } if subkey.ends_with("\\shell\\open\\command") && name.is_empty())
        })
        .unwrap()
        .0
        .clone();
    // Description and command already match. Only OpenWithProgids will be owned
    // by this transaction's receipt, exactly the R1 false-availability trigger.
    h.fake.lock().unwrap().values = required
        .iter()
        .filter(|(location, _)| {
            matches!(location,
        Location::WindowsValue { subkey, .. } if subkey.starts_with("Kwil."))
        })
        .cloned()
        .collect();
    assert_eq!(h.fake.lock().unwrap().values.len(), 2);
    let review = h.preview_one("txt");
    assert_eq!(h.session().extension_status()[0].changed_values, 1);
    assert_eq!(h.fake.lock().unwrap().writes, 0);
    h.session().register_selected(review, None).unwrap();
    h.event();
    assert_eq!(h.session().extension_status()[0].outcome, ExtensionOutcome::ReceiptApplied);
    h.fake.lock().unwrap().values.retain(|(location, _)| location != &command);
    assert!(!h.fake.lock().unwrap().values.iter().any(|(location, _)| location == &command));
    let writes = h.fake.lock().unwrap().writes;
    h.session().status().unwrap();
    h.event();
    let row = &h.session().extension_status()[0];
    assert_eq!((row.changed_values, row.applied, row.before, row.conflicts, row.unreadable), (1, 1, 0, 0, 0));
    assert_eq!(
        row.outcome,
        ExtensionOutcome::ReceiptApplied,
        "only receipt state is known, not full Open With availability"
    );
    assert!(matches!(h.session().registration_status(), RegistrationStatus::RetainedRecovery { .. }));
    assert_eq!(h.fake.lock().unwrap().writes, writes, "inspection never repairs or changes OS state");
}

#[test]
fn registration_full_changed_receipt_control_reports_only_verified_receipt_and_detects_owned_command_loss() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let required = h.required_windows_values("txt");
    let command = required
        .iter()
        .find(|(location, _)| {
            matches!(location,
        Location::WindowsValue { subkey, .. } if subkey.ends_with("\\shell\\open\\command"))
        })
        .unwrap()
        .0
        .clone();
    let review = h.preview_one("txt");
    assert_eq!(h.session().extension_status()[0].changed_values, 3);
    h.session().register_selected(review, None).unwrap();
    h.event();
    h.session().status().unwrap();
    h.event();
    assert!(required.iter().all(|expected| h.fake.lock().unwrap().values.contains(expected)));
    let row = &h.session().extension_status()[0];
    assert_eq!((row.applied, row.before, row.conflicts, row.unreadable), (3, 0, 0, 0));
    assert_eq!(row.outcome, ExtensionOutcome::ReceiptApplied);
    h.fake.lock().unwrap().values.retain(|(location, _)| location != &command);
    h.session().status().unwrap();
    h.event();
    let row = &h.session().extension_status()[0];
    assert_eq!((row.applied, row.before), (2, 1));
    assert_eq!(row.outcome, ExtensionOutcome::PartiallyApplied);
}

#[test]
fn registration_already_matching_preview_is_time_scoped_and_empty_receipt_cannot_refresh_it() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let required = h.required_windows_values("txt");
    h.fake.lock().unwrap().values = required;
    let review = h.preview_one("txt");
    assert_eq!(h.session().extension_status()[0].changed_values, 0);
    assert_eq!(h.session().reviewed_preview().unwrap().extensions[0].outcome, ExtensionOutcome::UnchangedAtPreview);
    h.session().register_selected(review, None).unwrap();
    h.event();
    assert_eq!(h.session().extension_status()[0].outcome, ExtensionOutcome::VerificationIncomplete);
    h.fake.lock().unwrap().values.retain(|(location, _)| {
        !matches!(location,
        Location::WindowsValue { subkey, .. } if subkey.ends_with("\\shell\\open\\command"))
    });
    h.session().status().unwrap();
    h.event();
    assert_eq!(h.session().extension_status()[0].outcome, ExtensionOutcome::VerificationIncomplete);
    assert_eq!(
        h.session().reviewed_preview().unwrap().extensions[0].outcome,
        ExtensionOutcome::UnchangedAtPreview,
        "historical Plan observation remains visibly separate from current receipt inspection"
    );
    assert_eq!(h.fake.lock().unwrap().writes, 0);
}

static REGISTRATION_ADMISSION_CASE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn registration_admission_case() -> std::sync::MutexGuard<'static, ()> {
    REGISTRATION_ADMISSION_CASE.lock().unwrap_or_else(|poison| poison.into_inner())
}

// KW-72 (owner, 2026-10-29): "after unregister it still tells me that its
// registered, also the menu flow makes no sense". One Apply makes the OS match
// the checkboxes; Remove all clears everything; status shows the real state.
impl Harness {
    /// Follow one apply/remove chain to its end (each step is one worker event).
    fn settle(&mut self) -> RegistrationStatus {
        for _ in 0..6 {
            if !matches!(self.session().registration_status(), RegistrationStatus::Pending) {
                break;
            }
            self.event();
        }
        self.session().registration_status().clone()
    }
    fn open_with(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .fake
            .lock()
            .unwrap()
            .values
            .iter()
            .filter_map(|(location, _)| match location {
                Location::WindowsValue { subkey, .. } => {
                    subkey.strip_prefix('.').and_then(|tail| tail.strip_suffix("\\OpenWithProgids")).map(str::to_owned)
                }
                _ => None,
            })
            .collect();
        found.sort();
        found.dedup();
        found
    }
}
fn names(extensions: Option<&[Extension]>) -> Vec<String> {
    extensions.unwrap_or_default().iter().map(|e| e.as_str().to_owned()).collect()
}
#[test]
fn registration_apply_remove_all_and_reapply_show_the_real_state() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    assert_eq!(h.session().change_counts(), (3, 0));
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }), "one click registers");
    assert_eq!(names(h.session().registered()), ["log", "text", "txt"]);
    assert_eq!(h.open_with(), ["log", "text", "txt"]);
    assert_eq!(h.session().change_counts(), (0, 0));

    h.session().remove_all().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Unregistered { .. }));
    assert!(h.fake.lock().unwrap().values.is_empty(), "nothing of Kwil stays registered");
    // Reopening the page asks the worker again: it must not claim a registration.
    h.session().status().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::NoRetainedReceipt { .. }));
    assert!(!h.session().has_registration());
    assert_eq!(names(h.session().registered()), Vec::<String>::new());
    assert!(h.session().selection().extensions().is_empty(), "checkboxes show the real (empty) state");

    // Register again.
    h.session().selection_mut().add_custom("log").unwrap();
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }));
    assert_eq!(h.open_with(), ["log"]);
}
#[test]
fn registration_remove_all_blocking_is_the_uninstallers_remove_all() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    let deadline = || std::time::Instant::now() + Duration::from_secs(5);
    let Harness { session, wake, .. } = &mut h;
    let session = session.as_mut().unwrap();
    assert_eq!(session.remove_all_blocking(wake, deadline()), Ok(RemoveAllOutcome::NothingRegistered));

    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }));
    assert!(!h.fake.lock().unwrap().values.is_empty());
    let Harness { session, wake, .. } = &mut h;
    assert_eq!(session.as_mut().unwrap().remove_all_blocking(wake, deadline()), Ok(RemoveAllOutcome::Removed));
    assert!(h.fake.lock().unwrap().values.is_empty(), "uninstall leaves no Kwil entries");
    let Harness { session, wake, .. } = &mut h;
    assert_eq!(
        session.as_mut().unwrap().remove_all_blocking(wake, deadline()),
        Ok(RemoveAllOutcome::NothingRegistered),
        "running it twice is harmless"
    );
}
#[test]
fn registration_apply_adds_and_removes_single_types_of_an_existing_registration() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    for extension in ["txt", "log"] {
        h.session().selection_mut().add_custom(extension).unwrap();
    }
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }));
    h.session().selection_mut().toggle(Extension::parse("log").unwrap(), false).unwrap();
    h.session().selection_mut().add_custom("md").unwrap();
    assert_eq!(h.session().change_counts(), (1, 1));
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }), "no 'unregister first' detour");
    assert_eq!(h.open_with(), ["md", "txt"]);
    assert_eq!(names(h.session().registered()), ["md", "txt"]);
}
#[test]
fn registration_status_resets_checkboxes_to_what_is_registered() {
    let _admission_case = registration_admission_case();
    let mut h = Harness::new();
    h.session().selection_mut().add_custom("txt").unwrap();
    h.session().apply().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::Applied { .. }));
    // An unapplied draft is replaced by the real state when the page reopens.
    *h.session().selection_mut() = Selection::new(Preset::PlainTextOnly);
    h.session().status().unwrap();
    assert!(matches!(h.settle(), RegistrationStatus::RetainedRecovery { .. }));
    assert_eq!(names(Some(h.session().selection().extensions())), ["txt"]);
    assert!(h.session().has_registration());
}
