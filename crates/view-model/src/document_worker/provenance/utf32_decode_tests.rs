#[test]
fn utf32_capsule_definition_four_byte_bom_and_single_byte_reader_keep_framing() {
    for (encoding, raw) in [
        (Encoding::UTF32_LE, b"\xff\xfe\0\0A\0\0\0\0\xd8\0\0\xaa".as_slice()),
        (Encoding::UTF32_BE, b"\0\0\xfe\xff\0\0\0A\0\0\xd8\0\xaa".as_slice()),
    ] {
        let mut bytes = header(3);
        definition(&mut bytes, 1, encoding, 4, raw);
        reference(&mut bytes, 1, 0, 3);
        bytes.push(0);
        let physical = bytes.len();
        bytes.extend_from_slice(b"NEXT");
        struct Single<'a>(&'a [u8]);
        impl Read for Single<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let n = output.len().min(self.0.len()).min(1);
                output[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let mut reader = Single(&bytes);
        let mut sink = Sink::default();
        decode_capsule(&mut reader, physical as u64, 3, &limits(), &AtomicBool::new(false), &mut sink).unwrap();
        assert_eq!(reader.0, b"NEXT");
        assert_eq!(sink.origins[&1], raw);
        assert_eq!(sink.refs, vec![(1, 0..3)]);
        assert_eq!(sink.ended, Some(3));
        let mut invalid = header(3);
        definition(&mut invalid, 1, encoding, 2, raw);
        reference(&mut invalid, 1, 0, 3);
        invalid.push(0);
        let mut sink = Sink::default();
        assert!(run(&invalid, 3, &mut sink).is_err());
        assert!(sink.aborted);
        assert!(sink.origins.is_empty());
    }
}
