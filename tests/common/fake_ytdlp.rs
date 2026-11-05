//! yt-dlp stand-in for tests (unix shell script): every call is appended to a log;
//! `--dump-single-json` prints `search_json`, anything else prints a WAV file.

use std::path::{Path, PathBuf};

use super::write_wav;

/// Two YouTube candidates: a 10 h loop and a 2 s "official audio".
pub const TWO_CANDIDATES: &str = r#"{"_type":"playlist","entries":[{"_type":"url","ie_key":"Youtube","id":"aaaaaaaaaaa","title":"10 hour loop","duration":36000.0,"channel":"x"},{"_type":"url","ie_key":"Youtube","id":"bbbbbbbbbbb","title":"Official audio","duration":2.0,"channel":"y"}]}"#;

pub struct FakeYtDlp {
    pub path: String,
    pub log: PathBuf,
}

impl FakeYtDlp {
    pub fn install(dir: &Path, search_json: &str, wav_seconds: f32) -> Self {
        let wav = dir.join("tone.wav");
        write_wav(&wav, 48_000, 2, wav_seconds);
        let log = dir.join("ytdlp.log");
        let script = dir.join("yt-dlp");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n  *--dump-single-json*) echo '{search_json}' ;;\n  *) cat '{}' ;;\nesac\n",
                log.display(),
                wav.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            path: script.display().to_string(),
            log,
        }
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn searches(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|l| l.contains("--dump-single-json"))
            .collect()
    }

    pub fn downloads(&self) -> Vec<String> {
        self.calls().into_iter().filter(|l| l.contains("-o -")).collect()
    }
}
