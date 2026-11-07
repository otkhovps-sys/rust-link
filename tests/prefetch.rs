//! Rolling prefetch: a playlist load caches its first tracks, playing one caches the next.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{fake_ytdlp::FakeYtDlp, voice_mock::lavalink_ws, TestNode};
use serde_json::{json, Value};

const PLAYLIST: &str = r#"{"_type":"playlist","title":"Mix","entries":[
{"_type":"url","ie_key":"Youtube","id":"track000001","title":"One","duration":1.0},
{"_type":"url","ie_key":"Youtube","id":"track000002","title":"Two","duration":1.0},
{"_type":"url","ie_key":"Youtube","id":"track000003","title":"Three","duration":1.0},
{"_type":"url","ie_key":"Youtube","id":"track000004","title":"Four","duration":1.0},
{"_type":"url","ie_key":"Youtube","id":"track000005","title":"Five","duration":1.0}]}"#;

async fn wait_for_downloads(ytdlp: &FakeYtDlp, count: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let downloads = ytdlp.downloads();
        if downloads.len() >= count || tokio::time::Instant::now() > deadline {
            // Give stray extra downloads a moment to show up.
            tokio::time::sleep(Duration::from_millis(300)).await;
            return ytdlp.downloads();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn downloaded_ids(downloads: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = downloads
        .iter()
        .filter_map(|line| line.split("watch?v=").nth(1))
        .map(|id| id[..11].to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn playlist_tracks_are_cached_ahead_of_playback() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), &PLAYLIST.replace('\n', ""), 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| c.sources.ytdlp.path = path).await;

    let playlist: Value = node
        .get("/v4/loadtracks")
        .query(&[("identifier", "https://www.youtube.com/playlist?list=PLmix")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(playlist["loadType"], "playlist", "{playlist}");

    let downloads = wait_for_downloads(&ytdlp, 3).await;
    assert_eq!(
        downloaded_ids(&downloads),
        ["track000001", "track000002", "track000003"],
        "first three tracks prefetched"
    );
    let summary: Value = node
        .get("/v4/rustlink/cache")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary["summary"]["entries"], 3);

    // Starting track two (no voice connection needed) prefetches three, four and five;
    // three is already cached.
    let (_events, session_id) = lavalink_ws(&node).await;
    let second = playlist["data"]["tracks"][1]["encoded"].as_str().unwrap();
    let response = node
        .patch(&format!("/v4/sessions/{session_id}/players/3"))
        .json(&json!({"track": {"encoded": second}}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let downloads = wait_for_downloads(&ytdlp, 5).await;
    assert_eq!(
        downloaded_ids(&downloads),
        [
            "track000001",
            "track000002",
            "track000003",
            "track000004",
            "track000005"
        ],
        "every track downloaded exactly once"
    );
}

#[tokio::test]
async fn prefetch_can_be_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), &PLAYLIST.replace('\n', ""), 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| {
        c.sources.ytdlp.path = path;
        c.cache.prefetch_tracks = 0;
    })
    .await;
    node.get("/v4/loadtracks")
        .query(&[("identifier", "https://www.youtube.com/playlist?list=PLmix")])
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(ytdlp.downloads().is_empty());
}
