use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

pub mod demo;
pub mod projection;

pub type FilePath = String;
pub type LaneId = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRepo {
    lanes: BTreeSet<LaneId>,
    files: BTreeMap<FilePath, LaneFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotedFile {
    pub path: FilePath,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneFile {
    base_hash: u64,
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
    ReservedLane(LaneId),
    LaneMissing(LaneId),
    BaseMissing { path: FilePath },
    BaseChanged { path: FilePath },
    RangeOutOfBounds { start: u64, end: u64, len: u64 },
    BlobMissing(u64),
    ExtentOutOfBounds,
}

impl LaneRepo {
    pub fn new() -> Self {
        Self {
            lanes: BTreeSet::new(),
            files: BTreeMap::new(),
        }
    }

    pub fn lane_ids(&self) -> impl Iterator<Item = &str> {
        self.lanes.iter().map(String::as_str)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    pub fn overlay_paths(&self, lane: &str) -> Result<Vec<&str>, LaneError> {
        self.ensure_lane(lane)?;
        Ok(self
            .files
            .iter()
            .filter_map(|(path, file)| file.has_lane(lane).then_some(path.as_str()))
            .collect())
    }

    pub fn create_lane(&mut self, lane: impl Into<LaneId>) -> Result<bool, LaneError> {
        let lane = lane.into();
        ensure_user_lane(&lane)?;
        Ok(self.lanes.insert(lane))
    }

    pub fn discard_lane(&mut self, lane: &str) -> bool {
        let removed = self.lanes.remove(lane);
        for file in self.files.values_mut() {
            file.discard_lane(lane);
        }
        self.files.retain(|_, file| !file.is_empty());
        removed
    }

    pub fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        if lane == "base" {
            return Ok(base.to_vec());
        }
        self.ensure_lane(lane)?;
        match self.files.get(path) {
            Some(file) => file.read(path, lane, base),
            None => Ok(base.to_vec()),
        }
    }

    pub fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_lane(lane)?;
        if let Some(file) = self.files.get_mut(path) {
            file.write(path, lane, base, range, replacement)?;
            if file.is_empty() {
                self.files.remove(path);
            }
            return Ok(());
        }

        let mut file = LaneFile::new(base);
        file.write(path, lane, base, range, replacement)?;
        if !file.is_empty() {
            self.files.insert(path.to_owned(), file);
        }
        Ok(())
    }

    pub fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        content: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        let current_len = self.read(path, lane, base)?.len() as u64;
        self.write(path, lane, base, 0..current_len, content)
    }

    pub fn delete(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
    ) -> Result<(), LaneError> {
        self.write(path, lane, base, range, Vec::new())
    }

    pub fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get_mut(path) else {
            return Ok(base.to_vec());
        };

        let promoted = file.promote(path, lane, base)?;
        if file.is_empty() {
            self.files.remove(path);
        }
        Ok(promoted)
    }

    pub fn promote_lane(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Vec<u8>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        let base_by_path: BTreeMap<_, _> = bases.into_iter().collect();
        let mut changed_bases = Vec::new();
        for path in self.overlay_paths(lane)? {
            let base = base_by_path
                .get(path)
                .ok_or_else(|| LaneError::BaseMissing {
                    path: path.to_owned(),
                })?;
            if self.read(path, lane, base)? != *base {
                changed_bases.push((path.to_owned(), base.clone()));
            }
        }
        self.promote_paths(lane, changed_bases)
    }

    pub fn promote_paths(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Vec<u8>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        self.ensure_lane(lane)?;
        let mut draft = self.clone();
        let mut promoted = Vec::new();

        for (path, base) in bases {
            promoted.push(PromotedFile {
                bytes: draft.promote(&path, lane, &base)?,
                path,
            });
        }

        *self = draft;
        Ok(promoted)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LANEREPO\0\0\0\x01");

        write_u64(&mut bytes, self.lanes.len() as u64);
        for lane in &self.lanes {
            write_bytes(&mut bytes, lane.as_bytes());
        }

        write_u64(&mut bytes, self.files.len() as u64);
        for (path, file) in &self.files {
            write_bytes(&mut bytes, path.as_bytes());
            write_u64(&mut bytes, file.base_hash);

            write_u64(&mut bytes, file.blobs.len() as u64);
            for blob in &file.blobs {
                write_bytes(&mut bytes, blob);
            }

            write_u64(&mut bytes, file.lanes.len() as u64);
            for (lane, view) in &file.lanes {
                write_bytes(&mut bytes, lane.as_bytes());
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
        }

        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        cursor.expect(b"LANEREPO\0\0\0\x01")?;

        let mut lanes = BTreeSet::new();
        for _ in 0..cursor.read_u64()? {
            lanes.insert(read_string(&mut cursor)?);
        }

        let mut files = BTreeMap::new();
        for _ in 0..cursor.read_u64()? {
            let path = read_string(&mut cursor)?;
            let base_hash = cursor.read_u64()?;

            let mut blobs = Vec::new();
            for _ in 0..cursor.read_u64()? {
                blobs.push(cursor.read_bytes()?.to_vec());
            }

            let mut overlays = BTreeMap::new();
            for _ in 0..cursor.read_u64()? {
                let lane = read_string(&mut cursor)?;
                let mut extents = Vec::new();
                for _ in 0..cursor.read_u64()? {
                    let source = match cursor.read_byte()? {
                        0 => Source::Base,
                        1 => Source::Blob(cursor.read_u64()?),
                        tag => return Err(DecodeError::InvalidSource(tag)),
                    };
                    let start = cursor.read_u64()?;
                    let len = cursor.read_u64()?;
                    extents.push(Extent { source, start, len });
                }
                overlays.insert(
                    lane,
                    LaneView {
                        extents: normalize_extents_checked(extents)?,
                    },
                );
            }

            files.insert(
                path,
                LaneFile {
                    base_hash,
                    blobs,
                    lanes: overlays,
                },
            );
        }

        let repo = Self { lanes, files };
        repo.validate()?;
        if !cursor.is_finished() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(repo)
    }

    fn ensure_lane(&self, lane: &str) -> Result<(), LaneError> {
        if self.lanes.contains(lane) {
            Ok(())
        } else {
            Err(LaneError::LaneMissing(lane.to_owned()))
        }
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for file in self.files.values() {
            for lane in file.lanes.keys() {
                if !self.lanes.contains(lane) {
                    return Err(DecodeError::OverlayLaneMissing(lane.clone()));
                }
            }
            file.validate()?;
        }
        Ok(())
    }
}

