//! Plugin processes: track loading, playback targets, events, REST routes, the REST
//! bridge and restarts, with one plugin per SDK (Python and Node.js).

mod common;

use std::{path::Path, time::Duration};

use common::{
    voice_mock::{lavalink_ws, start_voice_server, wait_event},
    write_wav, TestNode, PASSWORD,
};
use rust_link::{
    api::{
        models::{LoadResult, TrackInfo},
        track_codec,
    },
    audio::crypto::EncryptionMode,
    cache::{cache_key_for, CacheOutcome, PlayableSource},
    config::PluginEntry,
};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const PYTHON_PLUGIN: &str = r#"
import os, threading, time
from rustlink_plugin import Plugin, track

plugin = Plugin(version="1.2.3", source_name="testsrc")
events = []

@plugin.search("test")
def search(query):
    if query == "wav":
        return track(identifier="wav-1", title="Plugin WAV", author="tester", length=2000)
    if query == "delegate":
        return track(identifier="delegate-1", title="Delegated", length=2000)
    if query == "list":
        return [track(identifier="a"), track(identifier="b", plugin_info={"rank": 2})]
    if query == "boom":
        raise RuntimeError("boom")
    return None

@plugin.url(r"^https://plugin\.test/")
def by_url(url):
    return track(identifier=url, uri=url, title="from url", length=1000)

@plugin.resolver
def resolve(info):
    if info["identifier"] == "delegate-1":
        return {"identifier": "test:wav"}
    return {"file": plugin.config["wav"]}

@plugin.on("ready", "playerUpdate", "TrackStartEvent")
def remember(message, session_id):
    events.append({"op": message["op"], "sessionId": session_id, "guildId": message.get("guildId")})

@plugin.route("GET", "/v4/test/echo/{word}")
def echo(request):
    return {"word": request.params["word"], "query": request.query}

@plugin.route("GET", "/v4/test/events")
def list_events(request):
    return events

@plugin.route("GET", "/v4/test/info")
def info(request):
    status, body = plugin.rest("GET", "/v4/info")
    return {"status": status, "plugins": [p["name"] for p in body["plugins"]]}

@plugin.route("POST", "/v4/test/text")
def text(request):
    return (201, "got {}".format(request.body["n"]))

@plugin.route("GET", "/v4/test/crash")
def crash(request):
    threading.Thread(target=lambda: (time.sleep(0.2), os._exit(3))).start()
    return {"crashing": True}

plugin.run()
"#;

const NODE_PLUGIN: &str = r#"
const { Plugin, track } = require('./rustlink-plugin');
const plugin = new Plugin({ version: '0.9.0', sourceName: 'ncsrc', cache: false });
plugin.search('nc', async (query) => track({ identifier: `nc-${query}`, title: `NC ${query}`, length: 2000 }));
plugin.resolver(async () => ({ file: plugin.config.wav }));
plugin.route('GET', '/v4/nc/{*rest}', async (request) => ({ rest: request.params.rest }));
plugin.run();
"#;

fn available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn repo_file(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

async fn start_node() -> Option<(TestNode, String, tempfile::TempDir)> {
    if !available("python3") || !available("node") {
        eprintln!("python3 or node missing, plugin tests skipped");
        return None;
    }
    let work = tempfile::tempdir().unwrap();
    let wav = work.path().join("tone.wav");
    write_wav(&wav, 48_000, 2, 2.0);
    let wav = wav.to_string_lossy().into_owned();

    let plugins = work.path().join("plugins");
    let tester = plugins.join("tester");
    std::fs::create_dir_all(&tester).unwrap();
    std::fs::write(tester.join("plugin.yml"), "command: python3 main.py\n").unwrap();
    std::fs::write(tester.join("main.py"), PYTHON_PLUGIN).unwrap();
    std::fs::write(
        tester.join("rustlink_plugin.py"),
        repo_file("sdk/python/rustlink_plugin.py"),
    )
    .unwrap();
    let disabled = plugins.join("disabled");
    std::fs::create_dir_all(&disabled).unwrap();
    std::fs::write(
        disabled.join("plugin.yml"),
        "command: [sh, -c, 'exit 1']\nenabled: false\n",
    )
    .unwrap();
    let broken = plugins.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("plugin.yml"), "command: {not: [a list\n").unwrap();

    let nocache = work.path().join("nocache");
    std::fs::create_dir_all(&nocache).unwrap();
    std::fs::write(nocache.join("main.js"), NODE_PLUGIN).unwrap();
    std::fs::write(
        nocache.join("rustlink-plugin.js"),
        repo_file("sdk/node/rustlink-plugin.js"),
    )
    .unwrap();

    let wav_for_config = wav.clone();
    let node = TestNode::start(move |config| {
        config.plugins.directory = plugins;
        config
            .plugins
            .settings
            .insert("tester".into(), json!({"wav": wav_for_config}));
        config.plugins.entries.push(PluginEntry {
            name: "nocache".into(),
            command: vec!["node".into(), "main.js".into()],
            working_dir: Some(nocache),
            config: Some(json!({"wav": wav_for_config})),
            ..Default::default()
        });
    })
    .await;
    Some((node, wav, work))
}

