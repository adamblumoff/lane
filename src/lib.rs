use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

pub mod cli;
pub mod storage;
pub mod vfs;

#[cfg(windows)]
pub mod winfsp_mount;

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
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneFile {
    base: BaseState,
    blobs: Vec<Vec<u8>>,
    lanes: BTreeMap<LaneId, LaneEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BaseState {
    Present(u64),
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaneEntry {
    Present(LaneView),
    Deleted,
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

    pub fn read_path(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        if lane == "base" {
            return Ok(base.map(<[u8]>::to_vec));
        }
        self.ensure_lane(lane)?;
        match self.files.get(path) {
            Some(file) => file.read(path, lane, base),
            None => Ok(base.map(<[u8]>::to_vec)),
        }
    }

    pub fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.read_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }

    pub fn write_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        let replacement = replacement.into();
        let mut current = self.read_path(path, lane, base)?.unwrap_or_else(Vec::new);
        ensure_valid_range(range.clone(), current.len() as u64)?;

        let start: usize = range
            .start
            .try_into()
            .map_err(|_| LaneError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: current.len() as u64,
            })?;
        let end: usize = range
            .end
            .try_into()
            .map_err(|_| LaneError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: current.len() as u64,
            })?;
        current.splice(start..end, replacement);
        self.replace_path(path, lane, base, Some(current))
    }

    pub fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.write_path(path, lane, Some(base), range, replacement)
    }

    pub fn replace_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        content: Option<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_lane(lane)?;
        if let Some(file) = self.files.get_mut(path) {
            file.replace(path, lane, base, content)?;
            if file.is_empty() {
                self.files.remove(path);
            }
            return Ok(());
        }

        let mut file = LaneFile::new(base);
        file.replace(path, lane, base, content)?;
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
        self.replace_path(path, lane, Some(base), Some(content.into()))
    }

    pub fn delete_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<(), LaneError> {
        self.replace_path(path, lane, base, None)
    }

    pub fn delete(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
    ) -> Result<(), LaneError> {
        self.write_path(path, lane, Some(base), range, Vec::new())
    }

    pub fn promote_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get_mut(path) else {
            return Ok(base.map(<[u8]>::to_vec));
        };

        let promoted = file.promote(path, lane, base)?;
        if file.is_empty() {
            self.files.remove(path);
        }
        Ok(promoted)
    }

    pub fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.promote_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }

    pub fn promote_lane(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Option<Vec<u8>>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        let base_by_path: BTreeMap<_, _> = bases.into_iter().collect();
        let mut changed_bases = Vec::new();
        for path in self.overlay_paths(lane)? {
            let base = base_by_path
                .get(path)
                .ok_or_else(|| LaneError::BaseMissing {
                    path: path.to_owned(),
                })?;
            if self.read_path(path, lane, base.as_deref())? != *base {
                changed_bases.push((path.to_owned(), base.clone()));
            }
        }
        self.promote_paths(lane, changed_bases)
    }

    pub fn promote_paths(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Option<Vec<u8>>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        self.ensure_lane(lane)?;
        let mut draft = self.clone();
        let mut promoted = Vec::new();

        for (path, base) in bases {
            promoted.push(PromotedFile {
                bytes: draft.promote_path(&path, lane, base.as_deref())?,
                path,
            });
        }

        *self = draft;
        Ok(promoted)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LANEREPO\0\0\0\x02");

        write_u64(&mut bytes, self.lanes.len() as u64);
        for lane in &self.lanes {
            write_bytes(&mut bytes, lane.as_bytes());
        }

        write_u64(&mut bytes, self.files.len() as u64);
        for (path, file) in &self.files {
            write_bytes(&mut bytes, path.as_bytes());
            match file.base {
                BaseState::Present(hash) => {
                    bytes.push(1);
                    write_u64(&mut bytes, hash);
                }
                BaseState::Missing => {
                    bytes.push(0);
                    write_u64(&mut bytes, 0);
                }
            }

            write_u64(&mut bytes, file.blobs.len() as u64);
            for blob in &file.blobs {
                write_bytes(&mut bytes, blob);
            }

            write_u64(&mut bytes, file.lanes.len() as u64);
            for (lane, entry) in &file.lanes {
                write_bytes(&mut bytes, lane.as_bytes());
                match entry {
                    LaneEntry::Present(view) => {
                        bytes.push(1);
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
                    LaneEntry::Deleted => bytes.push(0),
                }
            }
        }

        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        cursor.expect(b"LANEREPO\0\0\0\x02")?;

        let mut lanes = BTreeSet::new();
        for _ in 0..cursor.read_u64()? {
            lanes.insert(read_string(&mut cursor)?);
        }

        let mut files = BTreeMap::new();
        for _ in 0..cursor.read_u64()? {
            let path = read_string(&mut cursor)?;
            let base = match cursor.read_byte()? {
                0 => {
                    cursor.read_u64()?;
                    BaseState::Missing
                }
                1 => BaseState::Present(cursor.read_u64()?),
                tag => return Err(DecodeError::InvalidBase(tag)),
            };

            let mut blobs = Vec::new();
            for _ in 0..cursor.read_u64()? {
                blobs.push(cursor.read_bytes()?.to_vec());
            }

            let mut overlays = BTreeMap::new();
            for _ in 0..cursor.read_u64()? {
                let lane = read_string(&mut cursor)?;
                let entry = match cursor.read_byte()? {
                    0 => LaneEntry::Deleted,
                    1 => {
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
                        LaneEntry::Present(LaneView {
                            extents: normalize_extents_checked(extents)?,
                        })
                    }
                    tag => return Err(DecodeError::InvalidEntry(tag)),
                };
                overlays.insert(lane, entry);
            }

            files.insert(
                path,
                LaneFile {
                    base,
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

impl BaseState {
    fn for_content(content: Option<&[u8]>) -> Self {
        match content {
            Some(bytes) => Self::Present(hash_bytes(bytes)),
            None => Self::Missing,
        }
    }
}

impl Default for LaneRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneFile {
    fn new(base: Option<&[u8]>) -> Self {
        Self {
            base: BaseState::for_content(base),
            blobs: Vec::new(),
            lanes: BTreeMap::new(),
        }
    }

    fn read(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        match self.lanes.get(lane) {
            Some(LaneEntry::Present(view)) => self.render(view, base.unwrap_or_default()).map(Some),
            Some(LaneEntry::Deleted) => Ok(None),
            None => Ok(base.map(<[u8]>::to_vec)),
        }
    }

    fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        content: Option<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_base(path, base)?;
        let entry = self.entry_for_content(base, content);
        match entry {
            Some(entry) => {
                self.lanes.insert(lane.to_owned(), entry);
            }
            None => {
                self.lanes.remove(lane);
            }
        };
        Ok(())
    }

    fn promote(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        let promoted = self.read(path, lane, base)?;

        let lanes: Vec<_> = self.lanes.keys().cloned().collect();
        let mut preserved = BTreeMap::new();
        for lane_id in lanes {
            let bytes = self.read(path, &lane_id, base)?;
            preserved.insert(lane_id, self.entry_for_snapshot(bytes));
        }

        self.base = BaseState::for_content(promoted.as_deref());
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

    fn entry_for_content(
        &mut self,
        base: Option<&[u8]>,
        content: Option<Vec<u8>>,
    ) -> Option<LaneEntry> {
        if content.as_deref() == base {
            return None;
        }
        match content {
            Some(bytes) => {
                let blob_id = self.push_blob(bytes);
                let len = self.blobs[blob_id as usize].len() as u64;
                Some(LaneEntry::Present(LaneView {
                    extents: vec![Extent {
                        source: Source::Blob(blob_id),
                        start: 0,
                        len,
                    }],
                }))
            }
            None => Some(LaneEntry::Deleted),
        }
    }

    fn entry_for_snapshot(&mut self, content: Option<Vec<u8>>) -> LaneEntry {
        match content {
            Some(bytes) => {
                let blob_id = self.push_blob(bytes);
                let len = self.blobs[blob_id as usize].len() as u64;
                LaneEntry::Present(LaneView {
                    extents: vec![Extent {
                        source: Source::Blob(blob_id),
                        start: 0,
                        len,
                    }],
                })
            }
            None => LaneEntry::Deleted,
        }
    }

    fn ensure_base(&self, path: &str, base: Option<&[u8]>) -> Result<(), LaneError> {
        if self.base == BaseState::for_content(base) {
            Ok(())
        } else {
            Err(LaneError::BaseChanged {
                path: path.to_owned(),
            })
        }
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for entry in self.lanes.values() {
            let LaneEntry::Present(view) = entry else {
                continue;
            };
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
    InvalidBase(u8),
    InvalidEntry(u8),
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
