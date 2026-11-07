//! `/v4/loadtracks` results reused from the SQLite load cache.
#![cfg(unix)]

mod common;

use common::{
    fake_ytdlp::{FakeYtDlp, TWO_CANDIDATES},
    TestNode,
};
use serde_json::Value;

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
async fn repeated_searches_skip_ytdlp() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| c.sources.ytdlp.path = path).await;

    let first = load(&node, "ytsearch:Hello World").await;
    assert_eq!(first["loadType"], "search", "{first}");
    let again = load(&node, "YTSEARCH:  hello   world ").await;
    assert_eq!(again, first, "same normalized query, same answer");
    assert_eq!(ytdlp.searches().len(), 1, "{:?}", ytdlp.calls());

    load(&node, "ytsearch:another song").await;
    assert_eq!(ytdlp.searches().len(), 2, "a different query is resolved");

    // Links of music services are cached too; cache searches are always fresh.
    load(&node, "https://www.youtube.com/playlist?list=PLabc").await;
    load(&node, "https://www.youtube.com/playlist?list=PLabc").await;
    assert_eq!(ytdlp.searches().len(), 3, "{:?}", ytdlp.calls());
    load(&node, "cachesearch:hello").await;
    load(&node, "cachesearch:hello").await;
    assert_eq!(ytdlp.searches().len(), 3);
}

#[tokio::test]
async fn load_cache_can_be_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let ytdlp = FakeYtDlp::install(dir.path(), TWO_CANDIDATES, 1.0);
    let path = ytdlp.path.clone();
    let node = TestNode::start(move |c| {
        c.sources.ytdlp.path = path;
        c.sources.load_cache_ttl_secs = 0;
    })
    .await;
    load(&node, "ytsearch:hello").await;
    load(&node, "ytsearch:hello").await;
    assert_eq!(ytdlp.searches().len(), 2);
}