impl Default for LaneRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneFile {
    fn new(base: &[u8]) -> Self {
        Self {
            base_hash: hash_bytes(base),
            blobs: Vec::new(),
            lanes: BTreeMap::new(),
        }
    }

    fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.ensure_base(path, base)?;
        match self.lanes.get(lane) {
            Some(view) => self.render(view, base),
            None => Ok(base.to_vec()),
        }
    }

    fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_base(path, base)?;
        let replacement = replacement.into();
        let view = self.view_for(lane, base);
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

        let next_view = LaneView {
            extents: normalize_extents(next),
        };
        if self.render(&next_view, base)? == base {
            self.lanes.remove(lane);
        } else {
            self.lanes.insert(lane.to_owned(), next_view);
        }
        Ok(())
    }

    fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.ensure_base(path, base)?;
        let promoted = self.read(path, lane, base)?;

        let lanes: Vec<_> = self.lanes.keys().cloned().collect();
        let mut preserved = BTreeMap::new();
        for lane_id in lanes {
            let bytes = self.read(path, &lane_id, base)?;
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

        self.base_hash = hash_bytes(&promoted);
        self.lanes = preserved;
        Ok(promoted)
    }

    fn discard_lane(&mut self, lane: &str) {
        self.lanes.remove(lane);
    }

    fn has_lane(&self, lane: &str) -> bool {
        self.lanes.contains_key(lane)
    }

    fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    fn view_for(&self, lane: &str, base: &[u8]) -> LaneView {
        self.lanes.get(lane).cloned().unwrap_or_else(|| LaneView {
            extents: vec![Extent {
                source: Source::Base,
                start: 0,
                len: base.len() as u64,
            }],
        })
    }

    fn render(&self, view: &LaneView, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        let mut bytes = Vec::with_capacity(extents_len(&view.extents) as usize);
        for extent in &view.extents {
            let source = match extent.source {
                Source::Base => base,
                Source::Blob(blob_id) => self
                    .blobs
                    .get(blob_id as usize)
                    .ok_or(LaneError::BlobMissing(blob_id))?,
            };
            let start: usize = extent
                .start
                .try_into()
                .map_err(|_| LaneError::ExtentOutOfBounds)?;
            let len: usize = extent
                .len
                .try_into()
                .map_err(|_| LaneError::ExtentOutOfBounds)?;
            let end = start.checked_add(len).ok_or(LaneError::ExtentOutOfBounds)?;
            let slice = source.get(start..end).ok_or(LaneError::ExtentOutOfBounds)?;
            bytes.extend_from_slice(slice);
        }
        Ok(bytes)
    }

    fn push_blob(&mut self, bytes: Vec<u8>) -> u64 {
        let blob_id = self.blobs.len() as u64;
        self.blobs.push(bytes);
        blob_id
    }

    fn ensure_base(&self, path: &str, base: &[u8]) -> Result<(), LaneError> {
        if self.base_hash == hash_bytes(base) {
            Ok(())
        } else {
            Err(LaneError::BaseChanged {
                path: path.to_owned(),
            })
        }
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for view in self.lanes.values() {
            for extent in &view.extents {
                let Source::Blob(blob_id) = extent.source else {
                    continue;
                };
                let source_len = self
                    .blobs
                    .get(blob_id as usize)
                    .ok_or(DecodeError::BlobMissing(blob_id))?
                    .len() as u64;
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
    OverlayLaneMissing(LaneId),
    TrailingBytes,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DecodeError {}

fn ensure_user_lane(lane: &str) -> Result<(), LaneError> {
    if lane.trim().is_empty() || lane == "base" {
        Err(LaneError::ReservedLane(lane.to_owned()))
    } else {
        Ok(())
    }
}

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

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_string(cursor: &mut Cursor<'_>) -> Result<String, DecodeError> {
    String::from_utf8(cursor.read_bytes()?.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
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
