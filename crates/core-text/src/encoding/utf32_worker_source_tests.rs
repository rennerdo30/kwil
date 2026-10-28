#[test]
fn utf32_worker_capture_one_byte_prepayment_and_cancel_keep_original_owner() {
    use crate::encoding::utf32_tests::{Fixture, raw};
    let fixture = Fixture::new();
    for encoding in [Encoding::UTF32_LE, Encoding::UTF32_BE] {
        let bytes = raw(encoding, &[65, 0x1f600, 0xd800], true, &[0xaa, 0xbb, 0xcc]);
        let buffer = fixture.buffer(&bytes, encoding, 4);
        let mut source = cursor(buffer.snapshot(), 0..buffer.len());
        let mut payment: Option<(usize, usize)> = None;
        let mut captured = Vec::new();
        let mut finished = false;
        for _ in 0..4096 {
            let mut credit = 1;
            let mut operations = 1;
            let mut logical = [0xa5; 8];
            let mut raw_bytes = [0x5a; 4];
            match source
                .fill(&mut logical, &mut raw_bytes, &AtomicBool::new(false), |requested| {
                    let (width, paid) = payment.get_or_insert((requested, 0));
                    assert_eq!(*width, requested);
                    let n = (requested - *paid).min(credit);
                    *paid += n;
                    credit -= n;
                    if *paid != requested || operations == 0 {
                        return Ok(false);
                    }
                    operations -= 1;
                    payment = None;
                    Ok(true)
                })
                .unwrap()
            {
                WorkerSourceStep::Pending => {
                    assert_eq!(logical, [0xa5; 8]);
                    assert_eq!(raw_bytes, [0x5a; 4]);
                }
                WorkerSourceStep::Ready(Some(item)) => captured.push((
                    item.kind,
                    logical[..item.logical_len].to_vec(),
                    raw_bytes[..item.raw_len].to_vec(),
                    item.encoding,
                )),
                WorkerSourceStep::Ready(None) => {
                    finished = true;
                    break;
                }
            }
        }
        assert!(finished);
        assert_eq!(captured.len(), 4);
        assert_eq!(
            captured[1],
            (
                WorkerSourceKind::Preserved,
                "😀".as_bytes().to_vec(),
                raw(encoding, &[0x1f600], false, &[]),
                Some(encoding)
            )
        );
        assert_eq!(
            captured[2],
            (WorkerSourceKind::Opaque, vec![0xff], raw(encoding, &[0xd800], false, &[]), Some(encoding))
        );
        assert_eq!(captured[3], (WorkerSourceKind::Opaque, vec![0xff], vec![0xaa, 0xbb, 0xcc], Some(encoding)));
        drop(source.into_snapshot());
        let mut source = cursor(buffer.snapshot(), 0..buffer.len());
        let mut logical = [0xa5; 8];
        let mut original = [0x5a; 4];
        assert_eq!(
            source.fill(&mut logical, &mut original, &AtomicBool::new(false), |_| Ok(false)).unwrap(),
            WorkerSourceStep::Pending
        );
        assert!(matches!(
            source.fill(&mut logical, &mut original, &AtomicBool::new(true), |_| panic!("cancelled")),
            Err(WorkerSourceError::Cancelled)
        ));
        assert_eq!(logical, [0xa5; 8]);
        assert_eq!(original, [0x5a; 4]);
        drop(source.into_snapshot());
    }
}
