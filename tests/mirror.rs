//! Spotify / Apple Music / Deezer links against mock APIs, and playback through the
//! matched YouTube video (fake yt-dlp).
#![cfg(unix)]

mod common;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{Form, Path as UrlPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use common::{
    fake_ytdlp::{FakeYtDlp, TWO_CANDIDATES},
    voice_mock::{lavalink_ws, start_voice_server, wait_event},
    TestNode,
};
use rust_link::audio::crypto::EncryptionMode;
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Counters {
    spotify_tokens: Arc<AtomicUsize>,
    base: Arc<std::sync::Mutex<String>>,
}

const APPLE_TOKEN: &str = "apple-developer-token";
const REDIRECT: &str = "http://127.0.0.1:8888/callback";

fn spotify_track(id: &str, name: &str, isrc: &str, duration: u64) -> Value {
    json!({"id": id, "type": "track", "name": name, "duration_ms": duration,
        "external_ids": {"isrc": isrc},
        "artists": [{"name": "Test Artist", "external_urls": {"spotify": "https://open.spotify.com/artist/ar1"}}],
        "album": {"name": "Test Album", "images": [{"url": "https://i.scdn.co/album.jpg"}],
                  "external_urls": {"spotify": "https://open.spotify.com/album/al1"}},
        "external_urls": {"spotify": format!("https://open.spotify.com/track/{id}")}})
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(expected)
}

async fn start_mock_apis(counters: Counters) -> String {
    let app = Router::new()
        // Spotify
        .route(
            "/spotify/accounts/api/token",
            post(|State(c): State<Counters>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>| async move {
                let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("sp-id:sp-secret"));
                if !authorized(&headers, &basic) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
                let refused = (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "Invalid refresh token"})));
                match field("grant_type") {
                    "client_credentials" => {
                        c.spotify_tokens.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"access_token": "sp-token", "token_type": "Bearer", "expires_in": 3600})).into_response()
                    }
                    // `rust-link spotify-login`: the code from the consent page.
                    "authorization_code" if field("code") == "good-code" && field("redirect_uri") == REDIRECT => {
                        Json(json!({"access_token": "user-token", "refresh_token": "rt-1", "expires_in": 3600,
                            "scope": "playlist-read-private playlist-read-collaborative"})).into_response()
                    }
                    // Spotify may rotate the refresh token.
                    "refresh_token" if matches!(field("refresh_token"), "rt-1" | "rt-2") => {
                        Json(json!({"access_token": "user-token", "refresh_token": "rt-2", "expires_in": 3600})).into_response()
                    }
                    _ => refused.into_response(),
                }
            }),
        )
        .route(
            "/spotify/api/me",
            get(|headers: HeaderMap| async move {
                if !authorized(&headers, "Bearer user-token") {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!({"id": "otkho", "display_name": "Otkho"})).into_response()
            }),
        )
        .route(
            "/spotify/api/tracks/{id}",
            get(|UrlPath(id): UrlPath<String>, headers: HeaderMap| async move {
                if !authorized(&headers, "Bearer sp-token") && !authorized(&headers, "Bearer user-token") {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(spotify_track(&id, "Mirror Song", "USTEST000001", 2_000)).into_response()
            }),
        )
        .route(
            "/spotify/api/playlists/{id}",
            get(|State(c): State<Counters>, UrlPath(id): UrlPath<String>, headers: HeaderMap| async move {
                let base = c.base.lock().unwrap().clone();
                // Owned by the signed-in account: entries only for its token.
                if id == "MinePlaylist" {
                    if authorized(&headers, "Bearer user-token") {
                        return Json(json!({"name": "Của tôi", "owner": {"display_name": "Otkho"},
                            "items": {"total": 1, "next": null, "items": [{"track": true, "item": spotify_track("m1", "Năm", "ISRC5", 1000)}]}}));
                    }
                    return Json(json!({"name": "Của tôi", "owner": {"display_name": "Otkho"}}));
                }
                // Web API since February 2026: entries under `items`, track under `item`.
                if id == "NewFormat" {
                    return Json(json!({"name": "Mới", "owner": {"display_name": "me"},
                        "items": {"total": 2, "items": [{"track": true, "item": spotify_track("n1", "Ba", "ISRC3", 1000)}],
                                  "next": format!("{base}/spotify/api/playlists/{id}/items?offset=1")}}));
                }
                // Another user's playlist, for a Development Mode app: metadata only.
                if id == "OthersPlaylist" {
                    return Json(json!({"name": "Nhạc suy", "owner": {"display_name": "someone"},
                        "external_urls": {"spotify": "https://open.spotify.com/playlist/OthersPlaylist"}}));
                }
                Json(json!({"name": "Road Trip", "external_urls": {"spotify": "https://open.spotify.com/playlist/pl"},
                    "images": [{"url": "https://i.scdn.co/pl.jpg"}], "owner": {"display_name": "dj"},
                    "tracks": {"total": 3, "items": [
                        {"track": spotify_track("t1", "One", "ISRC1", 1000)},
                        {"track": {"id": null, "is_local": true, "type": "track", "name": "Local file"}}
                    ], "next": format!("{base}/spotify/api/playlists/{id}/tracks?offset=2")}}))
            }),
        )
        .route(
            "/spotify/api/playlists/{id}/items",
            get(|| async {
                Json(json!({"items": [{"track": true, "item": spotify_track("n2", "Bốn", "ISRC4", 1000)}], "next": null}))
            }),
        )
        .route(
            "/spotify/api/artists/{id}",
            get(|UrlPath(id): UrlPath<String>| async move {
                Json(json!({"id": id, "name": "Test Artist", "images": [{"url": "https://i.scdn.co/artist.jpg"}]}))
            }),
        )
        // Removed for Development Mode apps in 2026.
        .route(
            "/spotify/api/artists/{id}/top-tracks",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/spotify/api/playlists/{id}/tracks",
            get(|| async {
                Json(json!({"items": [{"track": spotify_track("t2", "Two", "ISRC2", 1000)}], "next": null}))
            }),
        )
        .route(
            "/spotify/api/albums/{id}",
            get(|| async {
                Json(json!({"name": "Discovery", "images": [{"url": "https://i.scdn.co/disc.jpg"}],
                    "artists": [{"name": "Daft Punk"}], "external_urls": {"spotify": "https://open.spotify.com/album/d"},
                    "tracks": {"total": 1, "next": null, "items": [
                        {"id": "s1", "type": "track", "name": "One More Time", "duration_ms": 320000,
                         "artists": [{"name": "Daft Punk"}]}]}}))
            }),
        )
        .route(
            "/spotify/api/search",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                // Development Mode apps get at most 10 results since 2026; more is a 400.
                let limit: usize = query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(5);
                if limit > 10 {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                Json(json!({"tracks": {"items": [spotify_track("q1", "Hit", "ISRCQ1", 1000),
                                                  spotify_track("q2", "Hit (Remix)", "ISRCQ2", 1000)]}}))
                .into_response()
            }),
        )
        // Apple Music
        .route(
            "/apple/api/v1/catalog/{sf}/songs/{id}",
            get(|UrlPath((sf, id)): UrlPath<(String, String)>, headers: HeaderMap| async move {
                if !authorized(&headers, &format!("Bearer {APPLE_TOKEN}")) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!({"data": [{"id": id, "type": "songs", "attributes": {
                    "name": format!("Song in {sf}"), "artistName": "Adele", "albumName": "25",
                    "durationInMillis": 295502, "isrc": "GBBKS1500214",
                    "url": "https://music.apple.com/vn/album/hello/1?i=2",
                    "artwork": {"url": "https://is1-ssl.mzstatic.com/{w}x{h}bb.jpg"}}}]}))
                .into_response()
            }),
        )
        .route(
            "/apple/api/v1/catalog/{sf}/playlists/{id}",
            get(|UrlPath((_, id)): UrlPath<(String, String)>| async move {
                Json(json!({"data": [{"id": id, "type": "playlists",
                    "attributes": {"name": "Today's Hits", "curatorName": "Apple Music", "url": "https://music.apple.com/us/playlist/x/pl.1"},
                    "relationships": {"tracks": {
                        "data": [{"id": "1", "type": "songs", "attributes": {"name": "A", "artistName": "X", "durationInMillis": 1000}}],
                        "next": format!("/v1/catalog/us/playlists/{id}/tracks?offset=1")}}}]}))
            }),
        )
        .route(
            "/apple/api/v1/catalog/{sf}/playlists/{id}/tracks",
            get(|| async {
                Json(json!({"data": [{"id": "2", "type": "songs", "attributes": {"name": "B", "artistName": "Y", "durationInMillis": 1000}},
                                     {"id": "v", "type": "music-videos", "attributes": {"name": "Video"}}]}))
            }),
        )
        // Deezer
        .route(
            "/deezer/track/{id}",
            get(|UrlPath(id): UrlPath<String>| async move {
                let (id, isrc) = match id.strip_prefix("isrc:") {
                    Some(isrc) => ("999".to_string(), isrc.to_string()),
                    None => (id, "GBDUW0000059".to_string()),
                };
                Json(json!({"id": id.parse::<u64>().unwrap(), "type": "track", "title": "Harder", "duration": 224,
                    "isrc": isrc, "link": format!("https://www.deezer.com/track/{id}"),
                    "artist": {"name": "Daft Punk"}, "album": {"id": 302127, "title": "Discovery"}}))
            }),
        )
        .route(
            "/deezer/playlist/{id}",
            get(|| async { Json(json!({"title": "Deezer Mix", "nb_tracks": 2, "creator": {"name": "me"}})) }),
        )
        .route(
            "/deezer/playlist/{id}/tracks",
            get(|| async {
                Json(json!({"data": [
                    {"id": 1, "type": "track", "title": "First", "duration": 100, "artist": {"name": "A"}},
                    {"id": 2, "type": "track", "title": "Second", "duration": 100, "artist": {"name": "B"}}]}))
            }),
        )
        .route(
            "/deezer/search",
            get(|| async {
                Json(json!({"data": [{"id": 5, "type": "track", "title": "Found", "duration": 10, "artist": {"name": "C"}}]}))
            }),
        )
        .with_state(counters.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *counters.base.lock().unwrap() = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

async fn mirror_node(base: &str, ytdlp: &str) -> TestNode {
    let base = base.to_string();
    let ytdlp = ytdlp.to_string();
    TestNode::start(move |c| {
        c.sources.ytdlp.path = ytdlp;
        c.sources.spotify.client_id = "sp-id".into();
        c.sources.spotify.client_secret = "sp-secret".into();
        c.sources.spotify.api_url = format!("{base}/spotify/api");
        c.sources.spotify.accounts_url = format!("{base}/spotify/accounts");
        // Above the 10 results Spotify allows since 2026: must be capped, not rejected.
        c.sources.spotify.search_limit = 25;
        c.sources.apple_music.api_url = format!("{base}/apple/api");
        c.sources.apple_music.media_api_token = APPLE_TOKEN.into();
        c.sources.deezer.api_url = format!("{base}/deezer");
    })
    .await
}

async fn load(node: &TestNode, identifier: &str) -> Value {
    node.get("/v4/loadtracks")
        .query(&[("identifier", identifier)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn loads_spotify_apple_music_and_deezer_links() {
    let counters = Counters::default();
    let base = start_mock_apis(counters.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 2.0);
    let node = mirror_node(&base, &ytdlp.path).await;

    let info: Value = node.get("/v4/info").send().await.unwrap().json().await.unwrap();
    for name in ["spotify", "applemusic", "deezer"] {
        assert!(
            info["sourceManagers"].as_array().unwrap().contains(&json!(name)),
            "{name}"
        );
    }

    // Spotify track: metadata, LavaSrc pluginInfo, decodable encoding.
    let track = load(
        &node,
        "https://open.spotify.com/intl-vi/track/4uLU6hMCjMI75M1A2tKUQC?si=x",
    )
    .await;
    assert_eq!(track["loadType"], "track", "{track}");
    let data = &track["data"];
    assert_eq!(data["info"]["sourceName"], "spotify");
    assert_eq!(data["info"]["isrc"], "USTEST000001");
    assert_eq!(data["info"]["author"], "Test Artist");
    assert_eq!(data["pluginInfo"]["albumName"], "Test Album");
    assert_eq!(data["pluginInfo"]["isPreview"], false);
    let decoded: Value = node
        .get("/v4/decodetrack")
        .query(&[("encodedTrack", data["encoded"].as_str().unwrap())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(decoded["info"], data["info"]);

    // Playlist pages are followed; local files are skipped.
    let playlist = load(&node, "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M").await;
    assert_eq!(playlist["loadType"], "playlist", "{playlist}");
    assert_eq!(playlist["data"]["info"]["name"], "Road Trip");
    assert_eq!(playlist["data"]["pluginInfo"]["type"], "playlist");
    assert_eq!(playlist["data"]["pluginInfo"]["author"], "dj");
    let titles: Vec<&str> = playlist["data"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["info"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["One", "Two"]);

    // The February 2026 format (`items` / `item`), pages included.
    let fresh = load(&node, "https://open.spotify.com/playlist/NewFormat?si=x").await;
    assert_eq!(fresh["loadType"], "playlist", "{fresh}");
    let titles: Vec<&str> = fresh["data"]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["info"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Ba", "Bốn"]);

    // Playlists Spotify returns without entries are explained instead of looking empty.
    let hidden = load(&node, "https://open.spotify.com/playlist/OthersPlaylist").await;
    assert_eq!(hidden["loadType"], "error", "{hidden}");
    let message = hidden["data"]["message"].as_str().unwrap();
    assert!(
        message.contains("Nhạc suy") && message.contains("Development Mode"),
        "{message}"
    );

    // Artist links without the removed top-tracks endpoint: search results instead.
    let artist = load(&node, "https://open.spotify.com/artist/0gxyHStUsqpMadRV0Di1Qt").await;
    assert_eq!(artist["loadType"], "playlist", "{artist}");
    assert_eq!(artist["data"]["info"]["name"], "Test Artist's Top Tracks");
    assert_eq!(artist["data"]["tracks"].as_array().unwrap().len(), 2);

    // Simplified album tracks take the album artwork.
    let album = load(&node, "https://open.spotify.com/album/6N9PS4QXF1D0OWPk0Sxtb4").await;
    assert_eq!(
        album["data"]["tracks"][0]["info"]["artworkUrl"],
        "https://i.scdn.co/disc.jpg"
    );
    assert_eq!(album["data"]["pluginInfo"]["author"], "Daft Punk");

    let search = load(&node, "spsearch:hit").await;
    assert_eq!(search["loadType"], "search");
    assert_eq!(search["data"].as_array().unwrap().len(), 2);
    assert_eq!(counters.spotify_tokens.load(Ordering::SeqCst), 1, "token reused");

    // Apple Music: song selected through ?i=, developer token sent as bearer.
    let song = load(&node, "https://music.apple.com/vn/album/hello/1544494392?i=1544494393").await;
    assert_eq!(song["loadType"], "track", "{song}");
    assert_eq!(song["data"]["info"]["title"], "Song in vn");
    assert_eq!(song["data"]["info"]["sourceName"], "applemusic");
    assert_eq!(
        song["data"]["info"]["artworkUrl"],
        "https://is1-ssl.mzstatic.com/600x600bb.jpg"
    );
    let apple_playlist = load(
        &node,
        "https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb",
    )
    .await;
    assert_eq!(apple_playlist["data"]["pluginInfo"]["author"], "Apple Music");
    assert_eq!(
        apple_playlist["data"]["tracks"].as_array().unwrap().len(),
        2,
        "music videos skipped"
    );

    // Deezer.
    let deezer = load(&node, "https://www.deezer.com/fr/track/3135556").await;
    assert_eq!(deezer["data"]["info"]["identifier"], "3135556");
    assert_eq!(deezer["data"]["info"]["length"], 224_000);
    assert_eq!(
        deezer["data"]["pluginInfo"]["albumUrl"],
        "https://www.deezer.com/album/302127"
    );
    let mix = load(&node, "https://www.deezer.com/playlist/908622995").await;
    assert_eq!(mix["data"]["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(load(&node, "dzsearch:found").await["data"][0]["info"]["title"], "Found");
    assert_eq!(
        load(&node, "dzisrc:USUM71703861").await["data"]["info"]["isrc"],
        "USUM71703861"
    );

    // Without credentials the error says what is missing.
    let unconfigured = TestNode::start(|_| {}).await;
    let error = load(&unconfigured, "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").await;
    assert_eq!(error["loadType"], "error");
    assert!(
        error["data"]["message"].as_str().unwrap().contains("clientId"),
        "{error}"
    );
    let error = load(
        &unconfigured,
        "https://music.apple.com/us/album/hello/1544494392?i=1544494393",
    )
    .await;
    assert_eq!(error["loadType"], "error");
    assert!(
        error["data"]["message"].as_str().unwrap().contains("mediaApiToken"),
        "{error}"
    );
}

#[tokio::test]
async fn plays_mirrored_track_through_matched_youtube_video() {
    let base = start_mock_apis(Counters::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 2.0);
    let node = mirror_node(&base, &ytdlp.path).await;
    let (endpoint, _voice, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
    let (mut events, session_id) = lavalink_ws(&node).await;

    let track = load(&node, "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").await;
    let encoded = track["data"]["encoded"].as_str().unwrap().to_string();
    for round in 0..2 {
        let response = node
            .patch(&format!("/v4/sessions/{session_id}/players/5"))
            .json(&json!({
                "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "1"},
                "track": {"encoded": encoded}
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let start = wait_event(&mut events, "TrackStartEvent").await;
        assert_eq!(
            start["track"]["info"]["sourceName"], "spotify",
            "clients keep seeing the Spotify track"
        );
        let end = wait_event(&mut events, "TrackEndEvent").await;
        assert_eq!(end["reason"], "finished", "round {round}");
        let mut audio = 0;
        while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(400), frames.recv()).await {
            if frame.as_slice() != [0xF8, 0xFF, 0xFE] {
                audio += 1;
            }
        }
        assert!(
            (95..=105).contains(&audio),
            "2 s of audio, got {audio} frames (round {round})"
        );
    }

    let searches = ytdlp.searches();
    assert_eq!(searches.len(), 1, "the match is remembered: {:?}", ytdlp.calls());
    assert!(
        searches[0].contains("ytsearch5:\"USTEST000001\""),
        "ISRC searched first: {}",
        searches[0]
    );
    let downloads = ytdlp.downloads();
    assert_eq!(downloads.len(), 1, "second play is a cache hit: {:?}", ytdlp.calls());
    assert!(
        downloads[0].contains("watch?v=bbbbbbbbbbb"),
        "duration-matched candidate: {}",
        downloads[0]
    );

    let mirror = node.ctx.cache.index().get_mirror("isrc", "USTEST000001").await.unwrap();
    assert_eq!(mirror.map(|m| m.0).as_deref(), Some("bbbbbbbbbbb"));
}

async fn load_message(node: &TestNode, identifier: &str) -> String {
    let result = load(node, identifier).await;
    assert_eq!(result["loadType"], "error", "{result}");
    result["data"]["message"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn signed_in_account_reads_its_own_playlists() {
    let base = start_mock_apis(Counters::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 1.0);
    let node = mirror_node(&base, &ytdlp.path).await;
    let cfg = node.ctx.config.sources.spotify.clone();

    // Without a signed-in account Spotify withholds the entries; the error says what to do.
    let message = load_message(&node, "https://open.spotify.com/playlist/MinePlaylist").await;
    assert!(message.contains("rust-link spotify-login"), "{message}");

    // `spotify-login` after the consent page: tokens, profile, file with owner-only access.
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let path = cfg.user_credentials_file.clone();
    let login = rust_link::source::mirror::spotify_login::complete_login(&http, &cfg, "good-code", REDIRECT, &path)
        .await
        .unwrap();
    assert_eq!((login.label(), login.refresh_token.as_str()), ("Otkho", "rt-1"));
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(
        rust_link::source::mirror::spotify_login::complete_login(&http, &cfg, "bad-code", REDIRECT, &path)
            .await
            .is_err()
    );

    // The running node picks the account up on its next request, no restart.
    let mine = load(&node, "https://open.spotify.com/playlist/MinePlaylist").await;
    assert_eq!(mine["loadType"], "playlist", "{mine}");
    assert_eq!(mine["data"]["tracks"][0]["info"]["title"], "Năm");
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["refreshToken"], "rt-2", "rotated refresh token is kept");

    // Other people's playlists stay closed, and the error names the account.
    let message = load_message(&node, "https://open.spotify.com/playlist/OthersPlaylist").await;
    assert!(message.contains("(Otkho)"), "{message}");
}

#[tokio::test]
async fn revoked_sign_in_falls_back_to_the_app() {
    let base = start_mock_apis(Counters::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 1.0);
    let node = mirror_node(&base, &ytdlp.path).await;
    std::fs::write(
        &node.ctx.config.sources.spotify.user_credentials_file,
        r#"{"refreshToken": "revoked", "displayName": "Old"}"#,
    )
    .unwrap();

    let search = load(&node, "spsearch:hit").await;
    assert_eq!(search["loadType"], "search", "the app token still works: {search}");
    let message = load_message(&node, "https://open.spotify.com/playlist/MinePlaylist").await;
    assert!(message.contains("rust-link spotify-login"), "{message}");
}
