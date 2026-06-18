use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::ffi::ErrorCode;
use rusqlite::{Connection, OpenFlags, params};

use crate::{
    BaseFingerprint, BaseStorageSnapshot, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneFileStorageSnapshot, LaneId, LaneRepoStorageSnapshot,
};

use super::atomic::{replace_file_with_retry, temp_path_for};
use super::blobs::{hex, persist_blob, read_blob, sha256_hex};
use super::serde_util::invalid_storage;

pub(super) const STORE_VERSION: u32 = 3;

const BASE_PRESENT: &str = "present";
const BASE_MISSING: &str = "missing";
const ENTRY_PRESENT: &str = "present";
const ENTRY_DELETED: &str = "deleted";

pub(super) fn load_db_snapshot(storage_root: &Path) -> io::Result<Option<LaneRepoStorageSnapshot>> {
    let Some(stored) = load_stored_repo(storage_root)? else {
        return Ok(None);
    };
    stored_repo_to_snapshot(storage_root, stored).map(Some)
}

pub(super) fn persist_db_snapshot(
    storage_root: &Path,
    snapshot: &LaneRepoStorageSnapshot,
) -> io::Result<()> {
    fs::create_dir_all(storage_root)?;
    let db_path = super::paths::db_path(storage_root);
    let temp_path = temp_path_for(&db_path)?;
    remove_sqlite_temp_files(&temp_path)?;

    let result = (|| {
        write_snapshot_database(storage_root, &temp_path, snapshot)?;
        replace_file_with_retry(&temp_path, &db_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("replace database {}: {error}", db_path.display()),
            )
        })
    })();

    if let Err(error) = result {
        let _ = remove_sqlite_temp_files(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn write_snapshot_database(
    storage_root: &Path,
    db_path: &Path,
    snapshot: &LaneRepoStorageSnapshot,
) -> io::Result<()> {
    let mut connection = Connection::open(db_path).map_err(sqlite_error)?;
    initialize_schema(&connection)?;
    let transaction = connection.transaction().map_err(sqlite_error)?;
    transaction
        .execute(
            "INSERT INTO store_meta (key, value) VALUES ('schema_version', ?1)",
            [STORE_VERSION.to_string()],
        )
        .map_err(sqlite_error)?;

    {
        let mut insert_lane = transaction
            .prepare("INSERT INTO lanes (name) VALUES (?1)")
            .map_err(sqlite_error)?;
        for lane in &snapshot.lanes {
            insert_lane.execute([lane.as_str()]).map_err(sqlite_error)?;
        }
    }

    let mut persisted_blobs = BTreeSet::new();
    {
        let mut insert_file = transaction
            .prepare(
                "
                INSERT INTO files (path, base_state, base_fingerprint)
                VALUES (?1, ?2, ?3)
                ",
            )
            .map_err(sqlite_error)?;
        let mut insert_entry = transaction
            .prepare(
                "
                INSERT INTO lane_entries (path, lane, state)
                VALUES (?1, ?2, ?3)
                ",
            )
            .map_err(sqlite_error)?;
        let mut insert_op = transaction
            .prepare(
                "
                INSERT INTO ops
                    (path, lane, ordinal, id, base_start, base_len, order_key, inserted_blob, inserted_len)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ",
            )
            .map_err(sqlite_error)?;

        for (path, file) in &snapshot.files {
            let (base_state, base_fingerprint) = stored_base(&file.base);
            insert_file
                .execute(params![path.as_str(), base_state, base_fingerprint])
                .map_err(sqlite_error)?;

            for (lane, entry) in &file.lanes {
                let state = match entry {
                    LaneEntryStorageSnapshot::Present(_) => ENTRY_PRESENT,
                    LaneEntryStorageSnapshot::Deleted => ENTRY_DELETED,
                };
                insert_entry
                    .execute(params![path.as_str(), lane.as_str(), state])
                    .map_err(sqlite_error)?;

                let LaneEntryStorageSnapshot::Present(ops) = entry else {
                    continue;
                };
                for (ordinal, op) in ops.iter().enumerate() {
                    let inserted_blob = format!("sha256/{}", sha256_hex(&op.inserted));
                    if persisted_blobs.insert(inserted_blob.clone()) {
                        persist_blob(storage_root, &inserted_blob, &op.inserted)?;
                    }
                    insert_op
                        .execute(params![
                            path.as_str(),
                            lane.as_str(),
                            i64::try_from(ordinal).map_err(invalid_ordinal)?,
                            op.id.to_string(),
                            op.base_start.to_string(),
                            op.base_len.to_string(),
                            op.order_key.as_str(),
                            inserted_blob.as_str(),
                            op.inserted.len().to_string()
                        ])
                        .map_err(sqlite_error)?;
                }
            }
        }
    }

    transaction.commit().map_err(sqlite_error)?;
    connection.close().map_err(|(_, error)| sqlite_error(error))
}

fn remove_sqlite_temp_files(db_path: &Path) -> io::Result<()> {
    for path in [
        db_path.to_path_buf(),
        sqlite_sidecar_path(db_path, "-journal"),
        sqlite_sidecar_path(db_path, "-wal"),
        sqlite_sidecar_path(db_path, "-shm"),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut file_name = db_path
        .file_name()
        .expect("database path has a file name")
        .to_os_string();
    file_name.push(suffix);
    db_path.with_file_name(file_name)
}

pub(super) fn load_stored_repo(storage_root: &Path) -> io::Result<Option<StoredRepo>> {
    let db_path = super::paths::db_path(storage_root);
    if !db_path.exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sqlite_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(sqlite_error)?;
    let version = read_store_version(&connection, &db_path)?;
    if version != STORE_VERSION {
        return Err(invalid_storage(
            &db_path,
            format!("unsupported lane storage version {version}; expected {STORE_VERSION}"),
        ));
    }

    let lanes = read_lanes(&connection)?;
    let files = read_files(&connection)?;
    Ok(Some(StoredRepo {
        version,
        lanes,
        files,
    }))
}

fn initialize_schema(connection: &Connection) -> io::Result<()> {
    connection
        .execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS store_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS lanes (
                name TEXT PRIMARY KEY NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY NOT NULL,
                base_state TEXT NOT NULL CHECK (base_state IN ('present', 'missing')),
                base_fingerprint TEXT
            );

            CREATE TABLE IF NOT EXISTS lane_entries (
                path TEXT NOT NULL,
                lane TEXT NOT NULL,
                state TEXT NOT NULL CHECK (state IN ('present', 'deleted')),
                PRIMARY KEY (path, lane),
                FOREIGN KEY (path) REFERENCES files(path) ON DELETE CASCADE,
                FOREIGN KEY (lane) REFERENCES lanes(name) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS ops (
                path TEXT NOT NULL,
                lane TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                id TEXT NOT NULL,
                base_start TEXT NOT NULL,
                base_len TEXT NOT NULL,
                order_key TEXT NOT NULL,
                inserted_blob TEXT NOT NULL,
                inserted_len TEXT NOT NULL,
                PRIMARY KEY (path, lane, ordinal),
                FOREIGN KEY (path, lane) REFERENCES lane_entries(path, lane) ON DELETE CASCADE
            );
            ",
        )
        .map_err(sqlite_error)
}

fn read_store_version(connection: &Connection, db_path: &Path) -> io::Result<u32> {
    let version = connection
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(sqlite_error)?;
    version.parse::<u32>().map_err(|error| {
        invalid_storage(
            db_path,
            format!("schema_version {version:?} is not a u32: {error}"),
        )
    })
}

fn read_lanes(connection: &Connection) -> io::Result<BTreeSet<LaneId>> {
    let mut lanes = BTreeSet::new();
    let mut statement = connection
        .prepare("SELECT name FROM lanes ORDER BY name")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sqlite_error)?;
    for row in rows {
        let lane = row
            .map_err(sqlite_error)
            .and_then(|raw| LaneId::parse(&raw).map_err(invalid_lane))?;
        lanes.insert(lane);
    }
    Ok(lanes)
}

fn read_files(connection: &Connection) -> io::Result<BTreeMap<FilePath, StoredFile>> {
    let mut files = BTreeMap::new();
    let mut statement = connection
        .prepare("SELECT path, base_state, base_fingerprint FROM files ORDER BY path")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(sqlite_error)?;

    for row in rows {
        let (raw_path, base_state, base_fingerprint) = row.map_err(sqlite_error)?;
        let path = FilePath::parse(&raw_path).map_err(invalid_path)?;
        let file = StoredFile {
            base: parse_base(&base_state, base_fingerprint)?,
            lanes: BTreeMap::new(),
        };
        if files.insert(path.clone(), file).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "database contains duplicate file path after normalization: {:?}",
                    path.as_str()
                ),
            ));
        }
    }

    read_lane_entries(connection, &mut files)?;
    read_ops(connection, &mut files)?;
    Ok(files)
}

