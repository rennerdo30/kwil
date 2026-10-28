//! Worker-only PNG/JPEG decoding with explicit source, header, workspace and output limits.
//! WebP is refused until its separate Huffman/workspace guard is qualified.
use image::{ColorType, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};
pub const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RGBA_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 2048;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Unsupported,
    Corrupt,
    Limit,
    Cancelled,
    Allocation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    WebP,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub encoded_bytes: usize,
    pub rgba_bytes: usize,
    pub dimension: u32,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self { encoded_bytes: MAX_ENCODED_BYTES, rgba_bytes: MAX_RGBA_BYTES, dimension: MAX_DIMENSION }
    }
}
impl DecodeLimits {
    fn valid(self) -> bool {
        self.encoded_bytes > 0
            && self.encoded_bytes <= MAX_ENCODED_BYTES
            && self.rgba_bytes > 0
            && self.rgba_bytes <= MAX_RGBA_BYTES
            && self.dimension > 0
            && self.dimension <= MAX_DIMENSION
    }
    fn dimensions(self, width: u32, height: u32) -> Result<usize, DecodeError> {
        if width == 0 || height == 0 || width > self.dimension || height > self.dimension {
            return Err(DecodeError::Limit);
        }
        let bytes =
            (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).ok_or(DecodeError::Limit)?;
        if bytes > self.rgba_bytes {
            return Err(DecodeError::Limit);
        }
        Ok(bytes)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn check(cancelled: &dyn Fn() -> bool) -> Result<(), DecodeError> {
    if cancelled() { Err(DecodeError::Cancelled) } else { Ok(()) }
}
pub fn detect(bytes: &[u8]) -> Result<Format, DecodeError> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(Format::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(Format::Jpeg)
    } else {
        Err(DecodeError::Unsupported)
    }
}
fn be32(bytes: &[u8]) -> Result<u32, DecodeError> {
    Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| DecodeError::Corrupt)?))
}
fn le32(bytes: &[u8]) -> Result<u32, DecodeError> {
    Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| DecodeError::Corrupt)?))
}
fn little24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

