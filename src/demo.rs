use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

use crate::projection::SourceProjection;
use crate::{LaneError, LaneRepo};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

pub const SAMPLE_BASE: &[u8] = b"export const mode = 'base';\n\nexport function describeLane() {\n  return `current mode: ${mode}`;\n}\n";
pub const DEMO_PATH: &str = "demo/example.ts";
pub const STORAGE_PATH: &str = ".lane/repo.lane";

pub fn router(app_state: AppState) -> Router {
    Router::new()
        .route("/api/state", get(state))
        .route("/api/read", get(read_projection))
        .route("/api/lanes/{lane}", post(create_lane))
        .route("/api/lanes/{lane}/write", post(write_range))
        .route("/api/lanes/{lane}/replace", post(replace_lane))
        .route("/api/lanes/{lane}/delete", post(delete_range))
        .route("/api/lanes/{lane}/promote", post(promote_lane))
        .route("/api/lanes/{lane}/discard", post(discard_lane))
        .route("/api/reset", post(reset))
        .with_state(app_state)
}

pub async fn serve(root_path: PathBuf, addr: SocketAddr) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("lane server listening on http://{addr}");
    axum::serve(listener, router(AppState::new(root_path)?)).await
}

#[derive(Clone)]
pub struct AppState {
    repo: Arc<Mutex<LaneRepo>>,
    root_path: Arc<PathBuf>,
    storage_path: Arc<PathBuf>,
    storage_label: Arc<String>,
    file_path: Arc<String>,
}

impl AppState {
    pub fn new(root_path: PathBuf) -> io::Result<Self> {
        let storage_path = root_path.join(STORAGE_PATH);
        let source_path = root_path.join(DEMO_PATH);
        let repo = load_or_seed_repo(&storage_path, &source_path)?;
        Ok(Self {
            repo: Arc::new(Mutex::new(repo)),
            root_path: Arc::new(root_path),
            storage_path: Arc::new(storage_path),
            storage_label: Arc::new(STORAGE_PATH.to_owned()),
            file_path: Arc::new(DEMO_PATH.to_owned()),
        })
    }
}

#[derive(Serialize)]
struct StateResponse {
    file_path: String,
    storage_path: String,
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
struct ReadQuery {
    path: String,
    lane: String,
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
    let repo = state.repo.lock().expect("lane repo mutex");
    Ok(Json(snapshot(&state, &repo)?))
}

async fn read_projection(
    State(state): State<AppState>,
    Query(query): Query<ReadQuery>,
) -> Result<Response, ApiError> {
    let path = normalize_repo_path(&query.path)?;
    let source_path = project_file_path(&state, &path)?;
    let repo = state.repo.lock().expect("lane repo mutex");
    let source = read_source_file(&source_path)?;
    let bytes = repo
        .read(&path, &query.lane, source.bytes())
        .map_err(api_error_for_lane)?;
    Ok((
        [(CONTENT_TYPE, "application/octet-stream")],
        source.materialize(&bytes),
    )
        .into_response())
}

async fn create_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_repo(&state, |repo| {
        repo.create_lane(lane).map_err(api_error_for_lane)?;
        Ok(())
    })
}

async fn write_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let path = state.file_path.as_ref().clone();
    let source = read_project_source(&state, &path)?;
    let replacement = source.project_edit(request.replacement.into_bytes());
    mutate_repo(&state, |repo| {
        repo.write(
            &path,
            &lane,
            source.bytes(),
            request.start..request.end,
            replacement,
        )
        .map_err(api_error_for_lane)
    })
}

async fn replace_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<ReplaceRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let path = state.file_path.as_ref().clone();
    let source = read_project_source(&state, &path)?;
    let content = source.project_edit(request.content.into_bytes());
    mutate_repo(&state, |repo| {
        repo.replace(&path, &lane, source.bytes(), content)
            .map_err(api_error_for_lane)
    })
}

async fn delete_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let path = state.file_path.as_ref().clone();
    let base = read_project_file(&state, &path)?;
    mutate_repo(&state, |repo| {
        repo.delete(&path, &lane, &base, request.start..request.end)
            .map_err(api_error_for_lane)
    })
}

async fn promote_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let path = state.file_path.as_ref().clone();
    let source_path = project_file_path(&state, &path)?;
    let source = read_source_file(&source_path)?;

    let mut repo = state.repo.lock().expect("lane repo mutex");
    let mut draft = repo.clone();
    let promoted = draft
        .promote(&path, &lane, source.bytes())
        .map_err(api_error_for_lane)?;

    persist_repo(state.storage_path.as_ref(), &draft)
        .map_err(|error| ApiError::server_error(format!("persist lane repo: {error}")))?;
    let worktree_bytes = source.materialize(&promoted);
    if let Err(error) = persist_bytes(&source_path, &worktree_bytes) {
        persist_repo(state.storage_path.as_ref(), &repo).map_err(|rollback_error| {
            ApiError::server_error(format!(
                "persist source file: {error}; rollback lane repo: {rollback_error}"
            ))
        })?;
        return Err(ApiError::server_error(format!(
            "persist source file: {error}"
        )));
    }

    *repo = draft;
    Ok(Json(snapshot(&state, &repo)?))
}

async fn discard_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    mutate_repo(&state, |repo| {
        repo.discard_lane(&lane);
        Ok(())
    })
}

