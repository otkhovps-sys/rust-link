//! Static HTTP server for streaming tests: `Range` support, a request log and an optional
//! sliding-window live HLS playlist built from `seg<N>.ts` files.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::Response,
    Router,
};

#[derive(Debug, Clone)]
pub struct LoggedRequest {
    pub path: String,
    pub range: Option<String>,
}

#[derive(Clone)]
struct ServerState {
    root: PathBuf,
    requests: Arc<Mutex<Vec<LoggedRequest>>>,
    started: Instant,
    live_segments: usize,
}

pub struct MediaServer {
    pub base: String,
    pub requests: Arc<Mutex<Vec<LoggedRequest>>>,
}

impl MediaServer {
    pub fn ranges_for(&self, path: &str) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .filter_map(|r| r.range.clone())
            .collect()
    }
}

/// `/live.m3u8` exposes 3 one-second segments, advancing one segment per second.
fn live_playlist(state: &ServerState) -> String {
    let elapsed = state.started.elapsed().as_secs() as usize;
    let first = elapsed.min(state.live_segments.saturating_sub(3));
    let mut text = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for n in first..(first + 3).min(state.live_segments) {
        text.push_str(&format!("#EXTINF:1.000,\nseg{n}.ts\n"));
    }
    text
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "m3u8" => "application/vnd.apple.mpegurl",
        "ts" => "video/mp2t",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

async fn handle(State(state): State<ServerState>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/').to_string();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    state.requests.lock().unwrap().push(LoggedRequest {
        path: path.clone(),
        range: range.clone(),
    });
    let body = if path == "live.m3u8" && state.live_segments > 0 {
        live_playlist(&state).into_bytes()
    } else {
        match std::fs::read(state.root.join(&path)) {
            Ok(body) => body,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap()
            }
        }
    };
    let builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type(&path))
        .header(header::ACCEPT_RANGES, "bytes");
    let total = body.len();
    if let (Some(range), true) = (range, method == Method::GET) {
        let spec = range.trim_start_matches("bytes=");
        let (start, end) = spec.split_once('-').unwrap();
        let start: usize = start.parse().unwrap();
        let end: usize = end.parse::<usize>().map(|e| e.min(total - 1)).unwrap_or(total - 1);
        if start >= total {
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .unwrap();
        }
        return builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
            .body(Body::from(body[start..=end].to_vec()))
            .unwrap();
    }
    builder.status(StatusCode::OK).body(Body::from(body)).unwrap()
}

pub async fn serve(root: PathBuf, live_segments: usize) -> MediaServer {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = ServerState {
        root,
        requests: Arc::clone(&requests),
        started: Instant::now(),
        live_segments,
    };
    let app = Router::new().fallback(handle).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MediaServer {
        base: format!("http://{addr}"),
        requests,
    }
}
