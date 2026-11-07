//! YouTube Mix links (`list=RD...`) load as a limited playlist.
#![cfg(unix)]

mod common;

use common::{fake_ytdlp::FakeYtDlp, TestNode};
use rust_link::api::models::LoadResult;
use serde_json::{json, Value};

const SEED: &str = "Vt4kAu-ziRY";
const MIX_URL: &str = "https://www.youtube.com/watch?v=Vt4kAu-ziRY&list=RDVt4kAu-ziRY";

/// What yt-dlp prints for a mix: the seed video first, then related videos. The fake
/// ignores `--playlist-end`, so the node has to cut the list itself as well.
fn mix_json(entries: usize) -> String {
    let mut ids = vec![SEED.to_string()];
    ids.extend((1..entries).map(|i| format!("mixvideo{i:03}")));
    let entries: Vec<Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| json!({"_type": "url", "ie_key": "Youtube", "id": id, "title": format!("Song {i}"), "duration": 200.0, "channel": "artist"}))
        .collect();
    json!({"_type": "playlist", "id": "RDVt4kAu-ziRY", "title": "Mix - Song 0", "entries": entries}).to_string()
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

#[tokio::test]
async fn mix_links_load_a_limited_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), &mix_json(40), 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| c.sources.ytdlp.path = path).await;

    let LoadResult::Playlist(mix) = load(&node, MIX_URL).await else {
        panic!("a mix link must load as a playlist");
    };
    assert_eq!(mix.info.name, "Mix - Song 0");
    assert_eq!(mix.tracks.len(), 25, "default youtubeMixLimit");
    assert_eq!(mix.info.selected_track, 0);
    assert_eq!(mix.tracks[0].info.identifier, SEED);
    assert!(mix.tracks.iter().all(|t| t.info.source_name == "youtube"));

    let calls = ytdlp.searches();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("--playlist-end 25"), "{}", calls[0]);
    assert!(
        calls[0].contains(MIX_URL),
        "the mix list stays in the URL: {}",
        calls[0]
    );

    // YouTube Music radio links are mixes too; music.youtube.com is rewritten to www.
    let radio = "https://music.youtube.com/watch?v=Vt4kAu-ziRY&list=RDAMVMVt4kAu-ziRY";
    assert!(matches!(load(&node, radio).await, LoadResult::Playlist(_)));
    assert!(ytdlp
        .searches()
        .last()
        .unwrap()
        .contains("https://www.youtube.com/watch?v=Vt4kAu-ziRY&list=RDAMVMVt4kAu-ziRY"));
}

#[tokio::test]
async fn mix_limit_is_configurable_and_zero_plays_the_video() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), &mix_json(40), 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| {
        c.sources.ytdlp.path = path;
        c.sources.youtube_mix_limit = 3;
    })
    .await;
    let LoadResult::Playlist(mix) = load(&node, MIX_URL).await else {
        panic!("expected a playlist");
    };
    assert_eq!(mix.tracks.len(), 3);
    assert!(ytdlp.searches()[0].contains("--playlist-end 3"));

    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), &mix_json(1), 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| {
        c.sources.ytdlp.path = path;
        c.sources.youtube_mix_limit = 0;
    })
    .await;
    load(&node, MIX_URL).await;
    let call = &ytdlp.searches()[0];
    assert!(!call.contains("list=") && !call.contains("--playlist-end"), "{call}");
    assert!(call.contains("https://www.youtube.com/watch?v=Vt4kAu-ziRY"));
}

#[tokio::test]
async fn unreadable_mix_falls_back_to_the_linked_video() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let video = json!({"id": SEED, "title": "Seed song", "duration": 200.0, "channel": "artist",
        "webpage_url": format!("https://www.youtube.com/watch?v={SEED}"), "extractor_key": "Youtube"});
    let script = dir.path().join("yt-dlp");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncase \"$*\" in\n  *list=*) echo 'ERROR: mix unavailable' >&2; exit 1 ;;\n  *) echo '{video}' ;;\nesac\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = script.display().to_string();
    let node = TestNode::start(move |c| c.sources.ytdlp.path = path).await;

    match load(&node, MIX_URL).await {
        LoadResult::Track(track) => assert_eq!(track.info.identifier, SEED),
        other => panic!("expected the linked video, got {other:?}"),
    }
}