fn read_lane_entries(
    connection: &Connection,
    files: &mut BTreeMap<FilePath, StoredFile>,
) -> io::Result<()> {
    let mut statement = connection
        .prepare("SELECT path, lane, state FROM lane_entries ORDER BY path, lane")
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sqlite_error)?;

    for row in rows {
        let (raw_path, raw_lane, state) = row.map_err(sqlite_error)?;
        let path = FilePath::parse(&raw_path).map_err(invalid_path)?;
        let lane = LaneId::parse(&raw_lane).map_err(invalid_lane)?;
        let entry = match state.as_str() {
            ENTRY_PRESENT => StoredLaneEntry::Present(Vec::new()),
            ENTRY_DELETED => StoredLaneEntry::Deleted,
            state => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("database entry {raw_path}:{raw_lane} has invalid state {state:?}"),
                ));
            }
        };
        let file = files.get_mut(&path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("database entry {raw_path}:{raw_lane} references missing file"),
            )
        })?;
        if file.lanes.insert(lane.clone(), entry).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "database file {} contains duplicate lane entry after normalization: {}",
                    path.as_str(),
                    lane.as_str()
                ),
            ));
        }
    }

    Ok(())
}

fn read_ops(connection: &Connection, files: &mut BTreeMap<FilePath, StoredFile>) -> io::Result<()> {
    let mut statement = connection
        .prepare(
            "
            SELECT path, lane, id, base_start, base_len, order_key, inserted_blob, inserted_len
            FROM ops
            ORDER BY path, lane, ordinal
            ",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(sqlite_error)?;

    for row in rows {
        let (raw_path, raw_lane, id, base_start, base_len, order_key, inserted_blob, inserted_len) =
            row.map_err(sqlite_error)?;
        let path = FilePath::parse(&raw_path).map_err(invalid_path)?;
        let lane = LaneId::parse(&raw_lane).map_err(invalid_lane)?;
        let op = StoredOp {
            id: parse_u64(&id, "id")?,
            base_start: parse_u64(&base_start, "base_start")?,
            base_len: parse_u64(&base_len, "base_len")?,
            order_key,
            inserted_blob,
            inserted_len: parse_u64(&inserted_len, "inserted_len")?,
        };
        let file = files.get_mut(&path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "database op {raw_path}:{raw_lane}:{} references missing file",
                    op.id
                ),
            )
        })?;
        let entry = file.lanes.get_mut(&lane).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "database op {raw_path}:{raw_lane}:{} references missing entry",
                    op.id
                ),
            )
        })?;
        match entry {
            StoredLaneEntry::Present(ops) => ops.push(op),
            StoredLaneEntry::Deleted => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("deleted database entry {raw_path}:{raw_lane} has stored ops"),
                ));
            }
        }
    }
    Ok(())
}

