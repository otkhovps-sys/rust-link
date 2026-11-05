//! OneDrive backup against an in-process Microsoft Graph mock: chunked upload sessions,
//! resume after failures, integrity checks, LRU eviction and cold-storage pulls.

mod common;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    body::Bytes,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use rand::{Rng, SeedableRng};
use rust_link::{
    api::models::TrackInfo,
    cache::{cache_key_for, index::NewCacheEntry, relative_path_for, CacheIndex, CacheManager, CacheOutcome},
    config::{ByteSize, CacheConfig, OneDriveConfig, SourcesConfig},
    onedrive::{
        auth::{OneDriveCredentials, TokenManager},
        quickxor::QuickXorHash,
        sync_worker, OneDriveClient, OneDriveError, SyncWorker, UploadSession,
    },
    source::SourceManager,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-access-token";

#[derive(Default)]
struct Upload {
    path: String,
    data: Vec<u8>,
}

#[derive(Default)]
struct Mock {
    base: Mutex<String>,
    uploads: Mutex<HashMap<String, Upload>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    sessions_created: AtomicU32,
    chunk_puts: AtomicU32,
    fail_chunk: AtomicU32,
    wrong_hash: AtomicBool,
}

impl Mock {
    fn item(&self, path: &str, data: &[u8]) -> serde_json::Value {
        let mut hasher = QuickXorHash::new();
        hasher.update(data);
        let hash = if self.wrong_hash.load(Ordering::Relaxed) {
            "AAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()
        } else {
            hasher.base64()
        };
        json!({
            "id": format!("item-{}", path.len()),
            "name": path.rsplit('/').next().unwrap(),
            "size": data.len(),
            "file": {"hashes": {"quickXorHash": hash}}
        })
    }
}

type Shared = Arc<Mock>;

fn authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"code": "itemNotFound", "message": "The resource could not be found."}})),
    )
        .into_response()
}

fn item_path(rest: &str, suffix: &str) -> Option<String> {
    rest.strip_prefix("root:/")?.strip_suffix(suffix).map(str::to_string)
}

async fn graph_post(State(mock): State<Shared>, AxPath(rest): AxPath<String>, headers: HeaderMap) -> Response {
    assert!(authorized(&headers), "graph calls carry the bearer token");
    let Some(path) = item_path(&rest, ":/createUploadSession") else {
        return not_found();
    };
    let id = mock.sessions_created.fetch_add(1, Ordering::SeqCst) + 1;
    mock.uploads
        .lock()
        .unwrap()
        .insert(id.to_string(), Upload { path, data: Vec::new() });
    let base = mock.base.lock().unwrap().clone();
    Json(json!({
        "uploadUrl": format!("{base}/upload/{id}"),
        "expirationDateTime": "2099-01-01T00:00:00Z",
        "nextExpectedRanges": ["0-"]
    }))
    .into_response()
}

async fn graph_get(State(mock): State<Shared>, AxPath(rest): AxPath<String>, headers: HeaderMap) -> Response {
    assert!(authorized(&headers));
    if let Some(path) = item_path(&rest, ":/content") {
        return match mock.files.lock().unwrap().get(&path) {
            Some(data) => data.clone().into_response(),
            None => not_found(),
        };
    }
    let path = rest.strip_prefix("root:/").unwrap_or_default().to_string();
    match mock.files.lock().unwrap().get(&path) {
        Some(data) => Json(mock.item(&path, data)).into_response(),
        None => not_found(),
    }
}

async fn graph_put(
    State(mock): State<Shared>,
    AxPath(rest): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    assert!(authorized(&headers));
    let Some(path) = item_path(&rest, ":/content") else {
        return not_found();
    };
    let item = mock.item(&path, &body);
    mock.files.lock().unwrap().insert(path, body.to_vec());
    (StatusCode::CREATED, Json(item)).into_response()
}

async fn upload_put(
    State(mock): State<Shared>,
    AxPath(id): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    assert!(
        headers.get("authorization").is_none(),
        "upload URLs must not receive the bearer token"
    );
    let n = mock.chunk_puts.fetch_add(1, Ordering::SeqCst) + 1;
    if mock.fail_chunk.load(Ordering::SeqCst) == n {
        return (StatusCode::INTERNAL_SERVER_ERROR, "transient failure").into_response();
    }
    let range = headers.get("content-range").unwrap().to_str().unwrap().to_string();
    let (span, total) = range.trim_start_matches("bytes ").split_once('/').unwrap();
    let (start, end) = span.split_once('-').unwrap();
    let (start, end, total): (usize, usize, usize) =
        (start.parse().unwrap(), end.parse().unwrap(), total.parse().unwrap());
    assert_eq!(end - start + 1, body.len());
    assert!(
        body.len() == total - start || body.len().is_multiple_of(320 * 1024),
        "chunks are 320 KiB multiples"
    );

    let mut uploads = mock.uploads.lock().unwrap();
    let Some(upload) = uploads.get_mut(&id) else {
        return not_found();
    };
    if start != upload.data.len() {
        return (StatusCode::RANGE_NOT_SATISFIABLE, "unexpected range").into_response();
    }
    upload.data.extend_from_slice(&body);
    if upload.data.len() == total {
        let path = upload.path.clone();
        let data = std::mem::take(&mut upload.data);
        let item = mock.item(&path, &data);
        mock.files.lock().unwrap().insert(path, data);
        uploads.remove(&id);
        return (StatusCode::CREATED, Json(item)).into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"nextExpectedRanges": [format!("{}-", upload.data.len())]})),
    )
        .into_response()
}

