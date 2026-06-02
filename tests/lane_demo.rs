use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, Response, StatusCode, header::CONTENT_TYPE};
use lane::demo::{AppState, DEMO_PATH, SAMPLE_BASE, STORAGE_PATH, router};
use serde_json::Value;
use tower::ServiceExt;

static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn seeds_demo_source_and_repo_store() {
    let root = temp_root();
    let state = AppState::new(root.clone()).unwrap();

    assert_eq!(fs::read(root.join(DEMO_PATH)).unwrap(), SAMPLE_BASE);
    assert!(root.join(STORAGE_PATH).exists());

    let response = get_json(router(state), "/api/state").await;
    assert_eq!(response["file_path"], DEMO_PATH);
    assert_eq!(response["storage_path"], STORAGE_PATH);
    assert_eq!(mode(&response["base"]), "base");
    assert_eq!(mode(&response["lanes"][0]), "fast");
    assert_eq!(mode(&response["lanes"][1]), "safe");

    cleanup(root);
}

#[tokio::test]
async fn seeding_preserves_existing_normal_source() {
    let root = temp_root();
    let existing = b"export const existing = true;\n";
    write_file(&root.join(DEMO_PATH), existing);

    let response = get_json(router(AppState::new(root.clone()).unwrap()), "/api/state").await;

    assert_eq!(fs::read(root.join(DEMO_PATH)).unwrap(), existing);
    assert_eq!(content(&response["base"]).as_bytes(), existing);
    assert_eq!(content(&response["lanes"][0]).as_bytes(), existing);
    assert_eq!(content(&response["lanes"][1]).as_bytes(), existing);

    cleanup(root);
}

#[tokio::test]
async fn crlf_checkout_projects_lanes_and_read_materializes_newlines() {
    let root = temp_root();
    let state = AppState::new(root.clone()).unwrap();
    write_file(&root.join(DEMO_PATH), &crlf(SAMPLE_BASE));
    let app = router(state);

    let state_response = get_json(app.clone(), "/api/state").await;
    assert_eq!(content(&state_response["base"]).as_bytes(), SAMPLE_BASE);
    assert_eq!(mode(&state_response["lanes"][0]), "fast");
    assert_eq!(mode(&state_response["lanes"][1]), "safe");

    let read_response = get_response(app, "/api/read?path=demo%2Fexample.ts&lane=agent-a").await;
    assert_eq!(read_response.status(), StatusCode::OK);
    assert_eq!(
        response_bytes(read_response).await,
        crlf(&sample_content("fast"))
    );

    cleanup(root);
}

#[tokio::test]
async fn promote_preserves_observed_crlf_source_style() {
    let root = temp_root();
    let state = AppState::new(root.clone()).unwrap();
    let source_path = root.join(DEMO_PATH);
    write_file(&source_path, &crlf(SAMPLE_BASE));

    let response = post_json(router(state), "/api/lanes/agent-a/promote", "{}").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fs::read(source_path).unwrap(),
        crlf(&sample_content("fast"))
    );

    cleanup(root);
}

#[tokio::test]
async fn failed_source_promote_write_rolls_back_lane_repo() {
    let root = temp_root();
    let state = AppState::new(root.clone()).unwrap();
    let app = router(state);
    let source_path = root.join(DEMO_PATH);
    let repo_path = root.join(STORAGE_PATH);
    let source_before = fs::read(&source_path).unwrap();
    let repo_before = fs::read(&repo_path).unwrap();
    fs::create_dir(temp_write_path_for(&source_path)).unwrap();

    let response = post_json(app.clone(), "/api/lanes/agent-a/promote", "{}").await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fs::read(&source_path).unwrap(), source_before);
    assert_eq!(fs::read(&repo_path).unwrap(), repo_before);
    let state_response = get_json(app, "/api/state").await;
    assert_eq!(mode(&state_response["base"]), "base");

    cleanup(root);
}

#[tokio::test]
async fn failed_source_reset_write_rolls_back_lane_repo() {
    let root = temp_root();
    let state = AppState::new(root.clone()).unwrap();
    let app = router(state);
    assert_eq!(
        post_json(app.clone(), "/api/lanes/agent-a/promote", "{}")
            .await
            .status(),
        StatusCode::OK
    );

    let source_path = root.join(DEMO_PATH);
    let repo_path = root.join(STORAGE_PATH);
    let source_before = fs::read(&source_path).unwrap();
    let repo_before = fs::read(&repo_path).unwrap();
    fs::create_dir(temp_write_path_for(&source_path)).unwrap();

    let response = post_json(app.clone(), "/api/reset", "{}").await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fs::read(&source_path).unwrap(), source_before);
    assert_eq!(fs::read(&repo_path).unwrap(), repo_before);
    let state_response = get_json(app, "/api/state").await;
    assert_eq!(mode(&state_response["base"]), "fast");

    cleanup(root);
}

#[tokio::test]
async fn read_paths_must_be_normal_repo_files() {
    let root = temp_root();
    let app = router(AppState::new(root.clone()).unwrap());

    assert_eq!(
        get_response(app.clone(), "/api/read?path=.LANE%2Frepo.lane&lane=base")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get_response(app, "/api/read?path=..%2Fdemo%2Fexample.ts&lane=base")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    cleanup(root);
}

async fn get_json(app: Router, uri: &str) -> Value {
    let response = get_response(app, uri).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

async fn get_response(app: Router, uri: &str) -> Response<Body> {
    app.oneshot(
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn post_json(app: Router, uri: &str, body: &str) -> Response<Body> {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn response_bytes(response: Response<Body>) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn mode(view: &Value) -> &str {
    content(view)
        .split("mode = '")
        .nth(1)
        .and_then(|suffix| suffix.split('\'').next())
        .unwrap()
}

fn content(view: &Value) -> &str {
    view["content"].as_str().unwrap()
}

fn sample_content(mode: &str) -> Vec<u8> {
    format!(
        "export const mode = '{mode}';\n\nexport function describeLane() {{\n  return `current mode: ${{mode}}`;\n}}\n"
    )
    .into_bytes()
}

fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut converted = Vec::new();
    for byte in bytes {
        if *byte == b'\n' {
            converted.extend_from_slice(b"\r\n");
        } else {
            converted.push(*byte);
        }
    }
    converted
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn temp_write_path_for(path: &Path) -> PathBuf {
    let mut temp_name = path.file_name().unwrap().to_os_string();
    temp_name.push(".tmp");
    path.with_file_name(temp_name)
}

fn temp_root() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("lane-{}-{timestamp}-{unique}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn cleanup(root: PathBuf) {
    fs::remove_dir_all(root).unwrap();
}
