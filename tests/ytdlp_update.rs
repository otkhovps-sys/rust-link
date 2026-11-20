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
    let script = if let Some(version) = version.strip_prefix("slow:") {
        // A build whose first start takes longer than the probe allows.
        format!("#!/bin/sh\nsleep 5\necho {version}\n")
    } else if version.is_empty() {
        // A release that cannot start on this host.
        "#!/bin/sh\necho 'Failed to load Python DLL' >&2\nexit 1\n".to_string()
    } else {
        format!("#!/bin/sh\ntest -f \"$(dirname \"$0\")/_internal/base_library.zip\" || exit 3\necho {version}\n")
    };
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

    // Support files went missing (antivirus, damaged copy): startup downloads the build again.
    *release.corrupt_sums.lock().unwrap() = false;
    *release.archive.lock().unwrap() = fake_bundle("2099.02.02");
    let support = managed.join("yt-dlp-bundle/_internal/base_library.zip");
    std::fs::remove_file(&support).unwrap();
    ytdlp_update::ensure_installed(&cfg).await.unwrap();
    assert!(support.exists(), "bundle reinstalled");
    std::fs::remove_file(&support).unwrap();
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::Repaired {
            version: "2099.02.02".into()
        }
    );

    // A release that does not start is rolled back to the working build.
    *release.archive.lock().unwrap() = fake_bundle("");
    let error = ytdlp_update::update(&cfg).await.unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("does not start") && error.contains("Failed to load Python DLL"),
        "{error}"
    );
    let output = std::process::Command::new(&program).arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "2099.02.02",
        "working build restored"
    );

    let version_of = |program: &std::path::Path| {
        let output = std::process::Command::new(program).arg("--version").output().unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    ytdlp_update::set_probe_timeout(std::time::Duration::from_secs(1));

    // A new build that only times out is rolled back like one that fails.
    *release.archive.lock().unwrap() = fake_bundle("slow:2099.04.04");
    let error = format!("{:#}", ytdlp_update::update(&cfg).await.unwrap_err());
    assert!(
        error.contains("timed out") && error.contains("kept the previous build"),
        "{error}"
    );
    assert_eq!(
        version_of(&program),
        "2099.02.02",
        "working build restored after a timeout"
    );

    // An installed build that is only slow is kept, not downloaded again.
    let good = fake_bundle("2099.05.05");
    *release.archive.lock().unwrap() = good.clone();
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::Updated {
            version: "2099.05.05".into()
        }
    );
    let slow_script = "#!/bin/sh\nsleep 5\necho 2099.05.05\n";
    std::fs::write(&program, slow_script).unwrap();
    ytdlp_update::ensure_installed(&cfg).await.unwrap();
    assert_eq!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::UpToDate {
            version: "unknown version".into()
        }
    );
    assert_eq!(
        std::fs::read_to_string(&program).unwrap(),
        slow_script,
        "slow build kept"
    );
    ytdlp_update::set_probe_timeout(std::time::Duration::from_secs(60));

    // A rollback that could not finish (Windows kept the new files locked) left the
    // working build in yt-dlp-bundle.old: the next start puts it back, even offline.
    *release.archive.lock().unwrap() = good;
    std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
    assert!(matches!(
        ytdlp_update::update(&cfg).await.unwrap(),
        UpdateOutcome::Repaired { .. }
    ));
    let bundle = managed.join("yt-dlp-bundle");
    let previous = managed.join("yt-dlp-bundle.old");
    std::fs::rename(&bundle, &previous).unwrap();
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let offline = YtDlpConfig {
        update_url: "http://127.0.0.1:9".into(),
        ..cfg.clone()
    };
    let _ = ytdlp_update::ensure_installed(&offline).await;
    assert_eq!(version_of(&program), "2099.05.05", "previous build restored");
    assert!(!previous.exists());

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