async fn upload_status(State(mock): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    match mock.uploads.lock().unwrap().get(&id) {
        Some(upload) => Json(json!({"nextExpectedRanges": [format!("{}-", upload.data.len())]})).into_response(),
        None => not_found(),
    }
}

async fn start_mock() -> (Shared, String) {
    let mock = Arc::new(Mock::default());
    let app = Router::new()
        .route("/v1.0/me/drive/{*rest}", get(graph_get).post(graph_post).put(graph_put))
        .route("/upload/{id}", put(upload_put).get(upload_status))
        .with_state(Arc::clone(&mock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *mock.base.lock().unwrap() = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, base)
}

fn onedrive_config(base: &str) -> OneDriveConfig {
    OneDriveConfig {
        enabled: true,
        graph_base_url: format!("{base}/v1.0"),
        chunk_size: ByteSize(320 * 1024),
        simple_upload_max: ByteSize(64 * 1024),
        ..Default::default()
    }
}

fn client(base: &str, dir: &Path) -> Arc<OneDriveClient> {
    let creds = OneDriveCredentials {
        client_id: "client".into(),
        client_secret: None,
        tenant: "consumers".into(),
        scope: "offline_access Files.ReadWrite".into(),
        redirect_uri: None,
        refresh_token: Some("refresh".into()),
        access_token: Some(TOKEN.into()),
        expires_at: Some(i64::MAX / 2),
    };
    let tokens = TokenManager::from_credentials(creds, dir.join("onedrive.json"), base, reqwest::Client::new());
    Arc::new(OneDriveClient::new(&onedrive_config(base), tokens).unwrap())
}

fn random_file(dir: &Path, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(len as u64);
    let data: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
    let path = dir.join(name);
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

#[tokio::test]
async fn chunked_upload_resumes_after_server_error() {
    let (mock, base) = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let client = client(&base, dir.path());
    let (path, data) = random_file(dir.path(), "big.opus", 1_000_000);
    mock.fail_chunk.store(2, Ordering::SeqCst);

    let events = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&events);
    let item = client
        .upload_file(
            &path,
            "AudioCache/bi/big.opus",
            None,
            move |session: Option<UploadSession>| {
                recorder.lock().unwrap().push(session.is_some());
                async {}
            },
        )
        .await
        .unwrap();

    assert_eq!(item.size, Some(1_000_000));
    assert_eq!(mock.files.lock().unwrap()["AudioCache/bi/big.opus"], data);
    assert_eq!(mock.sessions_created.load(Ordering::SeqCst), 1);
    // 4 chunks of 320 KiB (last one partial) + 1 failed attempt.
    assert_eq!(mock.chunk_puts.load(Ordering::SeqCst), 5);
    assert_eq!(*events.lock().unwrap(), vec![true, false]);

    let mut stream = client.download_stream("AudioCache/bi/big.opus").await.unwrap();
    let mut downloaded = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        downloaded.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(downloaded, data);
}

#[tokio::test]
async fn stale_session_is_replaced_and_small_files_use_put() {
    let (mock, base) = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let client = client(&base, dir.path());
    let (path, data) = random_file(dir.path(), "a.opus", 700_000);
    let stale = UploadSession {
        upload_url: format!("{base}/upload/does-not-exist"),
        expiration_date_time: None,
        next_expected_ranges: vec![],
    };
    client
        .upload_file(&path, "AudioCache/a.opus", Some(stale), |_| async {})
        .await
        .unwrap();
    assert_eq!(mock.files.lock().unwrap()["AudioCache/a.opus"], data);
    assert_eq!(mock.sessions_created.load(Ordering::SeqCst), 1);

    let (small, small_data) = random_file(dir.path(), "s.opus", 10_000);
    client
        .upload_file(&small, "AudioCache/s.opus", None, |_| async {})
        .await
        .unwrap();
    assert_eq!(mock.files.lock().unwrap()["AudioCache/s.opus"], small_data);
    assert_eq!(
        mock.sessions_created.load(Ordering::SeqCst),
        1,
        "small files skip upload sessions"
    );
}

#[tokio::test]
async fn hash_mismatch_is_rejected() {
    let (mock, base) = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let client = client(&base, dir.path());
    mock.wrong_hash.store(true, Ordering::Relaxed);
    let (path, _) = random_file(dir.path(), "h.opus", 500_000);
    let error = client
        .upload_file(&path, "AudioCache/h.opus", None, |_| async {})
        .await
        .unwrap_err();
    assert!(matches!(error, OneDriveError::Integrity(_)), "{error}");
}

#[tokio::test]
async fn backup_eviction_and_cold_storage_pull() {
    let (mock, base) = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let client = client(&base, dir.path());
    let cache_cfg = CacheConfig {
        directory: dir.path().join("cache"),
        database: dir.path().join("cache/index.db"),
        max_size: ByteSize(100_000),
        ..Default::default()
    };
    let index = CacheIndex::open(&cache_cfg.database).await.unwrap();
    let sources = Arc::new(SourceManager::new(&SourcesConfig::default(), Some(index.clone())).unwrap());
    let cache = CacheManager::new(cache_cfg.clone(), index.clone(), sources, Some(Arc::clone(&client)));

    // A 1 s stereo WAV (~192 KB) that is "already cached".
    let info = TrackInfo {
        identifier: "https://example.invalid/cold.wav".into(),
        is_seekable: true,
        author: "tests".into(),
        length: 1000,
        is_stream: false,
        position: 0,
        title: "cold".into(),
        uri: Some("https://example.invalid/cold.wav".into()),
        artwork_url: None,
        isrc: None,
        source_name: "http".into(),
    };
    let key = cache_key_for(&info);
    let relative = relative_path_for(&key, "opus");
    let path = cache.absolute_path(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    common::write_wav(&path, 48_000, 2, 1.0);
    let original = std::fs::read(&path).unwrap();
    index
        .upsert_downloaded(&NewCacheEntry {
            video_id: key.clone(),
            info: info.clone(),
            file_path: relative.clone(),
            file_size: original.len() as i64,
            duration_ms: 1000,
            sha256: rust_link::util::sha256_hex(&original),
            container: Some("wav".into()),
        })
        .await
        .unwrap();

    // Over the high watermark but not backed up yet: nothing may be evicted.
    let report = cache.enforce_capacity().await.unwrap();
    assert_eq!(report.evicted_files, 0);
    assert_eq!(report.skipped_unsynced, 1);

    let worker = SyncWorker::new(
        Arc::clone(&cache),
        Arc::clone(&client),
        onedrive_config(&base),
        cache_cfg.database.clone(),
    );
    let sync = worker.sync_pending(&CancellationToken::new()).await.unwrap();
    assert_eq!(sync.uploaded, 1);
    let entry = index.get(&key).await.unwrap().unwrap();
    assert!(entry.onedrive_synced);
    assert_eq!(
        entry.onedrive_path.as_deref(),
        Some(format!("AudioCache/{relative}").as_str())
    );
    assert_eq!(mock.files.lock().unwrap()[&format!("AudioCache/{relative}")], original);

    // Backed up: LRU deletes the local file but keeps the row as cold.
    let report = cache.enforce_capacity().await.unwrap();
    assert_eq!(report.evicted_files, 1);
    assert!(!path.exists());
    let entry = index.get(&key).await.unwrap().unwrap();
    assert!(!entry.is_local && entry.onedrive_synced);

    // Playing it again pulls it back from OneDrive.
    let acquired = cache.acquire(&info).await.unwrap();
    assert_eq!(acquired.outcome, CacheOutcome::ColdStorage);
    if let rust_link::cache::PlayableSource::Progressive { progress } = &acquired.source {
        progress.wait_done().await.unwrap();
    }
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(index.get(&key).await.unwrap().unwrap().is_local);

    // Index snapshot backup + disaster recovery restore.
    worker.upload_snapshot().await.unwrap();
    assert!(mock
        .files
        .lock()
        .unwrap()
        .contains_key("AudioCache/_meta/cache_index.sqlite"));
    let restored_path = dir.path().join("restored/index.db");
    assert!(sync_worker::restore_index_if_missing(&client, &restored_path)
        .await
        .unwrap());
    let restored = CacheIndex::open(&restored_path).await.unwrap();
    let restored_entry = restored.get(&key).await.unwrap().unwrap();
    assert!(restored_entry.onedrive_synced);
    assert!(!sync_worker::restore_index_if_missing(&client, &restored_path)
        .await
        .unwrap());
}