fn stored_repo_to_snapshot(
    storage_root: &Path,
    stored: StoredRepo,
) -> io::Result<LaneRepoStorageSnapshot> {
    let files = stored
        .files
        .into_iter()
        .map(|(path, file)| stored_file_to_snapshot(storage_root, file).map(|file| (path, file)))
        .collect::<io::Result<_>>()?;
    Ok(LaneRepoStorageSnapshot {
        lanes: stored.lanes,
        files,
    })
}

fn stored_file_to_snapshot(
    storage_root: &Path,
    file: StoredFile,
) -> io::Result<LaneFileStorageSnapshot> {
    let lanes = file
        .lanes
        .into_iter()
        .map(|(lane, entry)| {
            stored_entry_to_snapshot(storage_root, entry).map(|entry| (lane, entry))
        })
        .collect::<io::Result<_>>()?;
    Ok(LaneFileStorageSnapshot {
        base: file.base,
        lanes,
    })
}

fn stored_entry_to_snapshot(
    storage_root: &Path,
    entry: StoredLaneEntry,
) -> io::Result<LaneEntryStorageSnapshot> {
    match entry {
        StoredLaneEntry::Deleted => Ok(LaneEntryStorageSnapshot::Deleted),
        StoredLaneEntry::Present(ops) => Ok(LaneEntryStorageSnapshot::Present(
            ops.into_iter()
                .map(|op| {
                    Ok(FileOpStorageSnapshot {
                        id: op.id,
                        base_start: op.base_start,
                        base_len: op.base_len,
                        order_key: op.order_key,
                        inserted: read_blob(storage_root, &op.inserted_blob)?,
                    })
                })
                .collect::<io::Result<_>>()?,
        )),
    }
}

