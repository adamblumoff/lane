use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use lane::LaneFile;
use serde::{Deserialize, Serialize};

const SAMPLE_BASE: &[u8] = b"export const mode = 'base';\n\nexport function describeLane() {\n  return `current mode: ${mode}`;\n}\n";
const STORAGE_PATH: &str = "demo/example.lane";

#[tokio::main]
async fn main() {
    let app_state = AppState::new(storage_path()).expect("load lane storage");
    let app = Router::new()
        .route("/api/state", get(state))
        .route("/api/lanes/{lane}/write", post(write_range))
        .route("/api/lanes/{lane}/replace", post(replace_lane))
        .route("/api/lanes/{lane}/delete", post(delete_range))
        .route("/api/lanes/{lane}/promote", post(promote_lane))
        .route("/api/lanes/{lane}/discard", post(discard_lane))
        .route("/api/reset", post(reset))
        .with_state(app_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3001));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind lane server");
    println!("lane server listening on http://{addr}");
    axum::serve(listener, app).await.expect("serve lane server");
}

#[derive(Clone)]
struct AppState {
    file: Arc<Mutex<LaneFile>>,
    storage_path: Arc<PathBuf>,
    storage_label: Arc<String>,
}

impl AppState {
    fn new(storage_path: PathBuf) -> io::Result<Self> {
        let file = load_or_seed_file(&storage_path)?;
        let storage_label = storage_label(&storage_path);
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            storage_path: Arc::new(storage_path),
            storage_label: Arc::new(storage_label),
        })
    }
}

#[derive(Serialize)]
struct StateResponse {
    file_path: String,
    base: ViewResponse,
    lanes: Vec<ViewResponse>,
}

#[derive(Serialize)]
struct ViewResponse {
    id: String,
    content: String,
    byte_len: usize,
}

#[derive(Deserialize)]
struct WriteRequest {
    start: u64,
    end: u64,
    replacement: String,
}

#[derive(Deserialize)]
struct ReplaceRequest {
    content: String,
}

async fn state(State(state): State<AppState>) -> Result<Json<StateResponse>, ApiError> {
    let file = state.file.lock().expect("lane file mutex");
    Ok(Json(snapshot(&state, &file)?))
}

async fn write_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_file(&state, |file| {
        file.write(lane, request.start..request.end, request.replacement)
            .map_err(|error| ApiError::bad_request(format!("{error:?}")))
    })
}

async fn replace_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<ReplaceRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_file(&state, |file| {
        let current_len = file
            .read(&lane)
            .map_err(|error| ApiError::bad_request(format!("{error:?}")))?
            .len() as u64;
        file.write(lane, 0..current_len, request.content)
            .map_err(|error| ApiError::bad_request(format!("{error:?}")))
    })
}

async fn delete_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_file(&state, |file| {
        file.delete(lane, request.start..request.end)
            .map_err(|error| ApiError::bad_request(format!("{error:?}")))
    })
}

async fn promote_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_file(&state, |file| {
        file.promote(&lane)
            .map_err(|error| ApiError::bad_request(format!("{error:?}")))
    })
}

async fn discard_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_file(&state, |file| {
        file.discard(&lane);
        Ok(())
    })
}

async fn reset(State(state): State<AppState>) -> Result<Json<StateResponse>, ApiError> {
    mutate_file(&state, |file| {
        *file = seed_file();
        Ok(())
    })
}

fn storage_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(STORAGE_PATH)
}

fn storage_label(path: &FsPath) -> String {
    let label = path
        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or(path)
        .to_string_lossy();
    label.trim_start_matches(['\\', '/']).replace('\\', "/")
}

fn load_or_seed_file(path: &FsPath) -> io::Result<LaneFile> {
    match fs::read(path) {
        Ok(bytes) => LaneFile::from_bytes(&bytes).map_err(decode_error),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let file = seed_file();
            persist_file(path, &file)?;
            Ok(file)
        }
        Err(error) => Err(error),
    }
}

