//! Blocking reader over a file that is still being downloaded.
//!
//! Reads past the written watermark wait for the downloader instead of returning EOF, so
//! symphonia can start decoding before the download completes.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use symphonia::core::io::MediaSource;

use super::{DownloadProgress, ProgressState};

const POLL: Duration = Duration::from_millis(200);

pub struct ProgressiveReader {
    file: File,
    pos: u64,
    file_pos: u64,
    progress: Arc<DownloadProgress>,
    cancel: Arc<AtomicBool>,
}

fn cancelled_error() -> io::Error {
    io::Error::other("playback cancelled")
}

impl ProgressiveReader {
    pub fn open(progress: Arc<DownloadProgress>, cancel: Arc<AtomicBool>) -> io::Result<Self> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled_error());
            }
            match File::open(&progress.part_path) {
                Ok(file) => return Ok(Self::with_file(file, progress, cancel)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let state = progress.snapshot();
                    if let Some(error) = state.error {
                        return Err(io::Error::other(error));
                    }
                    if state.done {
                        // Download finished and the part file was renamed in between.
                        let file = File::open(&progress.final_path)?;
                        return Ok(Self::with_file(file, progress, cancel));
                    }
                    progress.wait_blocking(|s| s.written > 0 || s.done || s.error.is_some(), POLL);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn with_file(file: File, progress: Arc<DownloadProgress>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            file,
            pos: 0,
            file_pos: 0,
            progress,
            cancel,
        }
    }

    fn wait(&self, ready: impl Fn(&ProgressState) -> bool) -> io::Result<ProgressState> {
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(cancelled_error());
            }
            let state = self.progress.wait_blocking(&ready, POLL);
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if ready(&state) {
                return Ok(state);
            }
        }
    }

    fn total_len(&self) -> io::Result<u64> {
        let state = self.wait(|s| s.total.is_some() || s.done)?;
        Ok(state.total.unwrap_or(state.written))
    }
}

impl Read for ProgressiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pos = self.pos;
        let state = self.wait(|s| s.written > pos || s.done)?;
        let limit = if state.done {
            buf.len()
        } else {
            buf.len().min((state.written - pos) as usize)
        };
        if self.file_pos != self.pos {
            self.file.seek(SeekFrom::Start(self.pos))?;
            self.file_pos = self.pos;
        }
        let n = self.file.read(&mut buf[..limit])?;
        self.pos += n as u64;
        self.file_pos = self.pos;
        Ok(n)
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let new_pos = match target {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.total_len()?.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))?;
        self.pos = new_pos;
        Ok(new_pos)
    }
}

impl MediaSource for ProgressiveReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        let state = self.progress.snapshot();
        state.total.or(state.done.then_some(state.written))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reader_waits_for_downloader() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("a.part");
        let fin = dir.path().join("a.bin");
        let progress = Arc::new(DownloadProgress::new(part.clone(), fin.clone()));
        let cancel = Arc::new(AtomicBool::new(false));

        let writer_progress = Arc::clone(&progress);
        let writer = std::thread::spawn(move || {
            let mut f = File::create(&part).unwrap();
            let mut written = 0;
            for chunk in 0..10u8 {
                f.write_all(&[chunk; 1000]).unwrap();
                f.flush().unwrap();
                written += 1000;
                writer_progress.set_written(written);
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::rename(&part, &fin).unwrap();
            writer_progress.complete();
        });

        let mut reader = ProgressiveReader::open(Arc::clone(&progress), cancel).unwrap();
        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        writer.join().unwrap();
        assert_eq!(all.len(), 10_000);
        assert_eq!(all[9_999], 9);
        assert_eq!(reader.seek(SeekFrom::End(-1000)).unwrap(), 9_000);
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert!(tail.iter().all(|b| *b == 9));
    }

    #[test]
    fn reader_reports_download_errors_and_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("b.part");
        std::fs::write(&part, b"abc").unwrap();
        let progress = Arc::new(DownloadProgress::new(part, dir.path().join("b.bin")));
        progress.set_written(3);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut reader = ProgressiveReader::open(Arc::clone(&progress), Arc::clone(&cancel)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 3);
        progress.fail("boom");
        assert!(reader.read(&mut buf).is_err());

        let progress = Arc::new(DownloadProgress::new(
            dir.path().join("c.part"),
            dir.path().join("c.bin"),
        ));
        cancel.store(true, Ordering::Relaxed);
        assert!(ProgressiveReader::open(progress, cancel).is_err());
    }
}
