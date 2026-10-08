//! Bounded seek-based preview extraction for classic TIFF camera files.
//!
//! Reads IFD tables, JPEG marker headers and display JPEG ranges, never sensor sample payloads. Unsupported layouts return
//! `None` so the caller can use the existing container-specific extractor. TIFF 6.0 tag and
//! offset semantics; no vendor libraries or algorithms are used.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use lightcraft_tiff::ByteOrder;

const MAX_READ: u64 = 48 * 1024 * 1024;
const MAX_JPEG: u64 = 32 * 1024 * 1024;
const MAX_IFDS: usize = 64;
const MAX_ENTRIES: usize = 8192;
const MAX_RANGES: usize = 512;
const MAX_READ_REQUESTS: usize = 4096;

#[derive(Clone, Copy, PartialEq)]
enum IfdKind {
    File,
    Maker,
    MakerPreview,
}

struct Reader<'a, R> {
    stream: &'a mut R,
    length: u64,
    remaining: u64,
    requests: usize,
    order: ByteOrder,
}

impl<R: Read + Seek> Reader<'_, R> {
    fn at(&mut self, offset: u64, count: u64) -> Option<Vec<u8>> {
        if self.requests == 0 || count > self.remaining || offset.checked_add(count)? > self.length {
            return None;
        }
        self.requests -= 1;
        self.remaining -= count;
        let count = usize::try_from(count).ok()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(count).ok()?;
        bytes.resize(count, 0);
        self.stream.seek(SeekFrom::Start(offset)).ok()?;
        self.stream.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    fn integers(&mut self, entry: &[u8]) -> Option<Vec<u32>> {
        let kind = self.order.read_u16(entry, 2)?;
        let count = self.order.read_u32(entry, 4)?;
        if count == 0 || count > 128 {
            return None;
        }
        let width = match kind {
            3 => 2,
            4 | 13 => 4,
            _ => return None,
        };
        let size = u64::from(count).checked_mul(width)?;
        let bytes = if size <= 4 {
            entry.get(8..8usize.checked_add(usize::try_from(size).ok()?)?)?.to_vec()
        } else {
            self.at(u64::from(self.order.read_u32(entry, 8)?), size)?
        };
        let mut values = Vec::with_capacity(usize::try_from(count).ok()?);
        for i in 0..count {
            let offset = usize::try_from(u64::from(i).checked_mul(width)?).ok()?;
            values.push(if width == 2 { u32::from(self.order.read_u16(&bytes, offset)?) } else { self.order.read_u32(&bytes, offset)? });
        }
        Some(values)
    }

    fn dct(&mut self, offset: u64, count: u64) -> Option<bool> {
        let end = offset.checked_add(count)?;
        if end > self.length {
            return None;
        }
        if count < 4 || self.at(offset, 2)? != [0xff, 0xd8] {
            return Some(false);
        }
        let mut at = offset.checked_add(2)?;
        for _ in 0..256 {
            if at.checked_add(4)? > end {
                return Some(false);
            }
            if at.checked_sub(offset)? > 64 * 1024 {
                return None;
            }
            let marker = self.at(at, 4)?;
            if marker.first() != Some(&0xff) {
                return Some(false);
            }
            match marker.get(1)? {
                0xff => at = at.checked_add(1)?,
                0xc0..=0xc2 => return Some(true),
                0xc3 | 0xc5..=0xc7 | 0xcb | 0xcd..=0xcf | 0xda | 0xd9 => return Some(false),
                _ => {
                    let length = u64::from(u16::from_be_bytes([*marker.get(2)?, *marker.get(3)?]));
                    if length < 2 {
                        return None;
                    }
                    at = at.checked_add(2)?.checked_add(length)?;
                }
            }
        }
        None
    }
}

