//! Keeps yt-dlp current. YouTube changes often and an outdated yt-dlp breaks every
//! YouTube track, so the node updates it on a schedule.
//!
//! * `sources.ytdlp.path: managed`: rust-link owns an official yt-dlp build in
//!   `sources.ytdlp.managedDir`. It uses the one-directory release (`yt-dlp_linux.zip`,
//!   ...), which starts about twice as fast as the single-file build because it does not
//!   unpack itself on every run. It is installed on first start and replaced when the
//!   release checksum (`SHA2-256SUMS`) no longer matches the installed archive. Every
//!   install is started once (`--version`): a build that does not start is rolled back,
//!   and an installed build that stopped starting (files removed by an antivirus, a
//!   damaged copy) is downloaded again. A `--version` that only runs out of time is
//!   slow, not broken: the installed build is kept. The previous build is deleted only
//!   once it is back in place, so a rollback Windows could not finish is completed on
//!   the next start.
//! * any other path: `yt-dlp -U` (works for the standalone builds; pip installs are
//!   reported with a hint instead).

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::YtDlpConfig;

pub const MANAGED: &str = "managed";
const SUMS_FILE: &str = "SHA2-256SUMS";
/// Extracted one-directory build inside `managedDir`.
const BUNDLE_DIR: &str = "yt-dlp-bundle";
/// Checksum of the release archive the bundle came from.
const MARKER: &str = ".release-sha256";
const SELF_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);
/// How long `--version` may take. The first start of a fresh build on Windows can be
/// slow while an antivirus scans it, so running out of time is not taken as broken.
static PROBE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(60_000);

/// Shorten the `--version` timeout (tests).
#[doc(hidden)]
pub fn set_probe_timeout(timeout: Duration) {
    PROBE_TIMEOUT_MS.store(timeout.as_millis() as u64, Ordering::Relaxed);
}

fn probe_timeout() -> Duration {
    Duration::from_millis(PROBE_TIMEOUT_MS.load(Ordering::Relaxed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Managed binary already matches the latest release.
    UpToDate { version: String },
    /// Managed binary downloaded for the first time.
    Installed { version: String },
    /// Managed binary replaced by a newer release.
    Updated { version: String },
    /// Managed binary no longer started and was downloaded again.
    Repaired { version: String },
    /// Result line of `yt-dlp -U` for a binary rust-link does not manage.
    SelfUpdate { message: String },
}

impl std::fmt::Display for UpdateOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateOutcome::UpToDate { version } => write!(f, "yt-dlp {version} is up to date"),
            UpdateOutcome::Installed { version } => write!(f, "installed yt-dlp {version}"),
            UpdateOutcome::Updated { version } => write!(f, "updated yt-dlp to {version}"),
            UpdateOutcome::Repaired { version } => write!(f, "reinstalled yt-dlp {version}"),
            UpdateOutcome::SelfUpdate { message } => write!(f, "{message}"),
        }
    }
}

pub fn is_managed(cfg: &YtDlpConfig) -> bool {
    cfg.path.trim().eq_ignore_ascii_case(MANAGED)
}

fn local_file_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

/// Executable to spawn for yt-dlp.
pub fn program_path(cfg: &YtDlpConfig) -> PathBuf {
    if is_managed(cfg) {
        cfg.managed_dir.join(BUNDLE_DIR).join(local_file_name())
    } else {
        PathBuf::from(&cfg.path)
    }
}

/// The host libc is musl (Alpine). rust-link itself is a static musl binary, so the
/// filesystem decides: a glibc loader wins, since glibc hosts may also carry musl.
fn musl_host() -> bool {
    const GLIBC_LOADERS: [&str; 3] = [
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    ];
    if GLIBC_LOADERS.iter().any(|p| Path::new(p).exists()) {
        return false;
    }
    std::fs::read_dir("/lib")
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
        })
        .unwrap_or(false)
}

/// Name of the official one-directory build (zip) for this platform.
pub fn release_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("yt-dlp_win.zip"),
        ("windows", "x86") => Some("yt-dlp_win_x86.zip"),
        ("windows", "aarch64") => Some("yt-dlp_win_arm64.zip"),
        ("macos", _) => Some("yt-dlp_macos.zip"),
        ("linux", "x86_64") if musl_host() => Some("yt-dlp_musllinux.zip"),
        ("linux", "x86_64") => Some("yt-dlp_linux.zip"),
        ("linux", "aarch64") if musl_host() => Some("yt-dlp_musllinux_aarch64.zip"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64.zip"),
        _ => None,
    }
}

