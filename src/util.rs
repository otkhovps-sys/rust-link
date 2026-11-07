use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::{distributions::Alphanumeric, Rng};
use sha2::{Digest, Sha256};

/// Unix timestamp in milliseconds.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Lavalink style session id: 16 lowercase alphanumeric characters.
pub fn random_session_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect()
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(data.as_ref()))
}

/// Exponential backoff with jitter, capped at `max`.
pub fn backoff(attempt: u32, base: Duration, max: Duration) -> Duration {
    let exp = base.saturating_mul(1u32 << attempt.min(16));
    let capped = exp.min(max);
    let jitter = rand::thread_rng().gen_range(0..=capped.as_millis() as u64 / 4 + 1);
    capped + Duration::from_millis(jitter)
}

/// Truncate a string to at most `max` bytes on a char boundary.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Atomic JSON write (temp file + rename) with owner-only permissions, for credentials.
pub async fn write_private_json(path: &std::path::Path, value: &impl serde::Serialize) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    tokio::fs::write(&tmp, json).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_shape() {
        let id = random_session_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate_utf8("héllo", 2), "h");
        assert_eq!(truncate_utf8("abc", 10), "abc");
    }
}