fn stored_base(base: &BaseStorageSnapshot) -> (&'static str, Option<String>) {
    match base {
        BaseStorageSnapshot::Present(fingerprint) => (BASE_PRESENT, Some(hex(fingerprint))),
        BaseStorageSnapshot::Missing => (BASE_MISSING, None),
    }
}

fn parse_base(state: &str, fingerprint: Option<String>) -> io::Result<BaseStorageSnapshot> {
    match state {
        BASE_PRESENT => {
            let fingerprint = fingerprint.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "present database base is missing fingerprint",
                )
            })?;
            Ok(BaseStorageSnapshot::Present(parse_fingerprint(
                &fingerprint,
            )?))
        }
        BASE_MISSING => {
            if fingerprint.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing database base must not have a fingerprint",
                ));
            }
            Ok(BaseStorageSnapshot::Missing)
        }
        state => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("database file has invalid base state {state:?}"),
        )),
    }
}

pub(super) fn parse_fingerprint(value: &str) -> io::Result<BaseFingerprint> {
    let bytes = parse_hex(value)?;
    if bytes.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base fingerprint must be 32 bytes",
        ));
    }
    let mut fingerprint = [0; 32];
    fingerprint.copy_from_slice(&bytes);
    Ok(fingerprint)
}

fn parse_hex(value: &str) -> io::Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hex value must have an even length",
        ));
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|chunk| {
            let hi = hex_digit(chunk[0])?;
            let lo = hex_digit(chunk[1])?;
            Ok((hi << 4) | lo)
        })
        .collect()
}

fn hex_digit(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hex value contains a non-hex digit",
        )),
    }
}

fn parse_u64(value: &str, field: &str) -> io::Result<u64> {
    value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("database field {field} value {value:?} is not a u64: {error}"),
        )
    })
}

fn invalid_lane(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn invalid_path(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn invalid_ordinal(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("too many stored operations for one path/lane: {error}"),
    )
}

fn sqlite_error(error: rusqlite::Error) -> io::Error {
    let kind = match &error {
        rusqlite::Error::SqliteFailure(error, _) => sqlite_error_kind(error.code),
        rusqlite::Error::InvalidPath(_) => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error)
}

fn sqlite_error_kind(code: ErrorCode) -> io::ErrorKind {
    match code {
        ErrorCode::PermissionDenied
        | ErrorCode::ReadOnly
        | ErrorCode::AuthorizationForStatementDenied => io::ErrorKind::PermissionDenied,
        ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => io::ErrorKind::WouldBlock,
        ErrorCode::SystemIoFailure
        | ErrorCode::CannotOpen
        | ErrorCode::FileLockingProtocolFailed => io::ErrorKind::Other,
        ErrorCode::DiskFull => io::ErrorKind::StorageFull,
        ErrorCode::OperationAborted | ErrorCode::OperationInterrupted => io::ErrorKind::Interrupted,
        ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::InvalidData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_error_maps_storage_failures_to_specific_io_kinds() {
        assert_eq!(
            sqlite_error(sqlite_failure(ErrorCode::DiskFull)).kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            sqlite_error(sqlite_failure(ErrorCode::ReadOnly)).kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            sqlite_error(sqlite_failure(ErrorCode::DatabaseBusy)).kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            sqlite_error(sqlite_failure(ErrorCode::DatabaseCorrupt)).kind(),
            io::ErrorKind::InvalidData
        );
    }

    fn sqlite_failure(code: ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            None,
        )
    }
}

#[derive(Clone, Debug)]
pub(super) struct StoredRepo {
    pub(super) version: u32,
    pub(super) lanes: BTreeSet<LaneId>,
    pub(super) files: BTreeMap<FilePath, StoredFile>,
}

#[derive(Clone, Debug)]
pub(super) struct StoredFile {
    pub(super) base: BaseStorageSnapshot,
    pub(super) lanes: BTreeMap<LaneId, StoredLaneEntry>,
}

#[derive(Clone, Debug)]
pub(super) enum StoredLaneEntry {
    Present(Vec<StoredOp>),
    Deleted,
}

#[derive(Clone, Debug)]
pub(super) struct StoredOp {
    pub(super) id: u64,
    pub(super) base_start: u64,
    pub(super) base_len: u64,
    pub(super) order_key: String,
    pub(super) inserted_blob: String,
    pub(super) inserted_len: u64,
}
