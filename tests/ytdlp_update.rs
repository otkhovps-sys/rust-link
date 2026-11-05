//! Managed yt-dlp install / update against a mock release server.
#![cfg(unix)]

use std::sync::{Arc, Mutex};

use axum::{extract::State, http::StatusCode, routing::get, Router};
use rust_link::{
    config::YtDlpConfig,
    source::ytdlp_update::{self, release_asset, UpdateOutcome},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Default)]
struct Release {
    binary: Arc<Mutex<Vec<u8>>>,
    corrupt_sums: Arc<Mutex<bool>>,
}

fn fake_ytdlp(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {version}\n").into_bytes()
}

async fn start(release: Release) -> String {
    let asset = release_asset().expect("linux host has an official build");
    let app = Router::new()
        .route(
            "/SHA2-256SUMS",
            get(|State(r): State<Release>| async move {
                let hash = if *r.corrupt_sums.lock().unwrap() {
                    "0".repeat(64)
                } else {
                    hex::encode(Sha256::digest(&*r.binary.lock().unwrap()))
                };
                format!(
                    "{hash}  {}\n{}  yt-dlp_other\n",
                    release_asset().unwrap(),
                    "1".repeat(64)
                )
            }),
        )
        .route(
            &format!("/{asset}"),
            get(|State(r): State<Release>| async move { (StatusCode::OK, r.binary.lock().unwrap().clone()) }),
        )
        .with_state(release);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn installs_updates_and_verifies_managed_ytdlp() {
    let release = Release::default();
    *release.binary.lock().unwrap() = fake_ytdlp("2099.01.01");
    let base = start(release.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = YtDlpConfig {
        path: "managed".into(),
        managed_dir: dir.path().join("bin"),
        update_url: base,
        ..Default::default()
    };
    // Bypass any HTTP proxy configured for the test environment.
    std::env::set_var("NO_PROXY", "127.0.0.1");

    ytdlp_update::ensure_installed(&cfg).await.unwrap();
    let program = ytdlp_update::program_path(&cfg);
    assert!(program.exists());
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::UpToDate {
            version: "2099.01.01".into()
        }
    );

    *release.binary.lock().unwrap() = fake_ytdlp("2099.02.02");
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::Updated {
            version: "2099.02.02".into()
        }
    );

    *release.binary.lock().unwrap() = fake_ytdlp("2099.03.03");
    *release.corrupt_sums.lock().unwrap() = true;
    let error = ytdlp_update::update(&cfg).await.unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
    let output = std::process::Command::new(&program).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "2099.02.02",
        "old binary kept"
    );
    assert!(!dir.path().join("bin/yt-dlp.download").exists());
}