async fn load(node: &TestNode, identifier: &str) -> LoadResult {
    node.get("/v4/loadtracks")
        .query(&[("identifier", identifier)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn load_track(node: &TestNode, identifier: &str) -> TrackInfo {
    match load(node, identifier).await {
        LoadResult::Track(track) => {
            assert_eq!(track_codec::decode_track(&track.encoded).unwrap(), track.info);
            track.info
        }
        other => panic!("{identifier}: expected a track, got {other:?}"),
    }
}

async fn get_json(node: &TestNode, path: &str) -> (u16, Value) {
    let response = node.get(path).send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugins_load_play_route_and_restart() {
    let Some((node, wav, _work)) = start_node().await else {
        return;
    };

    // Node info lists both plugins and their sources; the disabled one is not started.
    let (_, info) = get_json(&node, "/v4/info").await;
    let names: Vec<&str> = info["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"tester") && names.contains(&"nocache"), "{names:?}");
    assert!(!names.contains(&"disabled"));
    let tester = info["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "tester")
        .unwrap();
    assert_eq!(tester["version"], "1.2.3");
    let sources: Vec<&str> = info["sourceManagers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        sources.contains(&"testsrc") && sources.contains(&"ncsrc"),
        "{sources:?}"
    );

    // Track loading by prefix (case-insensitive) and by URL pattern.
    let wav_track = load_track(&node, "TEST:wav").await;
    assert_eq!(
        (wav_track.source_name.as_str(), wav_track.identifier.as_str()),
        ("testsrc", "wav-1")
    );
    assert_eq!(wav_track.author, "tester");
    let by_url = load_track(&node, "https://plugin.test/song").await;
    assert_eq!(by_url.title, "from url");
    match load(&node, "test:list").await {
        LoadResult::Search(tracks) => {
            assert_eq!(tracks.len(), 2);
            assert_eq!(tracks[1].plugin_info["rank"], 2);
        }
        other => panic!("expected search results, got {other:?}"),
    }
    match load(&node, "test:boom").await {
        LoadResult::Error(e) => assert!(e.message.unwrap().contains("boom")),
        other => panic!("expected an error, got {other:?}"),
    }
    // `None` from the plugin: the built-in sources take over (nothing matches here).
    assert!(matches!(load(&node, "test:unknown").await, LoadResult::Empty(_)));

    // Playback: the plugin names a local file, which is copied into the cache once.
    let acquired = node.ctx.cache.acquire(&wav_track).await.unwrap();
    assert_eq!(acquired.outcome, CacheOutcome::Miss);
    if let PlayableSource::Progressive { progress } = &acquired.source {
        progress.wait_done().await.unwrap();
    }
    drop(acquired);
    let entry = node
        .ctx
        .cache
        .index()
        .get(&cache_key_for(&wav_track))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.file_size as u64, std::fs::metadata(&wav).unwrap().len());
    assert_eq!(
        node.ctx.cache.acquire(&wav_track).await.unwrap().outcome,
        CacheOutcome::Hit
    );

    // A plugin track handed over to another identifier.
    let delegated = load_track(&node, "test:delegate").await;
    let acquired = node.ctx.cache.acquire(&delegated).await.unwrap();
    if let PlayableSource::Progressive { progress } = &acquired.source {
        progress.wait_done().await.unwrap();
    }

    // `cache: false` (Node.js plugin): played from the file itself.
    let nc = load_track(&node, "nc:abc").await;
    assert_eq!((nc.source_name.as_str(), nc.identifier.as_str()), ("ncsrc", "nc-abc"));
    let acquired = node.ctx.cache.acquire(&nc).await.unwrap();
    assert_eq!(acquired.outcome, CacheOutcome::Bypass);
    match &acquired.source {
        PlayableSource::File { path, container } => {
            assert_eq!(path.to_string_lossy(), wav);
            assert_eq!(container.as_deref(), Some("wav"));
        }
        _ => panic!("expected the plugin's file"),
    }

    // REST routes with path parameters, query, text bodies and status codes.
    let (status, echo) = get_json(&node, "/v4/test/echo/hello%20world?x=1").await;
    assert_eq!(status, 200);
    assert_eq!(echo, json!({"word": "hello world", "query": {"x": "1"}}));
    let (_, rest) = get_json(&node, "/v4/nc/a/b/c.mp3").await;
    assert_eq!(rest["rest"], "a/b/c.mp3");
    let response = node
        .http
        .post(node.url("/v4/test/text"))
        .header("Authorization", PASSWORD)
        .json(&json!({"n": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(response.text().await.unwrap(), "got 5");
    assert_eq!(get_json(&node, "/v4/test/missing").await.0, 404);
    let unauthorized = node.http.get(node.url("/v4/test/echo/x")).send().await.unwrap();
    assert_eq!(unauthorized.status().as_u16(), 401, "plugin routes need the password");

    // Plugins calling the node's REST API.
    let (_, bridged) = get_json(&node, "/v4/test/info").await;
    assert_eq!(bridged["status"], 200, "{bridged}");
    assert!(bridged["plugins"].as_array().unwrap().iter().any(|p| p == "nocache"));

    // Websocket messages reach subscribed plugins.
    let mut request = format!("ws://{}/v4/websocket", node.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("Authorization", PASSWORD.parse().unwrap());
    request.headers_mut().insert("User-Id", "42".parse().unwrap());
    let (_ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut seen = Value::Null;
    for _ in 0..50 {
        seen = get_json(&node, "/v4/test/events").await.1;
        if seen.as_array().is_some_and(|e| !e.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(seen[0]["op"], "ready");
    let session_id = seen[0]["sessionId"].as_str().unwrap().to_string();
    let created = node
        .patch(&format!("/v4/sessions/{session_id}/players/77"))
        .json(&json!({"volume": 50}))
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    let mut updates = 0;
    for _ in 0..60 {
        seen = get_json(&node, "/v4/test/events").await.1;
        updates = seen
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["op"] == "playerUpdate" && e["guildId"] == "77" && e["sessionId"] == session_id.as_str())
            .count();
        if updates > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(updates > 0, "playerUpdate was not forwarded: {seen}");

    // A crashed plugin is restarted and serves again.
    let (_, crash) = get_json(&node, "/v4/test/crash").await;
    assert_eq!(crash["crashing"], true);
    let mut status = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        status = get_json(&node, "/v4/rustlink/plugins").await.1;
        let tester = status
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "tester")
            .unwrap();
        if tester["restarts"] == 1 && tester["state"] == "running" {
            break;
        }
    }
    let tester = status
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "tester")
        .unwrap()
        .clone();
    assert_eq!(
        (tester["state"].as_str(), tester["restarts"].as_u64()),
        (Some("running"), Some(1)),
        "{status}"
    );
    assert!(tester["lastError"].as_str().unwrap().contains("exited"));
    assert_eq!(get_json(&node, "/v4/test/echo/again").await.1["word"], "again");
}

#[tokio::test]
async fn scaffolded_plugins_start() {
    if !available("python3") || !available("node") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let plugins = work.path().join("plugins");
    rust_link::plugin::scaffold(&plugins, "pyplug", rust_link::plugin::Language::Python).unwrap();
    rust_link::plugin::scaffold(&plugins, "jsplug", rust_link::plugin::Language::Node).unwrap();
    assert!(rust_link::plugin::scaffold(&plugins, "pyplug", rust_link::plugin::Language::Python).is_err());
    assert!(rust_link::plugin::scaffold(&plugins, "Bad Name", rust_link::plugin::Language::Python).is_err());
    let dir = plugins.clone();
    let node = TestNode::start(move |config| config.plugins.directory = dir).await;
    for name in ["pyplug", "jsplug"] {
        let (status, body) = get_json(&node, &format!("/v4/{name}/hello?q=1")).await;
        assert_eq!(status, 200, "{name}: {body}");
        assert_eq!(body, json!({"plugin": name, "query": {"q": "1"}}));
        let track = load_track(&node, &format!("{name}:https://cdn.example/a%20b.mp3")).await;
        assert_eq!(track.source_name, name);
        assert_eq!(track.uri.as_deref(), Some("https://cdn.example/a%20b.mp3"));
    }
    drop(work);
}

/// Copy a directory tree (examples into a temporary plugin folder).
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn example_plugins_ship_the_current_sdk() {
    assert_eq!(
        repo_file("examples/plugins/radio/rustlink_plugin.py"),
        repo_file("sdk/python/rustlink_plugin.py"),
        "copy sdk/python/rustlink_plugin.py into examples/plugins/radio"
    );
    assert_eq!(
        repo_file("examples/plugins/local-files/rustlink-plugin.js"),
        repo_file("sdk/node/rustlink-plugin.js"),
        "copy sdk/node/rustlink-plugin.js into examples/plugins/local-files"
    );
}

#[tokio::test]
async fn example_plugins_work() {
    if !available("python3") || !available("node") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let plugins = work.path().join("plugins");
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");
    copy_dir(&examples.join("radio"), &plugins.join("radio"));
    copy_dir(&examples.join("local-files"), &plugins.join("local-files"));
    let music = work.path().join("music/Some Artist");
    std::fs::create_dir_all(&music).unwrap();
    write_wav(&music.join("Quiet Tone.wav"), 48_000, 2, 1.0);
    let folder = work.path().join("music");

    let node = TestNode::start(move |config| {
        config.plugins.directory = plugins;
        config.plugins.settings.insert(
            "radio".into(),
            json!({"stations": {
                "lofi": {"title": "Lofi radio", "url": "https://radio.example/lofi.mp3"},
                "jazz": {"title": "Smooth jazz", "url": "https://radio.example/jazz.m3u8"}
            }}),
        );
        config
            .plugins
            .settings
            .insert("local-files".into(), json!({"folder": folder}));
    })
    .await;

    let lofi = load_track(&node, "radio:lofi").await;
    assert!(lofi.is_stream && lofi.source_name == "radio");
    assert_eq!(lofi.uri.as_deref(), Some("https://radio.example/lofi.mp3"));
    match load(&node, "radio:").await {
        LoadResult::Playlist(p) => assert_eq!(p.tracks.len(), 2),
        other => panic!("expected every station, got {other:?}"),
    }
    match load(&node, "radio:smooth").await {
        LoadResult::Search(tracks) => assert_eq!(tracks[0].info.identifier, "jazz"),
        other => panic!("expected a search result, got {other:?}"),
    }
    assert!(matches!(load(&node, "radio:nothing").await, LoadResult::Empty(_)));
    let (_, stations) = get_json(&node, "/v4/radio/stations").await;
    assert_eq!(stations.as_array().unwrap().len(), 2);

    let file = match load(&node, "local:quiet artist").await {
        LoadResult::Search(mut tracks) => tracks.remove(0).info,
        other => panic!("expected the local file, got {other:?}"),
    };
    assert_eq!(
        (file.title.as_str(), file.author.as_str()),
        ("Quiet Tone", "Some Artist")
    );
    let acquired = node.ctx.cache.acquire(&file).await.unwrap();
    assert_eq!(acquired.outcome, CacheOutcome::Bypass);
    assert!(
        matches!(&acquired.source, PlayableSource::File { path, .. } if path.ends_with("Some Artist/Quiet Tone.wav"))
    );
    let (_, listed) = get_json(&node, "/v4/local/files").await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    drop(work);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_tracks_play_through_voice() {
    let Some((node, _wav, _work)) = start_node().await else {
        return;
    };
    let (mut events, session_id) = lavalink_ws(&node).await;

    // Cached through the Python plugin, then straight from the file through the Node one.
    for (guild, identifier) in [("901", "test:wav"), ("902", "nc:voice")] {
        let (endpoint, _log, mut frames) = start_voice_server(EncryptionMode::AeadAes256GcmRtpSize).await;
        let player: Value = node
            .patch(&format!("/v4/sessions/{session_id}/players/{guild}"))
            .json(&json!({
                "voice": {"token": "t", "endpoint": endpoint, "sessionId": "s", "channelId": "5"},
                "track": {"identifier": identifier},
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(player["track"]["info"]["sourceName"].as_str().is_some(), "{player}");
        assert_eq!(wait_event(&mut events, "TrackStartEvent").await["guildId"], guild);
        let end = wait_event(&mut events, "TrackEndEvent").await;
        assert_eq!(end["reason"], "finished", "{identifier}: {end}");

        let mut audio = 0;
        while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
            audio += usize::from(frame.as_slice() != [0xF8, 0xFF, 0xFE]);
        }
        // 2 s of audio: 100 frames of 20 ms.
        assert!((98..=102).contains(&audio), "{identifier}: got {audio} audio frames");
    }

    let seen = get_json(&node, "/v4/test/events").await.1;
    let starts = seen.as_array().unwrap().iter().filter(|e| e["op"] == "event").count();
    assert!(starts >= 2, "TrackStartEvent reaches the plugin: {seen}");
}