// Header inspection takes place BEFORE constructing the library decoder, so a
// declared dimension bomb cannot first allocate a decoder output/frame buffer.
fn header(
    bytes: &[u8],
    format: Format,
    limits: DecodeLimits,
    cancel: &dyn Fn() -> bool,
) -> Result<(u32, u32), DecodeError> {
    match format {
        Format::Png => {
            if bytes.len() < 33 || bytes[8..12] != 13u32.to_be_bytes() || &bytes[12..16] != b"IHDR" {
                return Err(DecodeError::Corrupt);
            }
            Ok((be32(&bytes[16..20])?, be32(&bytes[20..24])?))
        }
        Format::Jpeg => {
            let mut at = 2usize;
            while at < bytes.len() {
                check(cancel)?;
                if bytes[at] != 0xff {
                    return Err(DecodeError::Corrupt);
                }
                while bytes.get(at) == Some(&0xff) {
                    at += 1;
                }
                let marker = *bytes.get(at).ok_or(DecodeError::Corrupt)?;
                at += 1;
                if matches!(marker, 0x00 | 0xd8 | 0xd9 | 0xda) {
                    return Err(DecodeError::Corrupt);
                }
                if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                    continue;
                }
                let part = bytes.get(at..at + 2).ok_or(DecodeError::Corrupt)?;
                let len = u16::from_be_bytes([part[0], part[1]]) as usize;
                if len < 2 {
                    return Err(DecodeError::Corrupt);
                }
                let end = at.checked_add(len).ok_or(DecodeError::Corrupt)?;
                let part = bytes.get(at..end).ok_or(DecodeError::Corrupt)?;
                if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                    if part.len() < 8 {
                        return Err(DecodeError::Corrupt);
                    }
                    if !matches!(marker, 0xc0..=0xc2) || part[2] != 8 || !matches!(part[7], 1 | 3 | 4) {
                        return Err(DecodeError::Unsupported);
                    }
                    if part.len() != 8 + usize::from(part[7]) * 3 {
                        return Err(DecodeError::Corrupt);
                    }
                    let mut blocks = 0usize;
                    for component in 0..usize::from(part[7]) {
                        let sample = part[9 + component * 3];
                        let (horizontal, vertical) = (sample >> 4, sample & 15);
                        if !(1..=4).contains(&horizontal) || !(1..=4).contains(&vertical) {
                            return Err(DecodeError::Corrupt);
                        }
                        blocks += usize::from(horizontal) * usize::from(vertical);
                    }
                    if blocks > 10 {
                        return Err(DecodeError::Unsupported);
                    }
                    return Ok((
                        u32::from(u16::from_be_bytes([part[5], part[6]])),
                        u32::from(u16::from_be_bytes([part[3], part[4]])),
                    ));
                }
                at = end;
            }
            Err(DecodeError::Corrupt)
        }
        Format::WebP => {
            let end = (le32(&bytes[4..8])? as usize).checked_add(8).ok_or(DecodeError::Corrupt)?;
            if end != bytes.len() {
                return Err(DecodeError::Corrupt);
            }
            let mut at = 12usize;
            let mut canvas = None;
            let mut frame = None;
            while at < end {
                check(cancel)?;
                let chunk = bytes.get(at..at + 8).ok_or(DecodeError::Corrupt)?;
                let len = le32(&chunk[4..8])? as usize;
                let begin = at + 8;
                let finish = begin.checked_add(len).ok_or(DecodeError::Corrupt)?;
                let data = bytes.get(begin..finish).ok_or(DecodeError::Corrupt)?;
                let dims = match &chunk[..4] {
                    b"VP8X" => {
                        if data.len() != 10 {
                            return Err(DecodeError::Corrupt);
                        }
                        if data[0] & 2 != 0 {
                            return Err(DecodeError::Unsupported);
                        }
                        if canvas.is_some() {
                            return Err(DecodeError::Corrupt);
                        }
                        let dims = (little24(&data[4..7]) + 1, little24(&data[7..10]) + 1);
                        canvas = Some(dims);
                        Some(dims)
                    }
                    b"ANIM" | b"ANMF" => return Err(DecodeError::Unsupported),
                    b"VP8 " => {
                        if data.len() < 10 || data[3..6] != [0x9d, 0x01, 0x2a] {
                            return Err(DecodeError::Corrupt);
                        }
                        if frame.is_some() {
                            return Err(DecodeError::Corrupt);
                        }
                        let dims = (
                            u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
                            u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
                        );
                        frame = Some(dims);
                        Some(dims)
                    }
                    b"VP8L" => {
                        if data.len() < 5 || data[0] != 0x2f {
                            return Err(DecodeError::Corrupt);
                        }
                        if frame.is_some() {
                            return Err(DecodeError::Corrupt);
                        }
                        let bits = le32(&data[1..5])?;
                        if bits >> 29 != 0 {
                            return Err(DecodeError::Corrupt);
                        }
                        let dims = ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1);
                        frame = Some(dims);
                        Some(dims)
                    }
                    _ => None,
                };
                if let Some((width, height)) = dims {
                    limits.dimensions(width, height)?;
                }
                at = finish.checked_add(len & 1).ok_or(DecodeError::Corrupt)?;
                if at > end {
                    return Err(DecodeError::Corrupt);
                }
            }
            let frame = frame.ok_or(DecodeError::Corrupt)?;
            if canvas.is_some_and(|value| value != frame) {
                return Err(DecodeError::Corrupt);
            }
            Ok(canvas.unwrap_or(frame))
        }
    }
}

