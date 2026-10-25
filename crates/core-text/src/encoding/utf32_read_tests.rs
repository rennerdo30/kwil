#[test]
fn utf32_sealed_read_lease_keeps_original_atoms_and_one_byte_query_progress() {
    use crate::encoding::{Encoding, utf32_tests::raw};
    for (encoding, key) in [(Encoding::UTF32_LE, 39), (Encoding::UTF32_BE, 40)] {
        let bytes = raw(encoding, &[65, 13, 10, 0x1f600, 0xd800], true, &[0xaa, 0xbb]);
        let (store, budget) = decoded_fixture(&bytes, encoding, 4);
        let read = lease(&store, &budget);
        let mut lookup = AtomLookup::new(7);
        let mut atom = None;
        // Each record byte needs one read credit and one decode credit.
        let polls = ReadLease::<Budget>::search_records(read.atom_count()) * (2 * ATOM_RECORD as u64) + 1;
        for _ in 0..polls {
            match read.atom_span(&mut lookup, &mut ReadWork::new(1, 1), &cancel()) {
                Step::Pending => {}
                Step::Ready(value) => {
                    atom = value;
                    break;
                }
                other => panic!("UTF32 admitted atom query: {other:?}"),
            }
        }
        let atom = atom.unwrap();
        assert_eq!(atom.encoding, key);
        assert_eq!(atom.kind, CaptureKind::Opaque);
        assert_eq!(atom.original_end - atom.original_start, 4);
        let mut original = [0; 4];
        ready(read.read_at(PayloadSpace::Original, atom.original_start, &mut original, &mut work(), &cancel()));
        assert_eq!(original.as_slice(), raw(encoding, &[0xd800], false, &[]));
        let tail = ready(read.atom_span(&mut AtomLookup::new(8), &mut work(), &cancel())).unwrap();
        assert_eq!(tail.original_end - tail.original_start, 2);
        assert_eq!(
            ready(read.line_span(&mut LineLookup::new(0), &mut work(), &cancel())),
            LineSpan { start: 0, content_end: 1, end: 3, terminator: super::super::Terminator::CrLf }
        );
        let mut out = [0xa5; 4];
        assert!(matches!(
            read.read_at(PayloadSpace::Logical, 0, &mut out, &mut work(), &AtomicBool::new(true)),
            Step::Cancelled
        ));
        assert_eq!(out, [0xa5; 4]);
        close_lease(read.into_retirement());
        close_store(store, &budget);
    }
}
