use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

pub type LaneId = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFile {
    base: Vec<u8>,
    blobs: Vec<Vec<u8>>,
    lanes: BTreeMap<LaneId, LaneView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneView {
    extents: Vec<Extent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Extent {
    source: Source,
    start: u64,
    len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Base,
    Blob(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneError {
    RangeOutOfBounds { start: u64, end: u64, len: u64 },
    BlobMissing(u64),
    LaneMissing(LaneId),
}

impl LaneFile {
    pub fn new(base: impl Into<Vec<u8>>) -> Self {
        Self {
            base: base.into(),
            blobs: Vec::new(),
            lanes: BTreeMap::new(),
        }
    }

    pub fn base(&self) -> &[u8] {
        &self.base
    }

    pub fn lane_ids(&self) -> impl Iterator<Item = &str> {
        self.lanes.keys().map(String::as_str)
    }

    pub fn read_base(&self) -> Vec<u8> {
        self.base.clone()
    }

    pub fn read(&self, lane: &str) -> Result<Vec<u8>, LaneError> {
        self.render(&self.view_for(lane))
    }

    pub fn write(
        &mut self,
        lane: impl Into<LaneId>,
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        let lane = lane.into();
        let replacement = replacement.into();
        let view = self.view_for(&lane);
        let current_len = extents_len(&view.extents);
        ensure_valid_range(range.clone(), current_len)?;

        let mut next = slice_extents(&view.extents, 0..range.start);

        if !replacement.is_empty() {
            let blob_id = self.push_blob(replacement);
            let blob_len = self.blobs[blob_id as usize].len() as u64;
            next.push(Extent {
                source: Source::Blob(blob_id),
                start: 0,
                len: blob_len,
            });
        }

        next.extend(slice_extents(&view.extents, range.end..current_len));
        self.lanes.insert(
            lane,
            LaneView {
                extents: normalize_extents(next),
            },
        );
        Ok(())
    }

    pub fn delete(&mut self, lane: impl Into<LaneId>, range: Range<u64>) -> Result<(), LaneError> {
        self.write(lane, range, Vec::new())
    }

    pub fn discard(&mut self, lane: &str) -> bool {
        self.lanes.remove(lane).is_some()
    }

    pub fn promote(&mut self, lane: &str) -> Result<(), LaneError> {
        if !self.lanes.contains_key(lane) {
            return Err(LaneError::LaneMissing(lane.to_owned()));
        }

        let promoted = self.read(lane)?;

        let lane_ids: Vec<_> = self
            .lanes
            .keys()
            .filter(|lane_id| lane_id.as_str() != lane)
            .cloned()
            .collect();

        let mut preserved = BTreeMap::new();
        for lane_id in lane_ids {
            let bytes = self.read(&lane_id)?;
            let blob_id = self.push_blob(bytes);
            let len = self.blobs[blob_id as usize].len() as u64;
            preserved.insert(
                lane_id,
                LaneView {
                    extents: vec![Extent {
                        source: Source::Blob(blob_id),
                        start: 0,
                        len,
                    }],
                },
            );
        }

        self.base = promoted;
        self.lanes = preserved;
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LANE\0\0\0\x01");
        write_bytes(&mut bytes, &self.base);
        write_u64(&mut bytes, self.blobs.len() as u64);
        for blob in &self.blobs {
            write_bytes(&mut bytes, blob);
        }
        write_u64(&mut bytes, self.lanes.len() as u64);
        for (lane_id, view) in &self.lanes {
            write_bytes(&mut bytes, lane_id.as_bytes());
            write_u64(&mut bytes, view.extents.len() as u64);
            for extent in &view.extents {
                match extent.source {
                    Source::Base => {
                        bytes.push(0);
                        write_u64(&mut bytes, extent.start);
                        write_u64(&mut bytes, extent.len);
                    }
                    Source::Blob(blob_id) => {
                        bytes.push(1);
                        write_u64(&mut bytes, blob_id);
                        write_u64(&mut bytes, extent.start);
                        write_u64(&mut bytes, extent.len);
                    }
                }
            }
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        cursor.expect(b"LANE\0\0\0\x01")?;

        let base = cursor.read_bytes()?.to_vec();
        let mut blobs = Vec::new();
        for _ in 0..cursor.read_u64()? {
            blobs.push(cursor.read_bytes()?.to_vec());
        }

        let mut lanes = BTreeMap::new();
        for _ in 0..cursor.read_u64()? {
            let lane_id = String::from_utf8(cursor.read_bytes()?.to_vec())
                .map_err(|_| DecodeError::InvalidUtf8)?;
            let mut extents = Vec::new();
            for _ in 0..cursor.read_u64()? {
                let tag = cursor.read_byte()?;
                let source = match tag {
                    0 => Source::Base,
                    1 => Source::Blob(cursor.read_u64()?),
                    _ => return Err(DecodeError::InvalidSource(tag)),
                };
                let start = cursor.read_u64()?;
                let len = cursor.read_u64()?;
                extents.push(Extent { source, start, len });
            }
            lanes.insert(
                lane_id,
                LaneView {
                    extents: normalize_extents_checked(extents)?,
                },
            );
        }

        let file = Self { base, blobs, lanes };
        file.validate()?;
        if !cursor.is_finished() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(file)
    }

    fn view_for(&self, lane: &str) -> LaneView {
        self.lanes.get(lane).cloned().unwrap_or_else(|| LaneView {
            extents: vec![Extent {
                source: Source::Base,
                start: 0,
                len: self.base.len() as u64,
            }],
        })
    }

    fn render(&self, view: &LaneView) -> Result<Vec<u8>, LaneError> {
        let mut bytes = Vec::with_capacity(extents_len(&view.extents) as usize);
        for extent in &view.extents {
            let source = match extent.source {
                Source::Base => &self.base,
                Source::Blob(blob_id) => self
                    .blobs
                    .get(blob_id as usize)
                    .ok_or(LaneError::BlobMissing(blob_id))?,
            };
            let start = extent.start as usize;
            let end = start + extent.len as usize;
            bytes.extend_from_slice(&source[start..end]);
        }
        Ok(bytes)
    }

    fn push_blob(&mut self, bytes: Vec<u8>) -> u64 {
        let blob_id = self.blobs.len() as u64;
        self.blobs.push(bytes);
        blob_id
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for view in self.lanes.values() {
            for extent in &view.extents {
                let source_len = match extent.source {
                    Source::Base => self.base.len() as u64,
                    Source::Blob(blob_id) => self
                        .blobs
                        .get(blob_id as usize)
                        .ok_or(DecodeError::BlobMissing(blob_id))?
                        .len() as u64,
                };
                if extent.start > source_len || extent.len > source_len - extent.start {
                    return Err(DecodeError::ExtentOutOfBounds);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    BadMagic,
    UnexpectedEof,
    InvalidUtf8,
    InvalidSource(u8),
    BlobMissing(u64),
    ExtentOutOfBounds,
    TrailingBytes,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DecodeError {}

fn ensure_valid_range(range: Range<u64>, len: u64) -> Result<(), LaneError> {
    if range.start > range.end || range.end > len {
        Err(LaneError::RangeOutOfBounds {
            start: range.start,
            end: range.end,
            len,
        })
    } else {
        Ok(())
    }
}

fn extents_len(extents: &[Extent]) -> u64 {
    extents.iter().map(|extent| extent.len).sum()
}

fn slice_extents(extents: &[Extent], range: Range<u64>) -> Vec<Extent> {
    let mut sliced = Vec::new();
    let mut cursor = 0;

    for extent in extents {
        let extent_start = cursor;
        let extent_end = cursor + extent.len;
        cursor = extent_end;

        let start = range.start.max(extent_start);
        let end = range.end.min(extent_end);
        if start >= end {
            continue;
        }

        sliced.push(Extent {
            source: extent.source.clone(),
            start: extent.start + (start - extent_start),
            len: end - start,
        });
    }

    normalize_extents(sliced)
}

fn normalize_extents(extents: Vec<Extent>) -> Vec<Extent> {
    normalize_extents_checked(extents).expect("extent arithmetic overflow")
}

fn normalize_extents_checked(extents: Vec<Extent>) -> Result<Vec<Extent>, DecodeError> {
    let mut normalized: Vec<Extent> = Vec::new();
    for extent in extents.into_iter().filter(|extent| extent.len > 0) {
        if let Some(previous) = normalized.last_mut()
            && previous.source == extent.source
            && previous
                .start
                .checked_add(previous.len)
                .is_some_and(|end| end == extent.start)
        {
            previous.len = previous
                .len
                .checked_add(extent.len)
                .ok_or(DecodeError::ExtentOutOfBounds)?;
            continue;
        }
        normalized.push(extent);
    }
    Ok(normalized)
}

fn write_bytes(target: &mut Vec<u8>, bytes: &[u8]) {
    write_u64(target, bytes.len() as u64);
    target.extend_from_slice(bytes);
}

fn write_u64(target: &mut Vec<u8>, value: u64) {
    target.extend_from_slice(&value.to_le_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn expect(&mut self, expected: &[u8]) -> Result<(), DecodeError> {
        let actual = self.take(expected.len())?;
        if actual == expected {
            Ok(())
        } else {
            Err(DecodeError::BadMagic)
        }
    }

    fn read_byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.read_u64()? as usize;
        self.take(len)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(DecodeError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_writes_do_not_change_base_or_other_lanes() {
        let mut file = LaneFile::new(b"abcdef".to_vec());

        file.write("agent-a", 2..4, b"XX".to_vec()).unwrap();
        file.write("agent-b", 1..5, b"Y".to_vec()).unwrap();

        assert_eq!(file.read_base(), b"abcdef");
        assert_eq!(file.read("agent-a").unwrap(), b"abXXef");
        assert_eq!(file.read("agent-b").unwrap(), b"aYf");
        assert_eq!(file.read("unknown").unwrap(), b"abcdef");
    }

    #[test]
    fn writes_are_addressed_against_the_lane_view() {
        let mut file = LaneFile::new(b"abcdef".to_vec());

        file.write("agent-a", 2..4, b"XX".to_vec()).unwrap();
        file.write("agent-a", 4..4, b"YY".to_vec()).unwrap();

        assert_eq!(file.read("agent-a").unwrap(), b"abXXYYef");
    }

    #[test]
    fn delete_removes_bytes_from_one_lane() {
        let mut file = LaneFile::new(b"abcdef".to_vec());

        file.delete("agent-a", 1..5).unwrap();

        assert_eq!(file.read_base(), b"abcdef");
        assert_eq!(file.read("agent-a").unwrap(), b"af");
    }

    #[test]
    fn promote_preserves_non_promoted_lane_renderings() {
        let mut file = LaneFile::new(b"abcdef".to_vec());
        file.write("agent-a", 2..4, b"XX".to_vec()).unwrap();
        file.write("agent-b", 1..5, b"Y".to_vec()).unwrap();

        file.promote("agent-a").unwrap();

        assert_eq!(file.read_base(), b"abXXef");
        assert_eq!(file.read("agent-b").unwrap(), b"aYf");
        assert_eq!(file.read("agent-a").unwrap(), b"abXXef");
    }

    #[test]
    fn promote_rejects_missing_lanes() {
        let mut file = LaneFile::new(b"abcdef".to_vec());
        file.write("agent-a", 2..4, b"XX".to_vec()).unwrap();

        assert_eq!(
            file.promote("missing"),
            Err(LaneError::LaneMissing("missing".to_owned()))
        );
        assert_eq!(file.read_base(), b"abcdef");
        assert_eq!(file.read("agent-a").unwrap(), b"abXXef");
    }

    #[test]
    fn serialized_lane_file_round_trips() {
        let mut file = LaneFile::new(b"abcdef".to_vec());
        file.write("agent-a", 2..4, b"XX".to_vec()).unwrap();
        file.delete("agent-b", 1..5).unwrap();

        let decoded = LaneFile::from_bytes(&file.to_bytes()).unwrap();

        assert_eq!(decoded.read_base(), b"abcdef");
        assert_eq!(decoded.read("agent-a").unwrap(), b"abXXef");
        assert_eq!(decoded.read("agent-b").unwrap(), b"af");
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = LaneFile::new(b"abcdef".to_vec()).to_bytes();
        bytes.push(0);

        assert_eq!(
            LaneFile::from_bytes(&bytes),
            Err(DecodeError::TrailingBytes)
        );
    }

    #[test]
    fn decode_rejects_extent_overflow_without_panicking() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LANE\0\0\0\x01");
        write_bytes(&mut bytes, b"base");
        write_u64(&mut bytes, 0);
        write_u64(&mut bytes, 1);
        write_bytes(&mut bytes, b"agent-a");
        write_u64(&mut bytes, 2);

        bytes.push(0);
        write_u64(&mut bytes, u64::MAX);
        write_u64(&mut bytes, 1);

        bytes.push(0);
        write_u64(&mut bytes, 0);
        write_u64(&mut bytes, 1);

        assert_eq!(
            LaneFile::from_bytes(&bytes),
            Err(DecodeError::ExtentOutOfBounds)
        );
    }
}