struct Cancellable<'a> {
    bytes: Cursor<&'a [u8]>,
    cancel: &'a dyn Fn() -> bool,
}
impl Cancellable<'_> {
    fn io_check(&self) -> io::Result<()> {
        if (self.cancel)() { Err(io::Error::other("preview decode cancelled")) } else { Ok(()) }
    }
}
impl Read for Cancellable<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.io_check()?;
        let count = out.len().min(16 * 1024);
        self.bytes.read(&mut out[..count])
    }
    fn read_to_end(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        self.io_check()?;
        let start = self.bytes.position().min(self.bytes.get_ref().len() as u64) as usize;
        let remaining = self.bytes.get_ref().len() - start;
        if out.len().checked_add(remaining).is_none_or(|size| size > MAX_ENCODED_BYTES) {
            return Err(io::Error::from(io::ErrorKind::OutOfMemory));
        }
        // image's JPEG constructor uses read_to_end into an internal Vec. Avoid
        // geometric EOF probing/growth past the admitted encoded capacity.
        out.try_reserve_exact(remaining).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        if out.capacity() > MAX_ENCODED_BYTES {
            return Err(io::Error::from(io::ErrorKind::OutOfMemory));
        }
        let mut at = start;
        while at < self.bytes.get_ref().len() {
            self.io_check()?;
            let end = (at + 16 * 1024).min(self.bytes.get_ref().len());
            out.extend_from_slice(&self.bytes.get_ref()[at..end]);
            at = end;
            self.bytes.set_position(at as u64);
        }
        Ok(remaining)
    }
}
impl BufRead for Cancellable<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.io_check()?;
        let bytes = self.bytes.fill_buf()?;
        Ok(&bytes[..bytes.len().min(16 * 1024)])
    }
    fn consume(&mut self, count: usize) {
        self.bytes.consume(count);
    }
}
impl Seek for Cancellable<'_> {
    fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
        self.io_check()?;
        self.bytes.seek(at)
    }
}
fn codec_error(error: image::ImageError) -> DecodeError {
    match error {
        image::ImageError::Limits(_) => DecodeError::Limit,
        image::ImageError::Unsupported(_) => DecodeError::Unsupported,
        image::ImageError::IoError(error) if error.kind() == io::ErrorKind::OutOfMemory => DecodeError::Limit,
        _ => DecodeError::Corrupt,
    }
}

fn png_native_bytes(bytes: &[u8], dimensions: (u32, u32), cancel: &dyn Fn() -> bool) -> Result<usize, DecodeError> {
    let (depth, color) = (bytes[24], bytes[25]);
    let mut channels = match color {
        0 => 1usize,
        2 | 3 => 3,
        4 => 2,
        6 => 4,
        _ => return Err(DecodeError::Unsupported),
    };
    let mut at = 33usize;
    while at < bytes.len() {
        check(cancel)?;
        let part = bytes.get(at..at + 8).ok_or(DecodeError::Corrupt)?;
        let count = be32(&part[..4])? as usize;
        let end = at.checked_add(12).and_then(|at| at.checked_add(count)).ok_or(DecodeError::Corrupt)?;
        if end > bytes.len() {
            return Err(DecodeError::Corrupt);
        }
        if &part[4..8] == b"tRNS" {
            channels = match color {
                0 => 2,
                2 | 3 => 4,
                _ => channels,
            };
        }
        if &part[4..8] == b"IDAT" {
            break;
        }
        at = end;
    }
    if !matches!(depth, 1 | 2 | 4 | 8 | 16) {
        return Err(DecodeError::Unsupported);
    }
    let sample = if depth == 16 && color != 3 { 2 } else { 1 };
    (dimensions.0 as usize)
        .checked_mul(dimensions.1 as usize)
        .and_then(|count| count.checked_mul(channels * sample))
        .ok_or(DecodeError::Limit)
}

