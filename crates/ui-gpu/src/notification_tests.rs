//! Windowless protocol/ownership controls. No native receiver, dialog or GPU
//! qualification is claimed. ROOT runs these and their fail-first mutants.
use super::*;
use std::{cell::RefCell, rc::Rc, sync::mpsc};

fn gate() -> (HostProxy, mpsc::Receiver<UserEvent>, AllocationBudget) {
    let budget = AllocationBudget::default();
    let (sender, receiver) = mpsc::channel();
    let proxy = HostProxy::admitted_test(sender, &budget).unwrap();
    (proxy, receiver, budget)
}
fn shared(proxy: &HostProxy) -> Arc<Shared> {
    proxy.shared().unwrap().clone()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn expired_handoff_seals_native_effect_without_success_proof() {
    let (proxy, _receiver, _) = gate();
    let end = deadline();
    proxy.request_terminal();
    let result = proxy.wait_native_with_clock(end, || end);
    let calls = std::cell::Cell::new(0);
    let route = HostProxy::route_with(&shared(&proxy), UserEvent::DocumentReady, false, |_| {
        calls.set(calls.get() + 1);
        NativeRoute::Continue(())
    });
    assert!(matches!(result, Err(Reason::TimedOut)));
    assert!(matches!(route.outcome, Routed::Continue(RouteStatus::OffUiWake)));
    assert_eq!(calls.get(), 0);
}

#[test]
fn late_zero_claim_observation_cannot_mint_on_time_success() {
    let (proxy, _receiver, _) = gate();
    let end = deadline();
    proxy.request_terminal();
    let mut times = [end - Duration::from_millis(1), end].into_iter();
    let result = proxy.wait_native_with_clock(end, || times.next().unwrap());
    assert!(matches!(result, Err(Reason::TimedOut)));
}

#[test]
fn fresh_native_error_is_retained_and_never_retried_or_off_ui_success() {
    let (proxy, _receiver, _) = gate();
    let calls = std::cell::Cell::new(0);
    let first =
        HostProxy::route_with(&shared(&proxy), UserEvent::Notice("exact native event".into()), false, |event| {
            calls.set(calls.get() + 1);
            NativeRoute::Break(EventLoopClosed(event))
        });
    proxy.request_terminal();
    let second = HostProxy::route_with(&shared(&proxy), UserEvent::DocumentReady, false, |_| {
        calls.set(calls.get() + 1);
        NativeRoute::Continue(())
    });
    assert!(matches!(first.outcome, Routed::Continue(RouteStatus::Fatal(Reason::NativeClosed))));
    assert!(matches!(second.outcome, Routed::Break((_, SendRefusal::Fatal(Reason::NativeClosed)))));
    assert_eq!(calls.get(), 1);
    let original = shared(&proxy);
    let state = lock(&original.state);
    assert!(
        matches!(&state.closed.as_ref().unwrap().original.0, UserEvent::Notice(value) if value == "exact native event")
    );
    drop(state);
    assert_eq!(proxy.inspect().unwrap().native_event(), Some("Notice"));
}

#[test]
fn native_panic_resumes_the_same_box_to_existing_callback_custodian() {
    let (proxy, _receiver, _) = gate();
    let original: Box<dyn Any + Send> = Box::new("original native panic".to_owned());
    let identity = std::ptr::from_ref(original.as_ref());
    let caught = catch_unwind(AssertUnwindSafe(|| {
        HostProxy::route_with(&shared(&proxy), UserEvent::StartupFontReady, true, |_| resume_unwind(original))
    }));
    let retained = match caught {
        Err(original) => original,
        Ok(_) => panic!("original panic must reach its custodian"),
    };
    assert!(std::ptr::eq(identity, std::ptr::from_ref(retained.as_ref())));
    assert_eq!(retained.downcast_ref::<String>().map(String::as_str), Some("original native panic"));
    assert_eq!(proxy.inspect().unwrap().reason(), Some(Reason::ExistingPanicCustodian));
}

#[test]
fn uncustodied_native_panic_remains_publicly_inspectable_after_ignored_send() {
    let (proxy, _receiver, _) = gate();
    let original: Box<dyn Any + Send> = Box::new(713usize);
    let identity = std::ptr::from_ref(original.as_ref());
    let route = HostProxy::route_with(&shared(&proxy), UserEvent::Notice("panic event".into()), false, |_| {
        resume_unwind(original)
    });
    assert!(matches!(route.outcome, Routed::Continue(RouteStatus::Fatal(Reason::NativePanicked))));
    let inspection = proxy.inspect().unwrap();
    assert!(std::ptr::eq(identity, std::ptr::from_ref(inspection.panic_payload().unwrap())));
    assert_eq!(inspection.panic_payload().unwrap().downcast_ref::<usize>(), Some(&713));
}

type Drops = Rc<RefCell<Vec<(&'static str, usize, ThreadId)>>>;
struct Observed<T> {
    original: T,
    name: &'static str,
    identity: usize,
    drops: Drops,
}
impl<T> Drop for Observed<T> {
    fn drop(&mut self) {
        self.drops.borrow_mut().push((self.name, self.identity, thread::current().id()));
    }
}

#[test]
fn held_claim_expiry_keeps_original_receiver_and_origin_only_owner() {
    let (proxy, receiver, budget) = gate();
    let drops = Rc::new(RefCell::new(Vec::new()));
    let original = Observed { original: receiver, name: "receiver", identity: 11, drops: drops.clone() };
    assert!(original.original.try_recv().is_err());
    let mut origin =
        OriginLoopOwner::admit(original, proxy.clone(), &budget).unwrap_or_else(|_| panic!("origin owner admission"));
    origin.retain_owners(Observed { original: (), name: "origin owner", identity: 12, drops: drops.clone() });
    let end = deadline();
    let (entered, started) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let actor = shared(&proxy);
    let worker = thread::spawn(move || {
        HostProxy::route_with(&actor, UserEvent::DocumentReady, false, |event| {
            entered.send(()).unwrap();
            held.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap();
            drop(event);
            NativeRoute::Continue(())
        })
    });
    started.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap();
    proxy.request_terminal();
    let expired = proxy.wait_native_with_clock(end, || end);
    drop(origin);
    let retained_at_failed_return = drops.borrow().is_empty();
    release.send(()).unwrap();
    let completed = worker.join().unwrap(); // cleanup original test actor before assertions
    assert!(matches!(expired, Err(Reason::TimedOut)));
    assert!(matches!(completed.outcome, Routed::Continue(RouteStatus::Enqueued)));
    assert!(retained_at_failed_return);
    assert!(drops.borrow().is_empty(), "late native completion cannot rehabilitate failed receiver release");
    assert_eq!(proxy.inspect().unwrap().reason(), Some(Reason::FailedVerdict));
}

#[test]
fn prestart_owner_and_queued_owner_are_distinct_unresolved_obligations() {
    let (proxy, _receiver, _) = gate();
    let transfer = proxy.reserve(SlotKind::Registration, size_of::<Delivery<Box<usize>>>()).unwrap();
    assert!(lock(&shared(&proxy).state).slots[SlotKind::Registration.index()].outstanding);
    transfer.start();
    let carrier = transfer.publish(Box::new(91usize));
    assert_eq!(**carrier.value.as_ref().unwrap(), 91);
    proxy.request_terminal();
    let proof = proxy.wait_native_until(deadline()).unwrap_or_else(|reason| panic!("quiescence: {reason:?}"));
    let result = proxy.finish_success(&proof, proof.deadline);
    assert_eq!(result, Err(Reason::UnresolvedOwner));
    drop(carrier);
    assert_eq!(proxy.inspect().unwrap().reason(), Some(Reason::UnresolvedOwner));
}

#[test]
fn original_gate_identity_rejects_foreign_quiescence_proof() {
    let (first, _first_receiver, _) = gate();
    let (second, _second_receiver, _) = gate();
    let end = deadline();
    first.request_terminal();
    second.request_terminal();
    let proof = first.wait_native_until(end).unwrap_or_else(|reason| panic!("quiescence: {reason:?}"));
    assert_eq!(second.finish_success(&proof, end), Err(Reason::Identity));
}

#[test]
fn terminal_value_and_intent_returns_original_without_native_delivery() {
    let (proxy, _receiver, _) = gate();
    proxy.request_terminal();
    let route = proxy.route(UserEvent::Notice("original computed product".into()), false);
    match route.outcome {
        Routed::Break((UserEvent::Notice(original), SendRefusal::CancelledValue)) => {
            assert_eq!(original, "original computed product")
        }
        _ => panic!("terminal value must return intact, not imply delivery"),
    }
    assert_eq!(proxy.send_event(UserEvent::SessionChanged(true)), Err(SendRefusal::CancelledIntent));
    assert_eq!(proxy.send_event(UserEvent::WindowPlacementProgress), Err(SendRefusal::CancelledIntent));
    assert_eq!(proxy.send_owned_wake(UserEvent::DocumentReady), Ok(WakeDisposition::OffUiWake));
}

#[test]
fn operation_admission_refuses_before_native_effect() {
    let (_probe, _probe_receiver, probe_budget) = gate();
    let bytes = HostProxy::operation_bytes().unwrap();
    let budget = AllocationBudget::new(probe_budget.retained_bytes().checked_add(bytes).unwrap() - 1);
    let (sender, _receiver) = mpsc::channel();
    let proxy = HostProxy::admitted_test(sender, &budget).unwrap();
    let calls = std::cell::Cell::new(0);
    let route = HostProxy::route_with(&shared(&proxy), UserEvent::DocumentReady, false, |_| {
        calls.set(calls.get() + 1);
        NativeRoute::Continue(())
    });
    assert!(matches!(route.outcome, Routed::Break((_, SendRefusal::Fatal(Reason::Admission)))));
    assert_eq!(calls.get(), 0);
}

#[test]
fn every_approved_value_intent_and_private_wake_is_explicitly_classified() {
    use crate::{markdown_ui, shaping};
    let (proxy, receiver, _) = gate();
    let document = kwil_view_model::Workspace::new(kwil_view_model::View::new(None)).unwrap().active_id();
    let values = [
        UserEvent::PaletteRoot(Err("original root".into())),
        UserEvent::SettingsPath(Err("original settings path".into())),
        UserEvent::Notice("original notice".into()),
        UserEvent::WindowPlacementLoaded(Err("original placement".into())),
        UserEvent::DiagnosticsReady("original diagnostics".into()),
        UserEvent::FontReady(None),
        UserEvent::UiFontReady(None),
        UserEvent::UiFallbackReady(901, Some((vec![1, 2, 3], 7))),
        UserEvent::GlyphReady(
            shaping::Tile {
                key: shaping::TileKey { text: "original tile".into(), columns: 2, fg: 3, bg: 4 },
                pixels: vec![17],
            },
            902,
        ),
        UserEvent::SettingsReady(Box::default(), Default::default()),
        UserEvent::MarkdownReady(Box::new(markdown_ui::notification_fixture_completion(document))),
    ];
    // Enqueue-before-handoff remains ordinary live dispatch, never a success
    // cancellation. Immutable values may retire only with the actual receiver.
    proxy.send_event(UserEvent::Notice("queued original".into())).unwrap();
    proxy.request_terminal();
    for original in values {
        let name = original.notification_name();
        let calls = std::cell::Cell::new(0);
        let routed = HostProxy::route_with(&shared(&proxy), original, false, |_| {
            calls.set(1);
            NativeRoute::Continue(())
        });
        match routed.outcome {
            Routed::Break((original, SendRefusal::CancelledValue)) => assert_eq!(original.notification_name(), name),
            _ => panic!("{name} must return the original computed value"),
        }
        assert_eq!(calls.get(), 0);
    }
    let intents = [
        UserEvent::SessionChanged(true),
        UserEvent::WindowPlacementProgress,
        UserEvent::DroppedFile("original-intent".into()),
    ];
    for original in intents {
        let name = original.notification_name();
        match proxy.route(original, false).outcome {
            Routed::Break((original, SendRefusal::CancelledIntent)) => assert_eq!(original.notification_name(), name),
            _ => panic!("{name} has no admitted operation or receipt authority"),
        }
    }
    #[cfg(windows)]
    assert_eq!(proxy.send_event(UserEvent::WindowPlacementTouched), Err(SendRefusal::CancelledIntent));
    #[cfg(feature = "frame-harness")]
    assert_eq!(
        proxy.send_event(UserEvent::Harness(crate::harness::Step::ContextClose)),
        Err(SendRefusal::CancelledIntent)
    );
    for original in [
        UserEvent::HighlightsReady,
        UserEvent::WrapReady,
        UserEvent::StartupFontReady,
        UserEvent::SearchReady,
        UserEvent::PaletteReady,
        UserEvent::SettingsPanelReady,
        UserEvent::WelcomeReady,
        UserEvent::RegistrationReady,
        UserEvent::DocumentReady,
        UserEvent::LogReady,
    ] {
        assert_eq!(proxy.send_owned_wake(original), Ok(WakeDisposition::OffUiWake));
    }
    assert_eq!(proxy.send_event(UserEvent::OpenExternal), Err(SendRefusal::RefusedControl));
    match receiver.try_recv().unwrap() {
        UserEvent::Notice(original) => assert_eq!(original, "queued original"),
        _ => panic!("original live queue value"),
    }
    assert!(receiver.try_recv().is_err());
}

#[cfg(feature = "gpu")]
#[test]
fn late_gpu_error_retains_original_and_ok_does_not_retire_capability() {
    let (proxy, _receiver, _) = gate();
    let capability = proxy.reserve_callback(size_of::<CallbackCapability>()).unwrap();
    proxy.request_terminal();
    let end = deadline();
    let proof = proxy.wait_native_until(end).unwrap_or_else(|reason| panic!("quiescence: {reason:?}"));
    assert_eq!(proxy.send_event(UserEvent::GpuAvailable(Ok(()))), Err(SendRefusal::CancelledReadiness));
    assert!(lock(&shared(&proxy).state).slots[SlotKind::GpuCallback.index()].outstanding);
    let original = "exact late GPU error".to_owned();
    let identity = original.as_ptr();
    let diagnostic = proxy.retain_gpu_error(original).unwrap();
    assert_eq!(proxy.send_event(UserEvent::GpuAvailable(Err(diagnostic))), Err(SendRefusal::RefusedControl));
    drop(capability); // actual last callback capability, never readiness credit
    assert_eq!(proxy.wait_capabilities_until(end), Ok(()));
    assert_eq!(proxy.finish_success(&proof, end), Err(Reason::GpuError));
    let inspection = proxy.inspect().unwrap();
    assert_eq!(inspection.gpu_error(), Some("exact late GPU error"));
    assert_eq!(inspection.gpu_error().unwrap().as_ptr(), identity);
}

#[test]
fn queued_control_and_foreign_generation_cannot_resolve_original_obligation() {
    let (proxy, receiver, _) = gate();
    let endpoint = proxy.control_endpoint(SlotKind::FileDialog, size_of::<ControlEndpoint>()).unwrap();
    let receipt =
        endpoint.receipt(crate::file_dialog::Event::Ready, false).unwrap_or_else(|_| panic!("original Ready"));
    proxy.send_event(UserEvent::FileDialog(receipt)).unwrap();
    let (foreign, _foreign_receiver, _) = gate();
    let original = match receiver.try_recv().unwrap() {
        UserEvent::FileDialog(original) => original,
        _ => panic!("original receipt"),
    };
    assert!(!original.matches(&foreign, SlotKind::FileDialog));
    assert!(original.matches(&proxy, SlotKind::FileDialog));
    proxy.request_terminal();
    let end = deadline();
    let proof = proxy.wait_native_until(end).unwrap_or_else(|reason| panic!("quiescence: {reason:?}"));
    assert_eq!(proxy.finish_success(&proof, end), Err(Reason::UnresolvedOwner));
    // Actual matching Ready consumption is still NOT Stopped and cannot clear
    // the lifetime or rehabilitate the already failed combined verdict.
    original.consume(|event| assert!(matches!(event, crate::file_dialog::Event::Ready))).unwrap();
    assert!(lock(&shared(&proxy).state).slots[SlotKind::FileDialog.index()].outstanding);
    assert_eq!(proxy.inspect().unwrap().reason(), Some(Reason::UnresolvedOwner));
}

fn retain_host(app: crate::App) -> (Box<crate::shutdown::Failure>, crate::OriginUiOwners) {
    use crate::{App, shutdown};
    let App {
        document: mut failure,
        view,
        wrap,
        markdown,
        logs,
        highlighting,
        search_panel,
        search,
        external_inbox,
        external_open,
        font_thread,
        font_size,
        ui_font_thread,
        ui_font_size,
        registration_prep,
        welcome_worker,
        file_types,
        settings_panel,
        window_placement,
        window_latest,
        about,
        palette,
        gfx,
        desktop_window,
        file_dialogs,
        close_dialog,
        #[cfg(feature = "gpu")]
        gpu,
        ..
    } = app;
    failure.auxiliary = shutdown::AuxiliaryOwners {
        prep: registration_prep,
        welcome: welcome_worker,
        registration: file_types,
        settings: settings_panel,
        placement: window_placement.map(|service| (service, window_latest)),
        diagnostics: Some(about),
        palette,
        #[cfg(feature = "gpu")]
        gpu,
    };
    failure.font = font_thread;
    failure.font_size = font_size;
    failure.chrome_font = ui_font_thread;
    failure.chrome_font_size = ui_font_size;
    failure.attach(shutdown::Owners {
        view,
        wrap,
        markdown,
        logs,
        highlighting,
        search_panel,
        search,
        external_inbox,
        external_open,
    });
    (failure, (gfx, desktop_window, file_dialogs, close_dialog))
}

fn held_font_failed_receiver(caught_panic: bool) {
    use crate::{
        document_ui::{FontProduct, StartupFont},
        search_ui, shutdown,
    };
    let end = deadline();
    let root = crate::test_support::directory("kw66-held-font").unwrap();
    let (mut app, old_receiver) = search_ui::host_dispatch_tests::host("original");
    drop(old_receiver);
    let budget = app.wrap.budget().clone();
    let (sender, receiver) = mpsc::channel();
    let proxy = HostProxy::admitted_test(sender, &budget).unwrap();
    app.proxy = proxy.clone();
    app.document.notifications = Some(proxy.clone());
    app.document.deadline = Some(end);
    let drops = Rc::new(RefCell::new(Vec::new()));
    let mut origin = OriginLoopOwner::admit(
        Observed { original: receiver, name: "receiver", identity: 201, drops: drops.clone() },
        proxy.clone(),
        &budget,
    )
    .unwrap_or_else(|_| panic!("origin admission"));
    let (entered, callback_entered) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let wake = proxy.clone();
    let mut font = StartupFont::<usize>::admit(&budget).unwrap();
    font.spawn_with_wake(
        || Some(777),
        move || {
            entered.send(()).unwrap();
            held.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap();
            wake.send_owned_wake(UserEvent::StartupFontReady)
                .map(|_| ())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        },
    )
    .unwrap();
    callback_entered.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap();
    let product = font.try_take_product(); // real ready/done AND actual product take
    app.start_document_at(Some(root.join("state")));
    let transport = app.document.live.transport.as_ref().unwrap();
    proxy.request_terminal();
    transport.request_off_ui_wakes();
    let document_proof = transport.wait_off_ui_wakes_until(end).unwrap();
    let matching_document = document_proof.matches(transport);
    let native_proof = proxy.wait_native_until(end).unwrap_or_else(|reason| panic!("native quiescence: {reason:?}"));
    let (mut failure, original_owners) = retain_host(app);
    origin.retain_owners(Observed {
        original: original_owners,
        name: "origin owners",
        identity: 202,
        drops: drops.clone(),
    });
    let panic_identity = if caught_panic {
        let original: Box<dyn Any + Send> = Box::new("original caught loop panic".to_owned());
        let identity = std::ptr::from_ref(original.as_ref());
        let caught = catch_unwind(AssertUnwindSafe(|| resume_unwind(original))).unwrap_err();
        failure.panic = Some(caught);
        Some(identity)
    } else {
        failure.frontend_error = Some(crate::GuiError::Unavailable("original loop error".into()));
        None
    };
    // Zero native claims + matching document proof still cannot release a
    // failed receiver while this ORIGINAL font callback remains held.
    drop(origin);
    let retained_while_font_held = drops.borrow().is_empty();
    release.send(()).unwrap();
    let font_cleanup = font.settle_until(end); // actual original join/right retirement
    let cleanup = failure.retry();
    let panic_cleanup = if caught_panic { failure.notification_fixture_cleanup_retained() } else { Ok(()) };
    let final_result = failure.take_final_success();
    let same_panic =
        panic_identity.map(|identity| std::ptr::eq(identity, std::ptr::from_ref(failure.panic_payload().unwrap())));
    drop(native_proof);
    drop(font);
    std::fs::remove_dir_all(&root).unwrap();
    assert!(matches!(product, FontProduct::Ready(Some(777))));
    assert!(matching_document);
    assert!(retained_while_font_held);
    assert_eq!(font_cleanup, Ok(()));
    if caught_panic {
        assert_eq!(cleanup, Err(shutdown::Status::Panicked));
        assert_eq!(same_panic, Some(true));
    } else {
        assert_eq!(cleanup, Ok(()));
    }
    assert_eq!(panic_cleanup, Ok(()));
    assert!(final_result.is_err());
    assert_eq!(failure.deadline(), Some(end));
    assert!(drops.borrow().is_empty(), "failed verdict stays failed after actual original cleanup");
    assert!(Instant::now() < end);
}
#[test]
fn held_actual_font_callback_and_document_proof_keep_receiver_on_loop_error() {
    held_font_failed_receiver(false);
}
#[test]
fn held_actual_font_callback_and_document_proof_keep_receiver_on_caught_panic() {
    held_font_failed_receiver(true);
}

#[test]
fn actual_font_deadline_expiry_retains_receiver_after_separate_physical_cleanup() {
    use crate::document_ui::{FontSettlementReason, StartupFont};
    type Outcome = Rc<RefCell<Option<(Result<(), FontSettlementReason>, Result<(), FontSettlementReason>)>>>;
    struct Teardown {
        font: Option<StartupFont<usize>>,
        release: Option<mpsc::SyncSender<()>>,
        original_deadline: Instant,
        outcome: Outcome,
    }
    impl Drop for Teardown {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            // ONE separate explicit fixture cleanup allowance; never a new
            // production settlement deadline. Drop also runs on assertion unwind.
            let cleanup_deadline = Instant::now() + Duration::from_secs(5);
            let mut font = self.font.take().unwrap();
            let cleanup = font.notification_fixture_cleanup_until(cleanup_deadline);
            let unchanged = font.settle_until(self.original_deadline);
            *self.outcome.borrow_mut() = Some((cleanup, unchanged));
            if cleanup.is_err() {
                std::mem::forget(font);
            } // retain exact refused owner/right/charge
        }
    }
    let (proxy, receiver, budget) = gate();
    let drops = Rc::new(RefCell::new(Vec::new()));
    let origin = OriginLoopOwner::<_, ()>::admit(
        Observed { original: receiver, name: "receiver", identity: 301, drops: drops.clone() },
        proxy.clone(),
        &budget,
    )
    .unwrap_or_else(|_| panic!("original receiver admission"));
    let end = deadline();
    let (entered, callback_entered) = mpsc::sync_channel(1);
    let (release, held) = mpsc::sync_channel(1);
    let wake = proxy.clone();
    let mut font = StartupFont::<usize>::admit(&budget).unwrap();
    font.spawn_with_wake(
        || Some(313),
        move || {
            entered.send(()).unwrap();
            // The teardown release, not an automatic test-thread timeout, owns this
            // gate. Its receive still has the original + bounded teardown allowance.
            held.recv_timeout(end.saturating_duration_since(Instant::now()) + Duration::from_secs(5))
                .map_err(std::io::Error::other)?;
            wake.send_owned_wake(UserEvent::StartupFontReady)
                .map(|_| ())
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        },
    )
    .unwrap();
    callback_entered.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap();
    proxy.request_terminal();
    drop(origin); // failed owner is retained while the callback is still held
    let outcome: Outcome = Rc::new(RefCell::new(None));
    let mut teardown =
        Teardown { font: Some(font), release: Some(release), original_deadline: end, outcome: outcome.clone() };
    let first = teardown.font.as_mut().unwrap().settle_until(end);
    assert_eq!(first, Err(FontSettlementReason::TimedOut), "first public verdict precedes fixture cleanup");
    drop(teardown); // also guaranteed by Drop if the FIRST assertion unwinds
    let (cleanup, unchanged) = outcome.borrow_mut().take().unwrap();
    assert_eq!(cleanup, Ok(()));
    assert_eq!(unchanged, Err(FontSettlementReason::TimedOut));
    assert!(drops.borrow().is_empty(), "late physical retirement cannot authorize receiver release");
    assert_eq!(proxy.inspect().unwrap().reason(), Some(Reason::FailedVerdict));
}

/// The original protocol owners, not ready/done flags, must authorize release.
/// The GPU is an actual windowless Gpu; native device/dialog qualification is
/// deliberately outside this control.
#[cfg(feature = "gpu")]
#[test]
fn complete_shutdown_releases_original_receiver_and_origin_owners_exactly_once() {
    use crate::{App, GpuNotificationCallback, close_dialog, file_dialog, file_prompt, gpu, search_ui, shutdown};
    use kwil_core_commands::associations::{
        AssociationAdapter, AssociationError, AssociationPlatform, DefaultChoice, Location, StoredValue, ValueBudget,
    };
    use kwil_view_model::file_type_registration::{FileTypeRegistration, RegistrationContext, RegistrationLimits};
    struct NoNative;
    impl AssociationAdapter for NoNative {
        fn owner(&self) -> String {
            "windowless-notification-control".into()
        }
        fn read(&mut self, _: &Location) -> Result<Option<StoredValue>, AssociationError> {
            Ok(None)
        }
        fn read_bounded(&mut self, _: &Location, _: &mut ValueBudget) -> Result<Option<StoredValue>, AssociationError> {
            Ok(None)
        }
        fn write(&mut self, _: &Location, _: Option<&StoredValue>) -> Result<(), AssociationError> {
            panic!("this control admits no association mutation")
        }
    }
    let end = deadline();
    let origin_thread = thread::current().id();
    let root = crate::test_support::directory("kw66-notification-release").unwrap();
    let (mut app, old_receiver) = search_ui::host_dispatch_tests::host("original dirty document");
    drop(old_receiver);
    let budget = app.wrap.budget().clone();
    let (sender, receiver) = mpsc::channel();
    let proxy = HostProxy::admitted_test(sender, &budget).unwrap();
    app.proxy = proxy.clone();
    app.document.notifications = Some(proxy.clone());
    app.document.deadline = Some(end);
    let drops = Rc::new(RefCell::new(Vec::new()));
    let mut origin = OriginLoopOwner::admit(
        Observed { original: receiver, name: "receiver", identity: 101, drops: drops.clone() },
        proxy.clone(),
        &budget,
    )
    .unwrap_or_else(|_| panic!("original receiver admission"));
    fn next(receiver: &mpsc::Receiver<UserEvent>, end: Instant) -> UserEvent {
        loop {
            match receiver.recv_timeout(end.saturating_duration_since(Instant::now())).unwrap() {
                UserEvent::RegistrationReady => {}
                original => return original,
            }
        }
    }

    // Reserve before either producer can create its original active owner.
    let registration = proxy
        .reserve(SlotKind::Registration, 2 * size_of::<Delivery<Result<Box<FileTypeRegistration>, String>>>())
        .unwrap();
    registration.start();
    let wake = proxy.clone();
    let service = FileTypeRegistration::start(
        root.join("association-journal"),
        RegistrationContext {
            platform: AssociationPlatform::Windows,
            executable: root.join("kwil.exe"),
            content_types: Default::default(),
            default_choice: DefaultChoice::KeepCurrent,
            limits: RegistrationLimits::Base128,
        },
        || Ok(NoNative),
        Arc::new(move || {
            let _ = wake.send_event(UserEvent::RegistrationReady);
        }),
    )
    .unwrap();
    proxy.send_event(UserEvent::RegistrationStarted(registration.publish(Ok(Box::new(service))))).unwrap();
    let transfer = proxy.reserve(SlotKind::Gpu, 2 * size_of::<Delivery<Result<Box<gpu::Gpu>, String>>>()).unwrap();
    let callback = GpuNotificationCallback {
        capability: proxy.reserve_callback(2 * size_of::<GpuNotificationCallback>()).unwrap(),
    };
    transfer.start();
    let (original_gpu, fixture) = gpu::notification_fixture(move |result| callback.send(result), end);
    let gpu_identity = fixture.identity;
    proxy.send_event(UserEvent::GpuReady(transfer.publish(Ok(Box::new(original_gpu))), 0.0)).unwrap();
    while app.file_types.is_none() || app.gpu.is_none() {
        let event = next(&origin.receiver_mut().original, end);
        match event {
            UserEvent::RegistrationStarted(result) => {
                app.adopt_registration_notification(result);
            }
            UserEvent::GpuReady(result, ms) => app.adopt_gpu_notification(result, ms),
            UserEvent::RegistrationReady => {}
            _ => panic!("unexpected startup notification"),
        }
    }
    assert_eq!(app.gpu.as_ref().unwrap().notification_fixture_identity(), gpu_identity);

    // Actual production receipt handlers, original captured request/ticket, and
    // the existing fake Port/Gate effects. No direct stopped-state mutation.
    let emit = file_prompt::notification_fixture_sender(proxy.clone());
    let mut dialog = file_dialog::NotificationFixture::new(end);
    dialog.ready(&emit);
    let event = next(&origin.receiver_mut().original, end);
    match event {
        UserEvent::FileDialog(receipt) => assert!(app.consume_file_dialog_notification(receipt)),
        _ => panic!("matching Ready"),
    }
    app.view.file_command("file.save").unwrap();
    dialog.submit(&mut app.file_dialogs, &mut app.view);
    dialog.shutdown(&mut app.file_dialogs, &mut app.view, &emit);
    for _ in 0..2 {
        let event = next(&origin.receiver_mut().original, end);
        match event {
            UserEvent::FileDialog(receipt) => assert!(app.consume_file_dialog_notification(receipt)),
            _ => panic!("matching Completed/Stopped"),
        }
    }
    assert!(file_dialog::NotificationFixture::is_stopped(&app.file_dialogs));
    drop(emit);
    close_dialog::notification_fixture_reply(&mut app, end);
    let event = next(&origin.receiver_mut().original, end);
    match event {
        UserEvent::CloseDialog(receipt) => assert!(app.consume_close_dialog_notification(receipt)),
        _ => panic!("matching confirmation receipt"),
    }
    assert!(app.close_dialog.is_none());

    // A real DocumentWorker supplies the matching handoff/source barriers and
    // original join/native-right retirement in Failure::retry, under SAME end.
    app.start_document_at(Some(root.join("state")));
    assert!(app.document.live.ever_started && app.document.live.transport.is_some());
    app.external_inbox.close();
    proxy.request_terminal();
    let App {
        document: mut failure,
        view,
        wrap,
        markdown,
        logs,
        highlighting,
        search_panel,
        search,
        external_inbox,
        external_open,
        font_thread,
        font_size,
        ui_font_thread,
        ui_font_size,
        registration_prep,
        welcome_worker,
        file_types,
        settings_panel,
        window_placement,
        window_latest,
        about,
        palette,
        gpu,
        gfx,
        desktop_window,
        file_dialogs,
        close_dialog,
        ..
    } = app;
    origin.retain_owners(Observed {
        original: (gfx, desktop_window, file_dialogs, close_dialog, dialog),
        name: "origin owners",
        identity: 102,
        drops: drops.clone(),
    });
    failure.auxiliary = shutdown::AuxiliaryOwners {
        prep: registration_prep,
        welcome: welcome_worker,
        registration: file_types,
        settings: settings_panel,
        placement: window_placement.map(|service| (service, window_latest)),
        diagnostics: Some(about),
        palette,
        gpu,
    };
    failure.font = font_thread;
    failure.font_size = font_size;
    failure.chrome_font = ui_font_thread;
    failure.chrome_font_size = ui_font_size;
    failure.attach(shutdown::Owners {
        view,
        wrap,
        markdown,
        logs,
        highlighting,
        search_panel,
        search,
        external_inbox,
        external_open,
    });
    let cleanup = failure.retry();
    assert_eq!(cleanup, Ok(()), "actual existing cleanup: {failure}");
    assert_eq!(failure.deadline(), Some(end));
    let success =
        failure.take_final_success().unwrap_or_else(|reason| panic!("combined success: {reason:?}; {failure}"));
    let released = origin.release(success);
    fixture.finish(end); // original test supervisor/renderer cleaned before final assertions
    drop(failure);
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(released, Ok(()));
    assert_eq!(drops.borrow().as_slice(), &[("origin owners", 102, origin_thread), ("receiver", 101, origin_thread)]);
    assert!(Instant::now() < end);
}