/// The largest supported JPEG and the raw's EXIF orientation. The stream position changes.
/// Classic TIFF, ordinary/Sony maker-note IFDs, and standalone Undefined JPEG values. BigTIFF
/// and other manufacturer-specific maker-note layouts return `None` for the existing bytes
/// extractor. Reads are capped at 48 MiB / 4096 requests, with 64 IFDs, 8192 entries per IFD,
/// 512 candidate ranges and any one display JPEG at 32 MiB. Ambiguous equal-size
/// candidates also fall back so this optimization does not change the original tie selection.
pub fn embedded_preview_reader<R: Read + Seek>(stream: &mut R) -> Option<(Vec<u8>, u16)> {
    let length = stream.seek(SeekFrom::End(0)).ok()?;
    let mut header = [0u8; 8];
    stream.seek(SeekFrom::Start(0)).ok()?;
    stream.read_exact(&mut header).ok()?;
    let order = match header.get(..2)? {
        b"II" => ByteOrder::Little,
        b"MM" => ByteOrder::Big,
        _ => return None,
    };
    if order.read_u16(&header, 2)? != 42 {
        return None;
    }
    let mut reader = Reader { stream, length, remaining: MAX_READ - 8, requests: MAX_READ_REQUESTS - 1, order };
    let mut pending = vec![(u64::from(order.read_u32(&header, 4)?), IfdKind::File)];
    let mut seen = HashSet::new();
    let mut ranges = Vec::new();
    let mut orientation = 1;
    while let Some((offset, kind)) = pending.pop() {
        if offset == 0 {
            continue;
        }
        if !seen.insert(offset) || seen.len() > MAX_IFDS {
            return None;
        }
        let count = usize::from(order.read_u16(&reader.at(offset, 2)?, 0)?);
        if count > MAX_ENTRIES {
            return None;
        }
        let table = reader.at(offset.checked_add(2)?, u64::try_from(count.checked_mul(12)?.checked_add(4)?).ok()?)?;
        let (mut compression, mut jpeg_offset, mut jpeg_length, mut strip_offset, mut strip_length) = (None, None, None, None, None);
        for i in 0..count {
            let start = i.checked_mul(12)?;
            let entry = table.get(start..start.checked_add(12)?)?;
            let tag = order.read_u16(entry, 0)?;
            let entry_kind = order.read_u16(entry, 2)?;
            let entry_count = order.read_u32(entry, 4)?;
            if entry_kind == 7 && entry_count > 1024 {
                let at = u64::from(order.read_u32(entry, 8)?);
                let length = u64::from(entry_count);
                if at.checked_add(length)? > reader.length {
                    return None;
                }
                if reader.at(at, 2)?.starts_with(&[0xff, 0xd8]) {
                    ranges.push((at, length));
                    if ranges.len() > MAX_RANGES {
                        return None;
                    }
                }
            }
            if tag == 37500 && kind == IfdKind::File {
                if entry_kind != 7 || entry_count < 2 {
                    return None;
                }
                let at = u64::from(order.read_u32(entry, 8)?);
                let length = u64::from(entry_count);
                if at.checked_add(length)? > reader.length {
                    return None;
                }
                let prefix = reader.at(at, length.min(16))?;
                let maker = if prefix.starts_with(b"SONY") || prefix.starts_with(b"VHAB     \0") {
                    at.checked_add(12)?
                } else if usize::from(order.read_u16(&prefix, 0)?) <= MAX_ENTRIES {
                    at
                } else {
                    // Nikon/Fuji/Olympus/Panasonic/Pentax headers have other offset bases.
                    return None;
                };
                pending.push((maker, IfdKind::Maker));
                if pending.len() > MAX_IFDS * 2 {
                    return None;
                }
                continue;
            }
            let child = kind == IfdKind::File && matches!(tag, 330 | 34665 | 34853 | 40965);
            let maker_preview = kind == IfdKind::Maker && tag == 0x0011;
            if !matches!(tag, 259 | 273 | 274 | 279 | 324 | 325 | 513 | 514) && !child && !maker_preview {
                continue;
            }
            if matches!(tag, 273 | 279 | 324 | 325) && entry_count != 1 {
                // Sensor strip arrays cannot describe one standalone JPEG; never read them.
                continue;
            }
            let values = reader.integers(entry)?;
            if child || maker_preview {
                let kind = if maker_preview { IfdKind::MakerPreview } else { IfdKind::File };
                pending.extend(values.into_iter().map(|offset| (u64::from(offset), kind)));
                if pending.len() > MAX_IFDS * 2 {
                    return None;
                }
                continue;
            }
            if values.len() != 1 {
                // Multiple raw strips are irrelevant to a standalone JPEG candidate.
                if matches!(tag, 273 | 279 | 324 | 325) {
                    continue;
                }
                return None;
            }
            let value = *values.first()?;
            match tag {
                259 => compression = Some(value),
                273 | 324 => strip_offset = Some(value),
                274 if seen.len() == 1 && (1..=8).contains(&value) => orientation = u16::try_from(value).ok()?,
                279 | 325 => strip_length = Some(value),
                513 => jpeg_offset = Some(value),
                514 => jpeg_length = Some(value),
                _ => {}
            }
        }
        if let (Some(offset), Some(count)) = (jpeg_offset, jpeg_length) {
            ranges.push((u64::from(offset), u64::from(count)));
        }
        if matches!(compression, Some(6 | 7 | 34892))
            && let (Some(offset), Some(count)) = (strip_offset, strip_length)
        {
            ranges.push((u64::from(offset), u64::from(count)));
        }
        if ranges.len() > MAX_RANGES {
            return None;
        }
        if kind == IfdKind::File {
            pending.push((u64::from(order.read_u32(&table, count.checked_mul(12)?)?), IfdKind::File));
        }
    }
    // Match the existing extractor's largest-byte-range choice without reading smaller JPEGs.
    ranges.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranges.dedup();
    for (index, &(offset, count)) in ranges.iter().enumerate() {
        // Inspect only JPEG markers before deciding to read a range. A lossless sensor JPEG
        // can be larger than the display JPEG; do not read its compressed sample payload.
        if !reader.dct(offset, count)? {
            continue;
        }
        if count > MAX_JPEG {
            return None;
        }
        if ranges.get(index + 1).is_some_and(|next| next.1 == count) {
            return None;
        }
        let jpeg = reader.at(offset, count)?;
        if super::preview::is_dct_jpeg(&jpeg) {
            return Some((super::preview::trim_eoi(&jpeg).to_vec(), orientation));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Result};

    struct Counted {
        cursor: Cursor<Vec<u8>>,
        read: usize,
        requests: usize,
    }
    impl Read for Counted {
        fn read(&mut self, bytes: &mut [u8]) -> Result<usize> {
            let n = self.cursor.read(bytes)?;
            self.read += n;
            self.requests += 1;
            Ok(n)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, position: SeekFrom) -> Result<u64> {
            self.cursor.seek(position)
        }
    }

    fn fixture(order: ByteOrder, offset: u32) -> Counted {
        let jpeg = [0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xff, 0xd9];
        let u16_bytes = |n: u16| match order {
            ByteOrder::Little => n.to_le_bytes(),
            ByteOrder::Big => n.to_be_bytes(),
        };
        let u32_bytes = |n: u32| match order {
            ByteOrder::Little => n.to_le_bytes(),
            ByteOrder::Big => n.to_be_bytes(),
        };
        let mut bytes = vec![0; offset as usize + jpeg.len() + 8 * 1024 * 1024];
        bytes[..2].copy_from_slice(if order == ByteOrder::Little { b"II" } else { b"MM" });
        bytes[2..4].copy_from_slice(&u16_bytes(42));
        bytes[4..8].copy_from_slice(&u32_bytes(8));
        bytes[8..10].copy_from_slice(&u16_bytes(3));
        for (i, (tag, kind, value)) in [(274, 3, 6), (513, 4, offset), (514, 4, jpeg.len() as u32)].into_iter().enumerate() {
            let start = 10 + i * 12;
            bytes[start..start + 2].copy_from_slice(&u16_bytes(tag));
            bytes[start + 2..start + 4].copy_from_slice(&u16_bytes(kind));
            bytes[start + 4..start + 8].copy_from_slice(&u32_bytes(1));
            if kind == 3 {
                bytes[start + 8..start + 10].copy_from_slice(&u16_bytes(value as u16));
            } else {
                bytes[start + 8..start + 12].copy_from_slice(&u32_bytes(value));
            }
        }
        bytes[offset as usize..offset as usize + jpeg.len()].copy_from_slice(&jpeg);
        Counted { cursor: Cursor::new(bytes), read: 0, requests: 0 }
    }

    #[test]
    fn reads_only_tables_and_preview_in_both_byte_orders() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut reader = fixture(order, 4096);
            let (jpeg, orientation) = embedded_preview_reader(&mut reader).unwrap();
            assert_eq!(orientation, 6);
            assert!(jpeg.starts_with(&[0xff, 0xd8]));
            assert!(reader.read < 128, "sensor payload must never be read");
        }
    }

    #[test]
    fn cycles_offsets_counts_and_truncation_refuse_without_payload_reads() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut()[46..50].copy_from_slice(&8u32.to_le_bytes());
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.read < 128);
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut()[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert_eq!(reader.read, 10);
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut()[30..34].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.read < 128);
        for length in 0..50 {
            let bytes = fixture(ByteOrder::Little, 4096).cursor.into_inner();
            assert!(embedded_preview_reader(&mut Cursor::new(bytes[..length].to_vec())).is_none());
        }
    }

    fn entry(bytes: &mut [u8], at: usize, tag: u16, kind: u16, count: u32, value: u32) {
        bytes[at..at + 2].copy_from_slice(&tag.to_le_bytes());
        bytes[at + 2..at + 4].copy_from_slice(&kind.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
    }

    fn append_entry(reader: &mut Counted, tag: u16, kind: u16, count: u32, value: u32) {
        let bytes = reader.cursor.get_mut();
        let old = u16::from_le_bytes([bytes[8], bytes[9]]);
        entry(bytes, 10 + usize::from(old) * 12, tag, kind, count, value);
        bytes[8..10].copy_from_slice(&(old + 1).to_le_bytes());
    }

    #[test]
    fn irrelevant_large_strip_arrays_never_get_read() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        append_entry(&mut reader, 273, 4, u32::MAX, u32::MAX);
        append_entry(&mut reader, 279, 4, u32::MAX, u32::MAX);
        assert!(embedded_preview_reader(&mut reader).is_some());
        assert!(reader.read < 128);
    }

    #[test]
    fn a_larger_lossless_sensor_jpeg_only_reads_its_header() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut().resize(48 * 1024 * 1024 + 256, 0);
        reader.cursor.get_mut()[256..262].copy_from_slice(&[0xff, 0xd8, 0xff, 0xc3, 0, 11]);
        append_entry(&mut reader, 259, 3, 1, 7);
        append_entry(&mut reader, 273, 4, 1, 256);
        append_entry(&mut reader, 279, 4, 1, 48 * 1024 * 1024);
        assert_eq!(embedded_preview_reader(&mut reader).unwrap().0.len(), 17);
        assert!(reader.read < 128, "the lossless sensor JPEG's sample bytes are never read");
    }

    #[test]
    fn undefined_and_plain_maker_preview_candidates_keep_largest_selection() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        let mut large = reader.cursor.get_ref()[4096..4111].to_vec();
        large.extend(std::iter::repeat_n(0x55, 1200));
        large.extend_from_slice(&[0xff, 0xd9]);
        reader.cursor.get_mut()[2048..2048 + large.len()].copy_from_slice(&large);
        append_entry(&mut reader, 0x002e, 7, large.len() as u32, 2048);
        assert_eq!(embedded_preview_reader(&mut reader).unwrap().0, large);

        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut()[2048..2048 + large.len()].copy_from_slice(&large);
        append_entry(&mut reader, 37500, 7, 64, 256);
        reader.cursor.get_mut()[256..258].copy_from_slice(&1u16.to_le_bytes());
        entry(reader.cursor.get_mut(), 258, 0x0011, 4, 1, 512);
        // Maker-note next pointers are not part of the existing preview traversal.
        reader.cursor.get_mut()[270..274].copy_from_slice(&u32::MAX.to_le_bytes());
        reader.cursor.get_mut()[512..514].copy_from_slice(&2u16.to_le_bytes());
        entry(reader.cursor.get_mut(), 514, 513, 4, 1, 2048);
        entry(reader.cursor.get_mut(), 526, 514, 4, 1, large.len() as u32);
        assert_eq!(embedded_preview_reader(&mut reader).unwrap().0, large);
    }

    #[test]
    fn unsupported_maker_bigtiff_and_equal_size_choices_use_existing_extractor() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        append_entry(&mut reader, 37500, 7, 32, 256);
        reader.cursor.get_mut()[256..262].copy_from_slice(b"Nikon\0");
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.read < 128);
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut()[2..4].copy_from_slice(&43u16.to_le_bytes());
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert_eq!(reader.read, 8);
        let mut reader = fixture(ByteOrder::Little, 4096);
        let jpeg = reader.cursor.get_ref()[4096..4113].to_vec();
        reader.cursor.get_mut()[2048..2065].copy_from_slice(&jpeg);
        append_entry(&mut reader, 259, 3, 1, 6);
        append_entry(&mut reader, 273, 4, 1, 2048);
        append_entry(&mut reader, 279, 4, 1, jpeg.len() as u32);
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.read < 128, "ambiguous equal-size JPEGs are not read");
    }

    #[test]
    fn aggregate_read_bytes_and_request_budgets_are_enforced() {
        let mut cursor = Cursor::new(vec![0; MAX_READ as usize]);
        let mut reader = Reader { stream: &mut cursor, length: MAX_READ, remaining: MAX_READ, requests: MAX_READ_REQUESTS, order: ByteOrder::Little };
        assert!(reader.at(u64::MAX, 1).is_none());
        assert!(reader.at(0, MAX_READ + 1).is_none());
        assert_eq!(reader.remaining, MAX_READ);
        assert!(reader.at(0, MAX_READ).is_some());
        assert!(reader.at(0, 1).is_none());

        let mut reader = fixture(ByteOrder::Little, 1_000_000);
        reader.cursor.get_mut()[8..10].copy_from_slice(&(MAX_ENTRIES as u16).to_le_bytes());
        for i in 0..MAX_ENTRIES {
            // In-bounds non-JPEG Undefined values should hit the request cap, not trigger
            // unbounded tiny filesystem reads or range allocations.
            entry(reader.cursor.get_mut(), 10 + i * 12, 0x9999, 7, 2048, 1_100_000);
        }
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.requests <= MAX_READ_REQUESTS);
        assert!(reader.read as u64 <= MAX_READ);
    }

    #[test]
    fn ifd_and_jpeg_allocation_limits_refuse_without_sensor_reads() {
        let mut reader = fixture(ByteOrder::Little, 4096);
        for i in 0..=MAX_IFDS {
            let at = 8 + i * 6;
            reader.cursor.get_mut()[at..at + 2].copy_from_slice(&0u16.to_le_bytes());
            reader.cursor.get_mut()[at + 2..at + 6].copy_from_slice(&((at + 6) as u32).to_le_bytes());
        }
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert_eq!(reader.read, 8 + MAX_IFDS * 6);
        let mut reader = fixture(ByteOrder::Little, 4096);
        reader.cursor.get_mut().resize(MAX_JPEG as usize + 4097, 0);
        reader.cursor.get_mut()[42..46].copy_from_slice(&((MAX_JPEG + 1) as u32).to_le_bytes());
        assert!(embedded_preview_reader(&mut reader).is_none());
        assert!(reader.read < 128);
    }
}
