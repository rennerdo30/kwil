use super::*;
use iso2022::State;

impl EncodedFile {
    pub(super) fn stream_iso_save(
        &self,
        snapshot: &Snapshot,
        plan: SavePlan,
        writer: &mut impl Write,
    ) -> Result<(), EncodingError> {
        if plan.bom && plan.encoding.bom_bytes().is_empty() {
            return Err(EncodingError::UnsupportedEncoding(format!("BOM for {}", plan.encoding.name())));
        }
        self.file.validate_source()?;
        if plan.bom { writer.write_all(plan.encoding.bom_bytes())?; }
        let mut output = TextOutput::new(plan, writer);
        let mut added = Utf8Input::default();
        let mut decoder = None;
        let preserve = plan.encoding == self.detection.encoding && plan.line_ending.is_none();
        for chunk in snapshot.source_chunks(0..snapshot.len())? {
            match chunk? {
                SourceChunk::Original { file, range, bytes, piece_range } => {
                    if !file.same_source(&self.file) { return Err(EncodingError::WrongSource); }
                    added.finish()?;
                    if range.start == piece_range.start {
                        let (at, state) = self.iso_position(range.start)?;
                        let (end, _) = self.iso_position(piece_range.end)?;
                        if at != range.start { return Err(EncodingError::SplitCharacter(range.start)); }
                        if end != piece_range.end { return Err(EncodingError::SplitCharacter(piece_range.end)); }
                        if preserve { output.iso_begin(state)?; }
                        else {
                            if state == State::Raw { return Err(EncodingError::MalformedSource); }
                            decoder = Some(state.decoder());
                        }
                    }
                    if preserve { output.writer.write_all(bytes)?; }
                    else { transcode(decoder.as_mut().unwrap(), bytes, false, &mut output)?; }
                    if range.end == piece_range.end {
                        if preserve { output.iso_raw_state = Some(self.iso_position(range.end)?.1); }
                        else { finish_source(&mut decoder, &mut output)?; }
                    }
                }
                SourceChunk::Added(bytes) => { added.feed(bytes, |text| output.feed(text))?; }
                _ => return Err(EncodingError::WrongSource),
            }
        }
        added.finish()?;
        output.finish()
    }
    pub(super) fn build_iso_index(&self, limit: usize) -> Result<DecodeIndex, EncodingError> {
        let mut checkpoints = IndexVec::new(&IndexMemory::new(limit));
        let (mut offset, mut line, mut line_start) = (0, 0, 0);
        let mut state = State::Ascii;
        let mut raw_mode = false;
        checkpoints.push(Checkpoint { offset, line, line_start, iso: state })?;
        while offset < self.file.len() {
            let bytes = self.file.read_range(offset..(offset + STRIDE as u64 + 8).min(self.file.len()))?;
            let eof = offset + bytes.len() as u64 == self.file.len();
            let mut at = 0;
            while at < bytes.len() && at < STRIDE - 8 {
                match iso2022::atom(&bytes[at..], state, eof) {
                    Ok(atom) => {
                        state = atom.state;
                        at += atom.raw;
                    }
                    Err(_) => {
                        raw_mode = true;
                        state = State::Raw;
                        at += 1;
                    }
                }
            }
            if at > 0 && bytes[at - 1] == b'\r' && bytes.get(at) == Some(&b'\n') {
                at += 1;
            }
            for_breaks(&bytes, 0, at, self.detection.encoding, |end, _| {
                line += 1;
                line_start = offset + end as u64;
            });
            offset += at as u64;
            if offset < self.file.len() {
                checkpoints.push(Checkpoint { offset, line, line_start, iso: state })?;
            }
            self.index.scanned.store(offset, Ordering::Relaxed);
        }
        self.file.validate_source()?;
        Ok(DecodeIndex { checkpoints: checkpoints.into_vec(), line_count: line + 1, raw_mode })
    }

    pub(super) fn iso_position(&self, position: u64) -> Result<(u64, State), EncodingError> {
        let (mut at, mut state) = if let Some(index) = self.index() {
            if index.raw_mode {
                return Ok((position, State::Raw));
            }
            let i = index.checkpoints.partition_point(|p| p.offset <= position).saturating_sub(1);
            let checkpoint = &index.checkpoints[i];
            (checkpoint.offset, checkpoint.iso)
        } else if position <= STRIDE as u64 {
            (0, State::Ascii)
        } else {
            return Err(EncodingError::IndexNotReady);
        };
        let end = (position + 8).min(self.file.len());
        let bytes = self.file.read_range(at..end)?;
        let base = at;
        while at < position {
            let atom = iso2022::atom(&bytes[(at - base) as usize..], state, end == self.file.len())?;
            if at + atom.raw as u64 > position {
                break;
            }
            at += atom.raw as u64;
            state = atom.state;
        }
        Ok((at, state))
    }

    pub(super) fn decode_iso_range(&self, range: Range<u64>) -> Result<DecodedRange<'_>, EncodingError> {
        let (start, mut state) = match self.iso_position(range.start) {
            Ok(position) => position,
            Err(EncodingError::MalformedSource) => (range.start, State::Raw),
            Err(error) => return Err(error),
        };
        let end = (range.end + 8).min(self.file.len());
        let bytes = self.file.read_range(start..end)?;
        if state == State::Raw {
            let raw = bytes[..(range.end - start) as usize].to_vec();
            return Ok(DecodedRange {
                source: start..range.end,
                segments: vec![DisplaySegment::Invalid(Cow::Owned(raw))],
                had_errors: true,
            });
        }
        let mut at = 0;
        let mut text = String::new();
        while start + (at as u64) < range.end {
            match iso2022::atom(&bytes[at..], state, end == self.file.len()) {
                Ok(atom) => {
                    text.push_str(std::str::from_utf8(&atom.text[..atom.len]).unwrap());
                    at += atom.raw;
                    state = atom.state;
                }
                Err(_) => {
                    return Ok(DecodedRange {
                        source: start..range.end,
                        segments: vec![DisplaySegment::Invalid(Cow::Owned(
                            bytes[..(range.end - start) as usize].to_vec(),
                        ))],
                        had_errors: true,
                    });
                }
            }
        }
        if at > MAX_VIEWPORT_BYTES {
            return Err(EncodingError::ViewportTooLarge);
        }
        Ok(DecodedRange {
            source: start..start + at as u64,
            segments: if text.is_empty() { vec![] } else { vec![DisplaySegment::Text(Cow::Owned(text))] },
            had_errors: false,
        })
    }
}