async fn reset(State(state): State<AppState>) -> Result<Json<StateResponse>, ApiError> {
    let source_path = project_file_path(&state, state.file_path.as_ref())?;
    let next_repo = seed_repo(SAMPLE_BASE);

    let mut current = state.repo.lock().expect("lane repo mutex");
    persist_repo(state.storage_path.as_ref(), &next_repo)
        .map_err(|error| ApiError::server_error(format!("persist lane repo: {error}")))?;
    if let Err(error) = persist_bytes(&source_path, SAMPLE_BASE) {
        persist_repo(state.storage_path.as_ref(), &current).map_err(|rollback_error| {
            ApiError::server_error(format!(
                "persist source file: {error}; rollback lane repo: {rollback_error}"
            ))
        })?;
        return Err(ApiError::server_error(format!(
            "persist source file: {error}"
        )));
    }

    *current = next_repo;
    Ok(Json(snapshot(&state, &current)?))
}

pub fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_or_seed_repo(storage_path: &FsPath, source_path: &FsPath) -> io::Result<LaneRepo> {
    match fs::read(storage_path) {
        Ok(bytes) => LaneRepo::from_bytes(&bytes).map_err(decode_error),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let base = read_or_seed_source(source_path)?;
            let repo = seed_repo(&base);
            persist_repo(storage_path, &repo)?;
            Ok(repo)
        }
        Err(error) => Err(error),
    }
}

fn read_or_seed_source(source_path: &FsPath) -> io::Result<Vec<u8>> {
    match fs::read(source_path) {
        Ok(bytes) => Ok(SourceProjection::from_worktree(bytes).into_bytes()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            persist_bytes(source_path, SAMPLE_BASE)?;
            Ok(SAMPLE_BASE.to_vec())
        }
        Err(error) => Err(error),
    }
}

fn seed_repo(base: &[u8]) -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").expect("seed agent-a lane");
    repo.create_lane("agent-b").expect("seed agent-b lane");
    if base == SAMPLE_BASE {
        repo.write(DEMO_PATH, "agent-a", base, 21..25, "fast")
            .expect("seed agent-a overlay");
        repo.write(DEMO_PATH, "agent-b", base, 21..25, "safe")
            .expect("seed agent-b overlay");
    }
    repo
}

fn persist_repo(path: &FsPath, repo: &LaneRepo) -> io::Result<()> {
    persist_bytes(path, &repo.to_bytes())
}

fn persist_bytes(path: &FsPath, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp_path = temp_path_for(path)?;
    let result = (|| {
        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(bytes)?;
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
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
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

fn read_project_file(state: &AppState, path: &str) -> Result<Vec<u8>, ApiError> {
    read_project_source(state, path).map(SourceProjection::into_bytes)
}

fn read_project_source(state: &AppState, path: &str) -> Result<SourceProjection, ApiError> {
    let source_path = project_file_path(state, path)?;
    read_source_file(&source_path)
}

fn read_source_file(path: &FsPath) -> Result<SourceProjection, ApiError> {
    fs::read(path)
        .map(SourceProjection::from_worktree)
        .map_err(|error| ApiError::server_error(format!("read source file: {error}")))
}

fn project_file_path(state: &AppState, path: &str) -> Result<PathBuf, ApiError> {
    Ok(state.root_path.join(normalize_repo_path(path)?))
}

fn normalize_repo_path(path: &str) -> Result<String, ApiError> {
    if path.trim().is_empty() {
        return Err(ApiError::bad_request("missing path".to_owned()));
    }

    let raw_path = FsPath::new(path);
    if raw_path.is_absolute() {
        return Err(ApiError::bad_request(
            "path must be repo-relative".to_owned(),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            _ => {
                return Err(ApiError::bad_request(
                    "path must stay inside the repo".to_owned(),
                ));
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(ApiError::bad_request("missing path".to_owned()));
    }

    let label = normalized.to_string_lossy().replace('\\', "/");
    let reserved_label = label.to_ascii_lowercase();
    if reserved_label == ".lane" || reserved_label.starts_with(".lane/") {
        return Err(ApiError::bad_request(
            "cannot project lane state files".to_owned(),
        ));
    }
    Ok(label)
}

fn decode_error(error: crate::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn mutate_repo(
    state: &AppState,
    mutate: impl FnOnce(&mut LaneRepo) -> Result<(), ApiError>,
) -> Result<Json<StateResponse>, ApiError> {
    let mut repo = state.repo.lock().expect("lane repo mutex");
    let mut draft = repo.clone();
    mutate(&mut draft)?;
    persist_repo(state.storage_path.as_ref(), &draft)
        .map_err(|error| ApiError::server_error(format!("persist lane repo: {error}")))?;
    *repo = draft;
    Ok(Json(snapshot(state, &repo)?))
}

fn snapshot(state: &AppState, repo: &LaneRepo) -> Result<StateResponse, ApiError> {
    let file_path = state.file_path.as_ref().clone();
    let base_bytes = read_project_file(state, &file_path)?;
    let base = render_view("base", base_bytes.clone());
    let lanes = repo
        .lane_ids()
        .map(|lane| {
            repo.read(&file_path, lane, &base_bytes)
                .map(|bytes| render_view(lane, bytes))
                .map_err(api_error_for_lane)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(StateResponse {
        file_path,
        storage_path: state.storage_label.as_ref().clone(),
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

fn api_error_for_lane(error: LaneError) -> ApiError {
    match error {
        LaneError::ReservedLane(_)
        | LaneError::LaneMissing(_)
        | LaneError::RangeOutOfBounds { .. } => ApiError::bad_request(format!("{error:?}")),
        LaneError::BaseChanged { .. } => ApiError::conflict(format!("{error:?}")),
        LaneError::BlobMissing(_) | LaneError::ExtentOutOfBounds => {
            ApiError::server_error(format!("{error:?}"))
        }
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

    fn conflict(message: String) -> Self {
        Self {
            status: StatusCode::CONFLICT,
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
