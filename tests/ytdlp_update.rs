//! Managed yt-dlp install / update against a mock release server.
#![cfg(unix)]

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use axum::{extract::State, http::StatusCode, routing::get, Router};
use rust_link::{
    config::YtDlpConfig,
    source::ytdlp_update::{self, release_asset, UpdateOutcome},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Default)]
struct Release {
    archive: Arc<Mutex<Vec<u8>>>,
    corrupt_sums: Arc<Mutex<bool>>,
}

/// One-directory build: the executable plus its `_internal` folder, like the real zips.
fn fake_bundle(version: &str) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let exe = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    zip.start_file("yt-dlp_linux", exe).unwrap();
    // The script checks that its support files sit next to it, as PyInstaller does.
    let script =
        format!("#!/bin/sh\ntest -f \"$(dirname \"$0\")/_internal/base_library.zip\" || exit 3\necho {version}\n");
    zip.write_all(script.as_bytes()).unwrap();
    zip.add_directory("_internal/", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.start_file("_internal/base_library.zip", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"python stdlib").unwrap();
    zip.finish().unwrap().into_inner()
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
                    hex::encode(Sha256::digest(&*r.archive.lock().unwrap()))
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
            get(|State(r): State<Release>| async move { (StatusCode::OK, r.archive.lock().unwrap().clone()) }),
        )
        .with_state(release);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn installs_updates_and_verifies_managed_ytdlp() {
    assert!(release_asset().unwrap().ends_with(".zip"), "one-directory build");
    let release = Release::default();
    *release.archive.lock().unwrap() = fake_bundle("2099.01.01");
    let base = start(release.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let managed = dir.path().join("bin");
    std::fs::create_dir_all(&managed).unwrap();
    // A single-file build left by an older rust-link is removed after the install.
    std::fs::write(managed.join("yt-dlp"), b"old single-file build").unwrap();
    let cfg = YtDlpConfig {
        path: "managed".into(),
        managed_dir: managed.clone(),
        update_url: base,
        ..Default::default()
    };
    // Bypass any HTTP proxy configured for the test environment.
    std::env::set_var("NO_PROXY", "127.0.0.1");

    ytdlp_update::ensure_installed(&cfg).await.unwrap();
    let program = ytdlp_update::program_path(&cfg);
    assert!(program.ends_with("yt-dlp-bundle/yt-dlp"), "{}", program.display());
    assert!(program.exists());
    assert!(!managed.join("yt-dlp").exists(), "legacy single-file build removed");
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::UpToDate {
            version: "2099.01.01".into()
        }
    );

    *release.archive.lock().unwrap() = fake_bundle("2099.02.02");
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::Updated {
            version: "2099.02.02".into()
        }
    );

    *release.archive.lock().unwrap() = fake_bundle("2099.03.03");
    *release.corrupt_sums.lock().unwrap() = true;
    let error = ytdlp_update::update(&cfg).await.unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
    let output = std::process::Command::new(&program).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "2099.02.02",
        "old build kept"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&managed)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        leftovers,
        ["yt-dlp-bundle"],
        "no partial downloads or staging directories"
    );
}
