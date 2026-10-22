use kwil_core_diff::*;
fn model(a: &[&[u8]], b: &[&[u8]], options: Options) -> Comparison {
    compare(&a, &b, &options, &CancellationToken::default()).unwrap()
}
// A sized adapter around a slice also exercises the object-safe input boundary.
impl LineSource for Source<'_> {
    fn line_count(&self) -> usize {
        self.0.len()
    }
    fn line(&self, i: usize) -> &[u8] {
        self.0[i]
    }
}
struct Source<'a>(&'a [&'a [u8]]);
#[test]
fn insert_delete_modify_alignment() {
    for algorithm in [Algorithm::Histogram, Algorithm::Myers] {
        let a: &[&[u8]] = &[b"a", b"removed", b"b", b"old", b"c"];
        let b: &[&[u8]] = &[b"a", b"b", b"new", b"c", b"added"];
        let m = model(a, b, Options { algorithm, ..Options::default() });
        let rows: Vec<_> = (0..m.row_count()).map(|i| m.row(i).unwrap()).collect();
        assert_eq!(
            rows,
            vec![
                Row { left: Some(0), right: Some(0), kind: Kind::Equal },
                Row { left: Some(1), right: None, kind: Kind::Delete },
                Row { left: Some(2), right: Some(1), kind: Kind::Equal },
                Row { left: Some(3), right: Some(2), kind: Kind::Modify },
                Row { left: Some(4), right: Some(3), kind: Kind::Equal },
                Row { left: None, right: Some(4), kind: Kind::Insert },
            ]
        );
        assert_eq!(m.hunks.len(), 3);
        assert_eq!(m.hunks[1].left, 3..4);
        assert_eq!(m.hunks[1].right, 2..3);
        assert!(m.row(m.row_count()).is_none());
    }
    assert_eq!(model(&[], &[], Options::default()).row_count(), 0);
    assert_eq!(model(&[], &[b"x"], Options::default()).row(0).unwrap().kind, Kind::Insert);
    assert_eq!(model(&[b"x"], &[], Options::default()).row(0).unwrap().kind, Kind::Delete);
}
#[test]
fn whitespace_modes_and_line_endings_are_independent() {
    for (mode, a, b) in [
        (Whitespace::All, &b" a \t b \n"[..], &b"ab\n"[..]),
        (Whitespace::Trailing, &b"a \t\r\n"[..], &b"a\r\n"[..]),
        (Whitespace::Amount, &b"a \t b\n"[..], &b"a b\n"[..]),
    ] {
        assert_eq!(model(&[a], &[b], Options::default()).hunks.len(), 1);
        assert!(model(&[a], &[b], Options { whitespace: mode, ..Options::default() }).hunks.is_empty());
    }
    let opts = Options { whitespace: Whitespace::Trailing, ..Options::default() };
    assert_eq!(model(&[b"a\r\n"], &[b"a\n"], opts.clone()).hunks.len(), 1);
    assert!(model(&[b"a\r\n"], &[b"a\n"], Options { ignore_line_endings: true, ..opts }).hunks.is_empty());
    assert_eq!(model(&[b"a"], &[b"a\n"], Options { ignore_line_endings: true, ..Options::default() }).hunks.len(), 1);
    assert_eq!(
        model(&[b"a b"], &[b"ab"], Options { whitespace: Whitespace::Amount, ..Options::default() }).hunks.len(),
        1
    );
    assert_eq!(
        model(&[b"a b"], &[b"a  b"], Options { whitespace: Whitespace::Trailing, ..Options::default() }).hunks.len(),
        1
    );
}
#[test]
fn word_ranges_address_original_bytes() {
    let (a, b) = (Source(&[b"hello old world"]), Source(&[b"hello new world"]));
    let m = compare(&a, &b, &Options::default(), &CancellationToken::default()).unwrap();
    let words = m.refine(0, &a, &b, &CancellationToken::default()).unwrap().unwrap();
    assert_eq!(words.left, vec![6..9]);
    assert_eq!(words.right, vec![6..9]);
    // Invalid UTF-8 remains byte-exact and never needs a full decoded string.
    let (a, b) = (Source(&[b"\xff old"]), Source(&[b"\xff new"]));
    let m = compare(&a, &b, &Options::default(), &CancellationToken::default()).unwrap();
    assert_eq!(m.refine(0, &a, &b, &CancellationToken::default()).unwrap().unwrap().left, vec![2..5]);
}
#[test]
fn refinement_has_line_and_hunk_bounds() {
    let line = vec![b'a'; MAX_WORD_LINE_BYTES + 1];
    let (a, b) = (Source(&[&line]), Source(&[b"new"]));
    let m = compare(&a, &b, &Options::default(), &CancellationToken::default()).unwrap();
    assert!(m.refine(0, &a, &b, &CancellationToken::default()).unwrap().is_none());
    let lines = vec![&b"old"[..]; MAX_WORD_HUNK_LINES + 1];
    let new = vec![&b"new"[..]; MAX_WORD_HUNK_LINES + 1];
    let (a, b) = (Source(&lines), Source(&new));
    let m = compare(&a, &b, &Options::default(), &CancellationToken::default()).unwrap();
    assert!(m.refine(0, &a, &b, &CancellationToken::default()).unwrap().is_none());
}
#[test]
fn collapse_and_navigation_use_alignment_rows() {
    let a: &[&[u8]] = &[b"a", b"b", b"old", b"d", b"e", b"f", b"old2", b"h", b"i"];
    let b: &[&[u8]] = &[b"a", b"b", b"new", b"d", b"e", b"f", b"new2", b"h", b"i"];
    let m = model(a, b, Options::default());
    assert_eq!(
        m.collapsed(1),
        vec![
            DisplaySegment::Collapsed(0..1),
            DisplaySegment::Rows(1..4),
            DisplaySegment::Collapsed(4..5),
            DisplaySegment::Rows(5..8),
            DisplaySegment::Collapsed(8..9)
        ]
    );
    assert_eq!(m.collapsed(2), vec![DisplaySegment::Rows(0..9)]);
    assert_eq!(m.next_hunk(0).unwrap().rows, 2..3);
    assert_eq!(m.next_hunk(2).unwrap().rows, 6..7);
    assert!(m.next_hunk(6).is_none());
    assert_eq!(m.previous_hunk(6).unwrap().rows, 2..3);
    assert_eq!(m.previous_hunk(7).unwrap().rows, 6..7);
    assert!(m.previous_hunk(2).is_none());
    assert_eq!(model(a, a, Options::default()).collapsed(1), vec![DisplaySegment::Collapsed(0..9)]);
}
#[test]
fn size_guards_precede_reads_and_bound_bytes() {
    struct Huge;
    impl LineSource for Huge {
        fn line_count(&self) -> usize {
            usize::MAX
        }
        fn line(&self, _: usize) -> &[u8] {
            panic!("guard must precede reads")
        }
    }
    assert_eq!(
        compare(&Huge, &Source(&[]), &Options::default(), &CancellationToken::default()).unwrap_err(),
        DiffError::TooLarge
    );
    assert_eq!(
        compare(&Huge, &Huge, &Options { max_lines: usize::MAX, ..Options::default() }, &CancellationToken::default())
            .unwrap_err(),
        DiffError::TooLarge
    );
    let line = vec![b'x'; MAX_LINE_BYTES + 1];
    assert_eq!(
        compare(&Source(&[&line]), &Source(&[]), &Options::default(), &CancellationToken::default()).unwrap_err(),
        DiffError::TooLarge
    );
    let line = vec![b'x'; MAX_LINE_BYTES];
    let lines = vec![line.as_slice(); 65];
    assert_eq!(
        compare(&Source(&lines), &Source(&[]), &Options::default(), &CancellationToken::default()).unwrap_err(),
        DiffError::TooLarge
    );
    assert_eq!(
        compare(
            &Source(&[b"a"]),
            &Source(&[b"b"]),
            &Options { max_lines: 1, ..Options::default() },
            &CancellationToken::default()
        )
        .unwrap_err(),
        DiffError::TooLarge
    );
}
#[test]
fn cancellation_before_and_during_source_reads() {
    let token = CancellationToken::default();
    token.cancel();
    assert_eq!(compare(&Source(&[]), &Source(&[]), &Options::default(), &token).unwrap_err(), DiffError::Cancelled);
    struct Cancelling(CancellationToken);
    impl LineSource for Cancelling {
        fn line_count(&self) -> usize {
            10
        }
        fn line(&self, _: usize) -> &[u8] {
            self.0.cancel();
            b"x"
        }
    }
    let token = CancellationToken::default();
    assert_eq!(
        compare(&Cancelling(token.clone()), &Source(&[]), &Options::default(), &token).unwrap_err(),
        DiffError::Cancelled
    );
}
