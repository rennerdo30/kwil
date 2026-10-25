#[test]
fn utf32_bom_checked_open_takes_text_route_without_binary_offer() {
    for raw in [
        b"\xff\xfe\0\0A\0\0\0\0\xf6\x01\0\r\0\0\0\n\0\0\0".as_slice(),
        b"\0\0\xfe\xff\0\0\0A\0\x01\xf6\0\0\0\0\r\0\0\0\n".as_slice(),
    ] {
        let fixture = Fixture::new();
        let (path, file) = fixture.file(raw);
        let (worker, rx) = start(&fixture);
        drop(file);
        worker.try_submit(DocumentRequest::OpenChecked { ticket: ticket(1, 1), path, file: None }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let document = loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                DocumentResult::BinaryOffer(_) => panic!("UTF32 BOM text must precede binary offer"),
                DocumentResult::CheckedPreview { preview, .. } => assert_eq!(preview.text, "A😀\r\n".as_bytes()),
                DocumentResult::CheckedOpened { result, .. } => break result.unwrap().unwrap(),
                _ => (),
            }
        };
        assert_eq!(logical(&document), "A😀\r\n".as_bytes());
        drop(document);
        finish(worker, &rx);
    }
}