/// Unpack the release archive into `dir` and give its executable a stable name.
fn extract_bundle(archive: &Path, dir: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).context("invalid yt-dlp archive")?;
    zip.extract(dir).context("cannot unpack the yt-dlp archive")?;
    let executable = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("yt-dlp"))
        })
        .ok_or_else(|| anyhow!("no yt-dlp executable in the archive"))?;
    let target = dir.join(local_file_name());
    if executable != target {
        std::fs::rename(&executable, &target)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// `sha256  name` lines of `SHA2-256SUMS`.
fn expected_sha256(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

/// Result of starting the executable once.
enum Probe {
    /// It started and printed its version.
    Started(String),
    /// It did not finish in time. Slow, not necessarily broken.
    TimedOut,
    /// It could not start: missing support files, a damaged copy, a wrong build.
    Failed(anyhow::Error),
}

/// Start the executable once. Succeeds only when it and its support files load, which
/// is what a damaged or half-removed bundle fails at.
async fn probe(program: &Path) -> Probe {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(probe_timeout(), output).await {
        Err(_) => return Probe::TimedOut,
        Ok(Err(e)) => return Probe::Failed(anyhow::Error::new(e).context(format!("cannot run {}", program.display()))),
        Ok(Ok(output)) => output,
    };
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !version.is_empty() {
        return Probe::Started(version);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output");
    Probe::Failed(anyhow!(
        "{} --version failed ({}): {reason}",
        program.display(),
        output.status
    ))
}

/// Remove a directory, retrying for a few seconds: on Windows an antivirus can hold
/// files of a program that just ran.
async fn remove_dir_retrying(dir: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt >= 5 => return Err(e),
            Err(_) => {}
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
    }
}

/// Put the previous build back in place of `bundle`. `previous` is only given up once
/// it has been moved back, so a failure leaves it on disk for the next attempt.
async fn restore_previous(bundle: &Path, previous: &Path) -> anyhow::Result<()> {
    remove_dir_retrying(bundle)
        .await
        .with_context(|| format!("cannot remove {}", bundle.display()))?;
    let mut attempt = 0;
    loop {
        match tokio::fs::rename(previous, bundle).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 5 => {
                return Err(anyhow::Error::new(e).context(format!(
                    "cannot move {} back to {}",
                    previous.display(),
                    bundle.display()
                )))
            }
            Err(_) => {}
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
    }
}

/// A previous build left behind by a rollback that could not finish is put back when
/// the current one does not start. Runs before any download, so it works offline.
async fn recover_previous(cfg: &YtDlpConfig) {
    let bundle = cfg.managed_dir.join(BUNDLE_DIR);
    let previous = cfg.managed_dir.join(format!("{BUNDLE_DIR}.old"));
    if !tokio::fs::try_exists(&previous).await.unwrap_or(false) {
        return;
    }
    let target = program_path(cfg);
    if tokio::fs::try_exists(&target).await.unwrap_or(false) && !matches!(probe(&target).await, Probe::Failed(_)) {
        return;
    }
    if !matches!(probe(&previous.join(local_file_name())).await, Probe::Started(_)) {
        return;
    }
    match restore_previous(&bundle, &previous).await {
        Ok(()) => info!(dir = %bundle.display(), "restored the previous yt-dlp build"),
        Err(error) => warn!(error = %format!("{error:#}"), "cannot restore the previous yt-dlp build"),
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("rust-link/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()?)
}

/// Install or refresh the managed build from `cfg.update_url`.
async fn update_managed(cfg: &YtDlpConfig, asset: &str) -> anyhow::Result<UpdateOutcome> {
    let client = http_client()?;
    recover_previous(cfg).await;
    let base = cfg.update_url.trim_end_matches('/');
    let sums = client
        .get(format!("{base}/{SUMS_FILE}"))
        .send()
        .await?
        .error_for_status()
        .context("cannot fetch the yt-dlp release checksums")?
        .text()
        .await?;
    let expected = expected_sha256(&sums, asset).ok_or_else(|| anyhow!("{asset} is missing from {SUMS_FILE}"))?;

    let bundle = cfg.managed_dir.join(BUNDLE_DIR);
    let target = program_path(cfg);
    let existed = tokio::fs::try_exists(&target).await.unwrap_or(false);
    let installed = tokio::fs::read_to_string(bundle.join(MARKER)).await.unwrap_or_default();
    let mut broken = false;
    if existed {
        match probe(&target).await {
            Probe::Started(version) if installed.trim() == expected => return Ok(UpdateOutcome::UpToDate { version }),
            Probe::Started(_) => {}
            Probe::TimedOut if installed.trim() == expected => {
                warn!(program = %target.display(), "yt-dlp --version timed out; keeping the installed build");
                return Ok(UpdateOutcome::UpToDate {
                    version: "unknown version".into(),
                });
            }
            Probe::TimedOut => {
                warn!(program = %target.display(), "yt-dlp --version timed out; updating it anyway");
            }
            Probe::Failed(error) => {
                warn!(error = %format!("{error:#}"), "the installed yt-dlp does not start, downloading it again");
                broken = true;
            }
        }
    }

    tokio::fs::create_dir_all(&cfg.managed_dir)
        .await
        .with_context(|| format!("cannot create {}", cfg.managed_dir.display()))?;
    let archive = cfg.managed_dir.join(format!("{asset}.download"));
    let response = client
        .get(format!("{base}/{asset}"))
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("cannot download {asset}"))?;
    let mut file = tokio::fs::File::create(&archive).await?;
    let mut hasher = Sha256::new();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        let _ = tokio::fs::remove_file(&archive).await;
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }

    let staging = cfg.managed_dir.join(format!("{BUNDLE_DIR}.new"));
    let previous = cfg.managed_dir.join(format!("{BUNDLE_DIR}.old"));
    let _ = tokio::fs::remove_dir_all(&staging).await;
    let _ = tokio::fs::remove_dir_all(&previous).await;
    let (archive_path, staging_dir) = (archive.clone(), staging.clone());
    let unpacked = tokio::task::spawn_blocking(move || extract_bundle(&archive_path, &staging_dir)).await?;
    let _ = tokio::fs::remove_file(&archive).await;
    if let Err(e) = unpacked {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(e);
    }
    tokio::fs::write(staging.join(MARKER), &expected).await?;
    // Directory renames keep running yt-dlp processes on the old files (on Windows the
    // swap fails while the old build is in use; the next scheduled check retries).
    if tokio::fs::try_exists(&bundle).await.unwrap_or(false) {
        tokio::fs::rename(&bundle, &previous)
            .await
            .with_context(|| format!("cannot replace {}", bundle.display()))?;
    }
    tokio::fs::rename(&staging, &bundle).await?;
    // The new build has to start before the previous one is given up. It is started in
    // its final place: on Windows a directory cannot be renamed right after one of its
    // files ran, while an antivirus still holds it.
    let fallback = !broken && tokio::fs::try_exists(&previous).await.unwrap_or(false);
    let error = match probe(&target).await {
        Probe::Started(version) => Ok(version),
        // Nothing to go back to: a slow build is better than none.
        Probe::TimedOut if !fallback => {
            warn!(program = %target.display(), "yt-dlp --version timed out; keeping the new build");
            Ok("unknown version".to_string())
        }
        Probe::TimedOut => Err(anyhow!("{} --version timed out", target.display())),
        Probe::Failed(error) => Err(error),
    };
    let version = match error {
        Ok(version) => version,
        Err(error) => {
            let error = error.context(format!("the downloaded {asset} does not start"));
            if fallback {
                if let Err(restore) = restore_previous(&bundle, &previous).await {
                    return Err(error.context(format!(
                        "{restore:#}; the previous build stays in {} and is restored on the next check",
                        previous.display()
                    )));
                }
                return Err(error.context("kept the previous build"));
            }
            let _ = remove_dir_retrying(&bundle).await;
            let _ = tokio::fs::remove_dir_all(&previous).await;
            return Err(error);
        }
    };
    let _ = tokio::fs::remove_dir_all(&previous).await;
    // Single-file build installed by earlier versions.
    let _ = tokio::fs::remove_file(cfg.managed_dir.join(local_file_name())).await;

    Ok(if broken {
        UpdateOutcome::Repaired { version }
    } else if existed {
        UpdateOutcome::Updated { version }
    } else {
        UpdateOutcome::Installed { version }
    })
}

/// `yt-dlp -U` for an installation rust-link does not own.
async fn self_update(cfg: &YtDlpConfig) -> anyhow::Result<UpdateOutcome> {
    let output = Command::new(program_path(cfg))
        .arg("-U")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(SELF_UPDATE_TIMEOUT, output)
        .await
        .map_err(|_| anyhow!("yt-dlp -U timed out"))?
        .with_context(|| format!("cannot run {}", cfg.path))?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let last = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string();
    if output.status.success() {
        Ok(UpdateOutcome::SelfUpdate { message: last })
    } else if text.contains("pip") {
        bail!("{last} (this yt-dlp was installed with pip: run `pip install -U yt-dlp`, or set sources.ytdlp.path: managed)")
    } else {
        bail!("{last}")
    }
}

/// Update yt-dlp once (the `update-ytdlp` command and the scheduled task).
pub async fn update(cfg: &YtDlpConfig) -> anyhow::Result<UpdateOutcome> {
    if is_managed(cfg) {
        let asset = release_asset().ok_or_else(|| {
            anyhow!(
                "no official yt-dlp build for {}/{}: install yt-dlp yourself and set sources.ytdlp.path",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        update_managed(cfg, asset).await
    } else {
        self_update(cfg).await
    }
}

/// Make sure a managed yt-dlp exists and starts before the node accepts connections.
pub async fn ensure_installed(cfg: &YtDlpConfig) -> anyhow::Result<()> {
    if !is_managed(cfg) {
        return Ok(());
    }
    recover_previous(cfg).await;
    let program = program_path(cfg);
    if tokio::fs::try_exists(&program).await.unwrap_or(false) {
        match probe(&program).await {
            Probe::Started(_) => return Ok(()),
            // Slow first start (antivirus scan): not a reason to download it again.
            Probe::TimedOut => {
                warn!(program = %program.display(), "yt-dlp --version timed out; keeping the installed build");
                return Ok(());
            }
            // `update` probes again, logs the reason and replaces the bundle.
            Probe::Failed(_) => {
                info!(dir = %cfg.managed_dir.display(), "repairing yt-dlp (sources.ytdlp.path: managed)")
            }
        }
    } else {
        info!(dir = %cfg.managed_dir.display(), "downloading yt-dlp (sources.ytdlp.path: managed)");
    }
    let outcome = update(cfg).await?;
    info!("{outcome}");
    Ok(())
}

/// Periodic updates until shutdown.
pub fn spawn_auto_update(cfg: YtDlpConfig, shutdown: CancellationToken) {
    if !cfg.auto_update {
        return;
    }
    let period = Duration::from_secs(cfg.update_interval_hours.max(1) * 3600);
    tokio::spawn(async move {
        // The first check waits a little so it never competes with startup work.
        let mut delay = Duration::from_secs(30);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            match update(&cfg).await {
                Ok(outcome) => info!("{outcome}"),
                Err(error) => warn!(error = %format!("{error:#}"), "yt-dlp update failed"),
            }
            delay = period;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checksum_lines() {
        let sums = "aa  yt-dlp\n\
                    0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF  yt-dlp_linux\n\
                    1111111111111111111111111111111111111111111111111111111111111111 *yt-dlp.exe\n";
        assert_eq!(
            expected_sha256(sums, "yt-dlp_linux").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert!(expected_sha256(sums, "yt-dlp.exe").is_some());
        assert_eq!(expected_sha256(sums, "yt-dlp"), None, "short hashes are ignored");
        assert_eq!(expected_sha256(sums, "yt-dlp_macos"), None);
    }

    #[test]
    fn resolves_program_paths() {
        let mut cfg = YtDlpConfig::default();
        assert_eq!(program_path(&cfg), PathBuf::from("yt-dlp"));
        cfg.path = "Managed".into();
        cfg.managed_dir = PathBuf::from("/data/bin");
        assert_eq!(
            program_path(&cfg),
            PathBuf::from("/data/bin").join(BUNDLE_DIR).join(local_file_name())
        );
        assert!(cfg!(not(target_os = "linux")) || release_asset().is_some());
    }
}
