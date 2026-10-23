//! Source fixtures only. Run individually with --test-threads=1 after ROOT's
//! integration/review; no registry or desktop databases are accessed.
use super::*;

// Synthetic safe extensions retain the exact 240/256 capacity and restoration witnesses.
pub(super) fn all240_request() -> AssociationRequest {
    AssociationRequest {
        platform: AssociationPlatform::Windows,
        executable: std::env::temp_dir().join("kwil-budget-fixture.exe"),
        types: (0..240).map(|n| FileType { extension: format!("safe_fixture{n}"), content_type: None }).collect(),
        default_choice: DefaultChoice::KeepCurrent,
    }
}
fn counted_request(count: usize) -> AssociationRequest {
    let mut request = all240_request();
    request.types = (0..count).map(|n| FileType { extension: format!("bounded{n}"), content_type: None }).collect();
    request
}
#[derive(Default)]
struct Fake {
    values: Vec<(Location, StoredValue)>,
    reads: usize,
    copies: usize,
    writes: usize,
    fail_from: Option<usize>,
}
impl AssociationAdapter for Fake {
    fn owner(&self) -> String {
        "budget-fixture-user".into()
    }
    fn read(&mut self, _: &Location) -> Result<Option<StoredValue>, AssociationError> {
        panic!("the admitted path must never call legacy read")
    }
    fn read_bounded(
        &mut self,
        location: &Location,
        budget: &mut ValueBudget,
    ) -> Result<Option<StoredValue>, AssociationError> {
        self.reads += 1;
        self.values
            .iter()
            .find(|(p, _)| p == location)
            .map(|(_, v)| {
                let admitted = budget.copy(v.kind, &v.bytes)?;
                self.copies += 1;
                Ok(admitted)
            })
            .transpose()
    }
    fn write(&mut self, location: &Location, value: Option<&StoredValue>) -> Result<(), AssociationError> {
        self.writes += 1;
        if self.fail_from.is_some_and(|n| self.writes >= n) {
            return Err(err("injected write/cleanup failure"));
        }
        self.values.retain(|(p, _)| p != location);
        if let Some(value) = value {
            self.values.push((location.clone(), value.clone()));
        }
        Ok(())
    }
}

#[test]
fn budget_all240_and_256_roundtrip_exact_typed_originals() {
    let _admission_case = crate::associations::test_admission_guard();
    for request in [all240_request(), counted_request(256)] {
        let expected = request.types.len() * 3;
        assert!(matches!(request.types.len(), 240 | 256));
        let mut fake = Fake::default();
        let original_plan = plan(&mut fake, &request).unwrap();
        assert_eq!(original_plan.changes().len(), expected);
        let originals: Vec<_> = original_plan
            .changes()
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 3 != 2)
            .map(|(i, c)| {
                (c.location.clone(), StoredValue { kind: 19 + i as u32, bytes: vec![0, 255, i as u8, 0, 128] })
            })
            .collect();
        fake.values = originals.clone();
        drop(original_plan);
        let reviewed = plan(&mut fake, &request).unwrap();
        assert_eq!(reviewed.changes().len(), expected);
        assert_eq!(fake.writes, 0);
        let mut receipt = apply(&mut fake, &reviewed).unwrap();
        assert_eq!(receipt.pending().len(), expected);
        assert_eq!(fake.writes, expected);
        assert!(plan(&mut fake, &request).unwrap().changes().is_empty());
        assert!(rollback(&mut fake, &mut receipt).is_empty());
        assert!(receipt.is_rolled_back());
        assert_eq!(fake.values.len(), originals.len());
        for (location, original) in &originals {
            assert_eq!(fake.values.iter().find(|(p, _)| p == location).map(|(_, v)| v), Some(original));
        }
    }
}

#[test]
fn budget_257_refuses_before_any_native_read() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut fake = Fake::default();
    assert!(plan(&mut fake, &counted_request(257)).is_err());
    assert_eq!((fake.reads, fake.copies, fake.writes), (0, 0, 0));
}

#[test]
fn budget_native_value_denial_precedes_allocation_and_fake_clone() {
    let _admission_case = crate::associations::test_admission_guard();
    let location = Location::WindowsValue { subkey: "Kwil.txt".into(), name: String::new() };
    let mut fake =
        Fake { values: vec![(location.clone(), StoredValue { kind: 91, bytes: vec![255; 9] })], ..Fake::default() };
    let mut budget = ValueBudget::new(8);
    assert!(fake.read_bounded(&location, &mut budget).is_err());
    assert_eq!(fake.copies, 0);
    assert_eq!(budget.remaining(), 8);
    assert!(budget.allocate(MAX_VALUE_BYTES + 1).is_err());
    assert_eq!(budget.remaining(), 8);
    fake.values[0].1.bytes = vec![255; MAX_VALUE_BYTES + 1];
    let mut per_value = ValueBudget::new(worker::MAX_RECORD_BYTES);
    assert!(fake.read_bounded(&location, &mut per_value).is_err());
    assert_eq!(fake.copies, 0);
    assert_eq!(per_value.remaining(), worker::MAX_RECORD_BYTES);
    let mut exact = ValueBudget::new(MAX_VALUE_BYTES);
    assert_eq!(exact.allocate(MAX_VALUE_BYTES).unwrap().len(), MAX_VALUE_BYTES);
    assert_eq!(exact.remaining(), 0);
    assert!(exact.allocate(1).is_err());
}