/// Decodes one static image into a single in-place native/RGBA work buffer.
/// No DynamicImage/to_rgba intermediate or network/path inference. Animation
/// is explicitly unsupported for WebP, rather than admitting hidden frames.
/// Host admission includes encoded input, decoder workspace and output capacity.
pub fn decode(bytes: &[u8], limits: DecodeLimits, cancelled: &dyn Fn() -> bool) -> Result<Decoded, DecodeError> {
    check(cancelled)?;
    if !limits.valid() || bytes.len() > limits.encoded_bytes {
        return Err(DecodeError::Limit);
    }
    let format = detect(bytes)?;
    let dimensions = header(bytes, format, limits, cancelled)?;
    let rgba_bytes = limits.dimensions(dimensions.0, dimensions.1)?;
    if format == Format::Png && png_native_bytes(bytes, dimensions, cancelled)? > limits.rgba_bytes {
        return Err(DecodeError::Limit);
    }
    check(cancelled)?;
    let format_id = match format {
        Format::Png => ImageFormat::Png,
        Format::Jpeg => ImageFormat::Jpeg,
        Format::WebP => ImageFormat::WebP,
    };
    let reader = Cancellable { bytes: Cursor::new(bytes), cancel: cancelled };
    let mut reader = ImageReader::with_format(reader, format_id);
    let mut decoder_limits = Limits::default();
    decoder_limits.max_image_width = Some(limits.dimension);
    decoder_limits.max_image_height = Some(limits.dimension);
    decoder_limits.max_alloc = Some(limits.rgba_bytes as u64);
    reader.limits(decoder_limits);
    let decoder_result = reader.into_decoder();
    check(cancelled)?;
    let decoder = decoder_result.map_err(codec_error)?;
    if decoder.dimensions() != dimensions {
        return Err(DecodeError::Corrupt);
    }
    let color = decoder.color_type();
    let native = usize::try_from(decoder.total_bytes()).map_err(|_| DecodeError::Limit)?;
    let bpp = color.bytes_per_pixel() as usize;
    let pixels = rgba_bytes / 4;
    if native != pixels.checked_mul(bpp).ok_or(DecodeError::Limit)? || native > limits.rgba_bytes {
        return Err(DecodeError::Limit);
    }
    if !matches!(
        color,
        ColorType::L8
            | ColorType::La8
            | ColorType::Rgb8
            | ColorType::Rgba8
            | ColorType::L16
            | ColorType::La16
            | ColorType::Rgb16
            | ColorType::Rgba16
    ) {
        return Err(DecodeError::Unsupported);
    }
    let capacity = native.max(rgba_bytes);
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(capacity).map_err(|_| DecodeError::Allocation)?;
    if buffer.capacity() > limits.rgba_bytes {
        return Err(DecodeError::Limit);
    }
    buffer.resize(capacity, 0);
    check(cancelled)?;
    let result = decoder.read_image(&mut buffer[..native]);
    check(cancelled)?;
    result.map_err(codec_error)?;
    // <=4 bpp expands backwards; >4 bpp compacts forwards. A four-byte local
    // pixel protects overlapping source/destination without a second image.
    if bpp <= 4 {
        for index in (0..pixels).rev() {
            if index % 1024 == 0 {
                check(cancelled)?;
            }
            let value = rgba_pixel(&buffer[index * bpp..][..bpp], color)?;
            buffer[index * 4..index * 4 + 4].copy_from_slice(&value);
        }
    } else {
        for index in 0..pixels {
            if index % 1024 == 0 {
                check(cancelled)?;
            }
            let value = rgba_pixel(&buffer[index * bpp..][..bpp], color)?;
            buffer[index * 4..index * 4 + 4].copy_from_slice(&value);
        }
    }
    buffer.truncate(rgba_bytes);
    check(cancelled)?;
    Ok(Decoded { format, width: dimensions.0, height: dimensions.1, rgba: buffer })
}
fn rgba_pixel(bytes: &[u8], color: ColorType) -> Result<[u8; 4], DecodeError> {
    let channel16 = |at| {
        let value = u16::from_ne_bytes([bytes[at], bytes[at + 1]]);
        ((u32::from(value) * 255 + 32767) / 65535) as u8
    };
    Ok(match color {
        ColorType::L8 => [bytes[0], bytes[0], bytes[0], 255],
        ColorType::La8 => [bytes[0], bytes[0], bytes[0], bytes[1]],
        ColorType::Rgb8 => [bytes[0], bytes[1], bytes[2], 255],
        ColorType::Rgba8 => [bytes[0], bytes[1], bytes[2], bytes[3]],
        ColorType::L16 => {
            let y = channel16(0);
            [y, y, y, 255]
        }
        ColorType::La16 => {
            let y = channel16(0);
            [y, y, y, channel16(2)]
        }
        ColorType::Rgb16 => [channel16(0), channel16(2), channel16(4), 255],
        ColorType::Rgba16 => [channel16(0), channel16(2), channel16(4), channel16(6)],
        _ => return Err(DecodeError::Unsupported),
    })
}
