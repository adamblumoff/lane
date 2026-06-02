use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use lane::LaneFile;
use serde::{Deserialize, Serialize};

const SAMPLE_BASE: &[u8] = b"export const mode = 'base';\n\nexport function describeLane() {\n  return `current mode: ${mode}`;\n}\n";

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/api/state", get(state))
        .route("/api/lanes/{lane}/write", post(write_range))
        .route("/api/lanes/{lane}/replace", post(replace_lane))
        .route("/api/lanes/{lane}/delete", post(delete_range))
        .route("/api/lanes/{lane}/promote", post(promote_lane))
        .route("/api/lanes/{lane}/discard", post(discard_lane))
        .route("/api/reset", post(reset))
        .with_state(AppState::new());

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
}

impl AppState {
    fn new() -> Self {
        Self {
            file: Arc::new(Mutex::new(seed_file())),
        }
    }
}

#[derive(Serialize)]
struct StateResponse {
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
    Ok(Json(snapshot(&file)?))
}

async fn write_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let mut file = state.file.lock().expect("lane file mutex");
    file.write(lane, request.start..request.end, request.replacement)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    Ok(Json(snapshot(&file)?))
}

async fn replace_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<ReplaceRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let mut file = state.file.lock().expect("lane file mutex");
    let current_len = file
        .read(&lane)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?
        .len() as u64;
    file.write(lane, 0..current_len, request.content)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    Ok(Json(snapshot(&file)?))
}

async fn delete_range(
    State(state): State<AppState>,
    Path(lane): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let mut file = state.file.lock().expect("lane file mutex");
    file.delete(lane, request.start..request.end)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    Ok(Json(snapshot(&file)?))
}

async fn promote_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let mut file = state.file.lock().expect("lane file mutex");
    file.promote(&lane)
        .map_err(|error| ApiError::bad_request(format!("{error:?}")))?;
    Ok(Json(snapshot(&file)?))
}

async fn discard_lane(
    State(state): State<AppState>,
    Path(lane): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    ensure_lane_id(&lane)?;
    let mut file = state.file.lock().expect("lane file mutex");
    file.discard(&lane);
    Ok(Json(snapshot(&file)?))
}

async fn reset(State(state): State<AppState>) -> Result<Json<StateResponse>, ApiError> {
    let mut file = state.file.lock().expect("lane file mutex");
    *file = seed_file();
    Ok(Json(snapshot(&file)?))
}

fn seed_file() -> LaneFile {
    let mut file = LaneFile::new(SAMPLE_BASE.to_vec());
    file.write("agent-a", 21..25, "fast").expect("seed agent-a");
    file.write("agent-b", 21..25, "safe").expect("seed agent-b");
    file
}

fn snapshot(file: &LaneFile) -> Result<StateResponse, ApiError> {
    let base = render_view("base", file.read_base());
    let lanes = file
        .lane_ids()
        .map(|lane| {
            file.read(lane)
                .map(|bytes| render_view(lane, bytes))
                .map_err(|error| ApiError::server_error(format!("{error:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(StateResponse { base, lanes })
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
        (self.status, Json(ErrorResponse { error: self.message })).into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}