#[test]
fn budget_one_mib_original_roundtrips_with_256_types() {
    let _admission_case = crate::associations::test_admission_guard();
    let request = counted_request(256);
    let mut fake = Fake::default();
    let seed = plan(&mut fake, &request).unwrap();
    let location = seed.changes()[0].location.clone();
    drop(seed);
    let original = StoredValue { kind: 91, bytes: vec![255; MAX_VALUE_BYTES] };
    fake.values.push((location.clone(), original.clone()));
    let reviewed = plan(&mut fake, &request).unwrap();
    assert_eq!(reviewed.changes().len(), 768);
    assert_eq!(reviewed.changes()[0].before.as_ref(), Some(&original));
    let mut receipt = apply(&mut fake, &reviewed).unwrap();
    assert!(rollback(&mut fake, &mut receipt).is_empty());
    assert_eq!(fake.values, vec![(location, original)]);
}

#[test]
fn budget_aggregate_read_denial_precedes_next_clone() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut request = counted_request(1);
    let mut fake = Fake::default();
    let seed = plan(&mut fake, &request).unwrap();
    let location = seed.changes()[0].location.clone();
    fake.values.push((location.clone(), StoredValue { kind: 1, bytes: vec![2; 7] }));
    let mut aggregate = ValueBudget::new(10);
    assert_eq!(fake.read_bounded(&location, &mut aggregate).unwrap().unwrap().bytes.len(), 7);
    assert!(fake.read_bounded(&location, &mut aggregate).is_err());
    assert_eq!((aggregate.remaining(), fake.copies), (3, 1));
    request.default_choice = DefaultChoice::MakeDefault;
    let reads = fake.reads;
    assert!(plan(&mut fake, &request).is_err());
    assert_eq!(fake.reads, reads, "protected defaults cannot become ordinary writes");
}

#[test]
fn budget_snapshots_share_admission_and_overlap_refuses_before_copy() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut fake = Fake::default();
    let original = plan(&mut fake, &counted_request(1)).unwrap();
    let snapshot = original.clone();
    assert!(Arc::ptr_eq(original.changes.0.as_ref().unwrap(), snapshot.changes.0.as_ref().unwrap()));
    let used = ASSOCIATION_MEMORY.load(Ordering::Acquire);
    let hold = Admission::acquire(MAX_ASSOCIATION_MEMORY - used).unwrap();
    assert!(original.changes.copied().is_err());
    assert_eq!(original.changes()[0], snapshot.changes()[0]);
    assert_eq!(fake.writes, 0);
    drop(original);
    assert_eq!(snapshot.changes().len(), 3, "last retained snapshot still owns its bytes/reservation");
    drop(hold);
    let (copied, _admission) = snapshot.changes.copied().unwrap();
    assert_eq!(copied, snapshot.changes());
}

#[test]
fn budget_failed_cleanup_retains_only_successful_original_ownership() {
    let _admission_case = crate::associations::test_admission_guard();
    let mut fake = Fake::default();
    let reviewed = plan(&mut fake, &counted_request(1)).unwrap();
    fake.fail_from = Some(2);
    let mut failure = apply(&mut fake, &reviewed).unwrap_err();
    assert_eq!(failure.receipt.pending(), &reviewed.changes()[..1]);
    assert_eq!(fake.values.len(), 1);
    let location = failure.receipt.pending()[0].location.clone();
    fake.values[0].1 = StoredValue { kind: 77, bytes: vec![7, 0, 255] };
    fake.fail_from = None;
    assert_eq!(rollback(&mut fake, &mut failure.receipt).len(), 1);
    assert_eq!(fake.values[0].1.kind, 77, "foreign value must survive cleanup");
    fake.values[0].1 = failure.receipt.pending()[0].after.clone().unwrap();
    assert_eq!(fake.values[0].0, location);
    assert!(rollback(&mut fake, &mut failure.receipt).is_empty());
    assert!(failure.receipt.is_rolled_back());
    assert!(fake.values.is_empty());
}

#[test]
fn budget_legacy_adapter_fails_closed_before_unbounded_read() {
    let _admission_case = crate::associations::test_admission_guard();
    struct Legacy;
    impl AssociationAdapter for Legacy {
        fn owner(&self) -> String {
            "legacy".into()
        }
        fn read(&mut self, _: &Location) -> Result<Option<StoredValue>, AssociationError> {
            panic!("unbounded read")
        }
        fn write(&mut self, _: &Location, _: Option<&StoredValue>) -> Result<(), AssociationError> {
            panic!("native write")
        }
    }
    assert!(plan(&mut Legacy, &counted_request(1)).unwrap_err().0.contains("budget-before-read"));
}