fn persist_file(path: &FsPath, file: &LaneFile) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp_path = temp_path_for(path)?;
    let result = (|| {
        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(&file.to_bytes())?;
        temp_file.sync_all()?;
        drop(temp_file);
        replace_file(&temp_path, path)
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn temp_path_for(path: &FsPath) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing storage file name"))?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    Ok(path.with_file_name(temp_name))
}

#[cfg(not(windows))]
fn replace_file(from: &FsPath, to: &FsPath) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &FsPath, to: &FsPath) -> io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let from = windows_path(from);
    let to = windows_path(to);
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_path(path: &FsPath) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn decode_error(error: lane::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn mutate_file(
    state: &AppState,
    mutate: impl FnOnce(&mut LaneFile) -> Result<(), ApiError>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut file = state.file.lock().expect("lane file mutex");
    let mut draft = file.clone();
    mutate(&mut draft)?;
    persist_file(state.storage_path.as_ref(), &draft)
        .map_err(|error| ApiError::server_error(format!("persist lane storage: {error}")))?;
    *file = draft;
    Ok(Json(snapshot(state, &file)?))
}

fn seed_file() -> LaneFile {
    let mut file = LaneFile::new(SAMPLE_BASE.to_vec());
    file.write("agent-a", 21..25, "fast").expect("seed agent-a");
    file.write("agent-b", 21..25, "safe").expect("seed agent-b");
    file
}

fn snapshot(state: &AppState, file: &LaneFile) -> Result<StateResponse, ApiError> {
    let file_path = state.storage_label.as_ref().clone();
    let base = render_view("base", file.read_base());
    let lanes = file
        .lane_ids()
        .map(|lane| {
            file.read(lane)
                .map(|bytes| render_view(lane, bytes))
                .map_err(|error| ApiError::server_error(format!("{error:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(StateResponse {
        file_path,
        base,
        lanes,
    })
}

fn render_view(id: &str, bytes: Vec<u8>) -> ViewResponse {
    let byte_len = bytes.len();
    ViewResponse {
        id: id.to_owned(),
        content: String::from_utf8_lossy(&bytes).into_owned(),
        byte_len,
    }
}

fn ensure_lane_id(lane: &str) -> Result<(), ApiError> {
    if lane.trim().is_empty() || lane == "base" {
        Err(ApiError::bad_request(format!("reserved lane id: {lane}")))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }

    fn server_error(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn storage_survives_edit_promote_and_reset_reloads() {
        let path = temp_storage_path();
        let mut file = load_or_seed_file(&path).unwrap();

        assert!(path.exists());
        assert!(!temp_path_for(&path).unwrap().exists());
        assert_contents(&file, "base", "base");
        assert_contents(&file, "agent-a", "fast");
        assert_contents(&file, "agent-b", "safe");

        let edited = content_for_mode("manual");
        let current_len = file.read("agent-a").unwrap().len() as u64;
        file.write("agent-a", 0..current_len, edited.as_bytes().to_vec())
            .unwrap();
        persist_file(&path, &file).unwrap();
        assert!(!temp_path_for(&path).unwrap().exists());

        let mut reloaded = load_or_seed_file(&path).unwrap();
        assert_eq!(reloaded.read_base(), SAMPLE_BASE);
        assert_eq!(reloaded.read("agent-a").unwrap(), edited.as_bytes());
        assert_contents(&reloaded, "agent-b", "safe");

        reloaded.promote("agent-a").unwrap();
        persist_file(&path, &reloaded).unwrap();
        assert!(!temp_path_for(&path).unwrap().exists());

        let promoted = load_or_seed_file(&path).unwrap();
        assert_eq!(promoted.read_base(), edited.as_bytes());
        assert_contents(&promoted, "agent-b", "safe");

        persist_file(&path, &seed_file()).unwrap();
        assert!(!temp_path_for(&path).unwrap().exists());

        let reset = load_or_seed_file(&path).unwrap();
        assert_contents(&reset, "base", "base");
        assert_contents(&reset, "agent-a", "fast");
        assert_contents(&reset, "agent-b", "safe");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_persist_does_not_commit_memory() {
        let blocked_path = temp_storage_path();
        fs::create_dir(&blocked_path).unwrap();

        let state = AppState {
            file: Arc::new(Mutex::new(seed_file())),
            storage_path: Arc::new(blocked_path.clone()),
            storage_label: Arc::new("blocked".to_owned()),
        };

        let result = mutate_file(&state, |file| {
            let current_len = file.read("agent-a").unwrap().len() as u64;
            file.write("agent-a", 0..current_len, content_for_mode("manual"))
                .map_err(|error| ApiError::bad_request(format!("{error:?}")))
        });

        assert!(result.is_err());
        let file = state.file.lock().unwrap();
        assert_contents(&file, "agent-a", "fast");

        fs::remove_dir(blocked_path).unwrap();
    }

    #[test]
    fn storage_label_matches_the_project_backing_file() {
        assert_eq!(storage_label(&storage_path()), "demo/example.lane");
    }

    fn assert_contents(file: &LaneFile, lane: &str, mode: &str) {
        let actual = if lane == "base" {
            file.read_base()
        } else {
            file.read(lane).unwrap()
        };
        assert_eq!(actual, content_for_mode(mode).as_bytes());
    }

    fn content_for_mode(mode: &str) -> String {
        format!(
            "export const mode = '{mode}';\n\nexport function describeLane() {{\n  return `current mode: ${{mode}}`;\n}}\n"
        )
    }

    fn temp_storage_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("lane-{unique}.lane"))
    }
}
