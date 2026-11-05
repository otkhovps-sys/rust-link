//! HLS (HTTP Live Streaming) client for live streams and `.m3u8` URLs.
//!
//! The media playlist is polled, new segments are downloaded in order and their audio is
//! extracted (MPEG-TS through [`TsDemuxer`], packed audio and fMP4 as-is) into one byte
//! stream that the decoder reads like any other live source. Nothing is cached.

use std::{io, time::Duration};

use anyhow::{anyhow, bail, Context};
use bytes::Bytes;
use reqwest::header::HeaderMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use url::Url;

use super::{
    mpegts::{looks_like_ts, TsDemuxer},
    ByteStream,
};

const LIVE_EDGE_SEGMENTS: usize = 3;
const SEGMENT_RETRIES: u32 = 3;
const VIDEO_CODECS: [&str; 6] = ["avc1", "avc3", "hvc1", "hev1", "vp09", "av01"];

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub sequence: u64,
    pub uri: Url,
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaPlaylist {
    pub target_duration: f64,
    pub segments: Vec<Segment>,
    pub ended: bool,
    /// fMP4 initialisation segment (`#EXT-X-MAP`).
    pub init: Option<Url>,
    pub encrypted: bool,
}

impl MediaPlaylist {
    pub fn duration_ms(&self) -> u64 {
        (self.segments.iter().map(|s| s.duration).sum::<f64>() * 1000.0).round() as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    /// Variant streams; the URL is the media playlist to play.
    Master(Url),
    Media(MediaPlaylist),
}

/// `KEY=value,KEY="quoted, value"` attribute lists.
fn attributes(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, remaining) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((value, remaining)) => (value.to_string(), remaining),
                None => (quoted.to_string(), ""),
            }
        } else {
            match after.split_once(',') {
                Some((value, remaining)) => (value.to_string(), remaining),
                None => (after.to_string(), ""),
            }
        };
        out.push((key.trim().to_ascii_uppercase(), value.trim().to_string()));
        rest = remaining.trim_start_matches(',').trim_start();
    }
    out
}

fn attribute<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn parse_playlist(text: &str, base: &Url) -> anyhow::Result<Playlist> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    if lines.peek() != Some(&"#EXTM3U") {
        bail!("not an HLS playlist (missing #EXTM3U)");
    }
    let mut media = MediaPlaylist {
        target_duration: 6.0,
        ..Default::default()
    };
    let mut sequence = 0u64;
    let mut pending_duration = None;
    // (audio-only?, bandwidth, uri) of the variant streams.
    let mut variants: Vec<(bool, u64, Url)> = Vec::new();
    let mut audio_renditions: Vec<(bool, Url)> = Vec::new();
    let mut pending_variant: Option<(bool, u64)> = None;

    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            media.target_duration = value.trim().parse().unwrap_or(6.0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value.split(',').next().unwrap_or("0").trim();
            pending_duration = Some(seconds.parse().unwrap_or(0.0));
        } else if line == "#EXT-X-ENDLIST" {
            media.ended = true;
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(value);
            if attribute(&attrs, "METHOD").is_some_and(|m| !m.eq_ignore_ascii_case("NONE")) {
                media.encrypted = true;
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(value);
            if let Some(uri) = attribute(&attrs, "URI") {
                media.init = Some(base.join(uri)?);
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(value);
            if attribute(&attrs, "TYPE") == Some("AUDIO") {
                if let Some(uri) = attribute(&attrs, "URI") {
                    let default = attribute(&attrs, "DEFAULT") == Some("YES");
                    audio_renditions.push((default, base.join(uri)?));
                }
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = attributes(value);
            let bandwidth = attribute(&attrs, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
            let codecs = attribute(&attrs, "CODECS").unwrap_or_default().to_ascii_lowercase();
            let audio_only = !codecs.is_empty() && !VIDEO_CODECS.iter().any(|v| codecs.contains(v));
            pending_variant = Some((audio_only, bandwidth));
        } else if line.starts_with('#') {
            continue;
        } else if let Some((audio_only, bandwidth)) = pending_variant.take() {
            variants.push((audio_only, bandwidth, base.join(line)?));
        } else {
            media.segments.push(Segment {
                sequence,
                uri: base.join(line)?,
                duration: pending_duration.take().unwrap_or(0.0),
            });
            sequence += 1;
        }
    }

    if let Some((_, url)) = audio_renditions
        .iter()
        .find(|(default, _)| *default)
        .or_else(|| audio_renditions.first())
    {
        return Ok(Playlist::Master(url.clone()));
    }
    if !variants.is_empty() {
        // Best audio-only variant, otherwise the cheapest variant (least video to discard).
        let audio_only = variants.iter().filter(|v| v.0).max_by_key(|v| v.1);
        let chosen = audio_only
            .or_else(|| variants.iter().min_by_key(|v| v.1))
            .expect("non-empty");
        return Ok(Playlist::Master(chosen.2.clone()));
    }
    Ok(Playlist::Media(media))
}

async fn fetch(client: &reqwest::Client, url: &Url, headers: &HeaderMap) -> anyhow::Result<Bytes> {
    let response = client
        .get(url.clone())
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?;
    Ok(response.bytes().await?)
}

/// Follow master playlists to the media playlist. Returns its URL and content.
pub async fn load_media_playlist(
    client: &reqwest::Client,
    url: &Url,
    headers: &HeaderMap,
) -> anyhow::Result<(Url, MediaPlaylist)> {
    let mut url = url.clone();
    for _ in 0..3 {
        let body = fetch(client, &url, headers).await.context("cannot load HLS playlist")?;
        let text = String::from_utf8_lossy(&body);
        match parse_playlist(&text, &url)? {
            Playlist::Master(next) => url = next,
            Playlist::Media(media) => {
                if media.encrypted {
                    bail!("encrypted HLS streams are not supported");
                }
                return Ok((url, media));
            }
        }
    }
    Err(anyhow!("too many nested HLS playlists"))
}

/// Live/VOD information for `/v4/loadtracks`.
pub async fn probe(client: &reqwest::Client, url: &Url) -> anyhow::Result<MediaPlaylist> {
    Ok(load_media_playlist(client, url, &HeaderMap::new()).await?.1)
}

/// Turns segment bytes into decoder input.
#[derive(Default)]
struct SegmentAudio {
    demuxer: Option<TsDemuxer>,
    decided: bool,
}

impl SegmentAudio {
    fn convert(&mut self, data: &[u8]) -> Bytes {
        if !self.decided {
            self.decided = true;
            if looks_like_ts(data) {
                self.demuxer = Some(TsDemuxer::new());
            }
        }
        match &mut self.demuxer {
            Some(demuxer) => {
                let mut out = Vec::with_capacity(data.len());
                demuxer.push(data, &mut out);
                Bytes::from(out)
            }
            None => Bytes::copy_from_slice(data),
        }
    }
}

async fn run(
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    tx: mpsc::Sender<io::Result<Bytes>>,
) -> anyhow::Result<()> {
    let (media_url, mut playlist) = load_media_playlist(&client, &url, &headers).await?;
    let mut audio = SegmentAudio::default();
    if let Some(init) = &playlist.init {
        let bytes = fetch(&client, init, &headers)
            .await
            .context("cannot load HLS init segment")?;
        if tx.send(Ok(bytes)).await.is_err() {
            return Ok(());
        }
        audio.decided = true;
    }
    let start = if playlist.ended {
        0
    } else {
        playlist.segments.len().saturating_sub(LIVE_EDGE_SEGMENTS)
    };
    let mut next_sequence = playlist.segments.get(start).map(|s| s.sequence).unwrap_or(0);
    let mut idle_since = tokio::time::Instant::now();

    loop {
        let mut progressed = false;
        let start_at = next_sequence;
        for segment in playlist.segments.iter().filter(|s| s.sequence >= start_at) {
            let mut attempt = 0;
            let bytes = loop {
                match fetch(&client, &segment.uri, &headers).await {
                    Ok(bytes) => break Some(bytes),
                    Err(e) if attempt + 1 < SEGMENT_RETRIES => {
                        attempt += 1;
                        debug!(error = %e, attempt, "retrying HLS segment");
                        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                    }
                    Err(e) if playlist.ended => return Err(e.context("HLS segment download failed")),
                    Err(e) => {
                        warn!(error = %e, sequence = segment.sequence, "skipping HLS segment");
                        break None;
                    }
                }
            };
            next_sequence = segment.sequence + 1;
            progressed = true;
            if let Some(bytes) = bytes {
                let data = audio.convert(&bytes);
                if !data.is_empty() && tx.send(Ok(data)).await.is_err() {
                    return Ok(());
                }
            }
        }
        if playlist.ended {
            return Ok(());
        }
        if progressed {
            idle_since = tokio::time::Instant::now();
        } else if idle_since.elapsed() > Duration::from_secs_f64(playlist.target_duration.max(1.0) * 6.0) {
            bail!("the live stream stopped producing segments");
        }
        let wait = Duration::from_secs_f64((playlist.target_duration / 2.0).clamp(1.0, 10.0));
        tokio::select! {
            _ = tx.closed() => return Ok(()),
            _ = tokio::time::sleep(wait) => {}
        }
        playlist = load_media_playlist(&client, &media_url, &headers).await?.1;
        if let Some(first) = playlist.segments.first() {
            // Fell behind the sliding window: jump to the oldest available segment.
            next_sequence = next_sequence.max(first.sequence);
        }
    }
}

/// Start streaming the audio of an HLS playlist.
pub fn open(client: reqwest::Client, url: Url, headers: HeaderMap) -> ByteStream {
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    let error_tx = tx.clone();
    tokio::spawn(async move {
        if let Err(e) = run(client, url, headers, tx).await {
            let _ = error_tx.send(Err(io::Error::other(format!("{e:#}")))).await;
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    ByteStream::new(Box::pin(stream), None)
}

pub fn is_hls_url(url: &Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/live/index.m3u8?token=1").unwrap()
    }

    #[test]
    fn parses_media_playlists() {
        let text = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:100\n\
                    #EXTINF:5.005,\nseg100.ts\n#EXTINF:4.995,title\nhttps://other.example/seg101.ts\n";
        let Playlist::Media(media) = parse_playlist(text, &base()).unwrap() else {
            panic!("media playlist expected")
        };
        assert_eq!(media.target_duration, 5.0);
        assert!(!media.ended);
        assert_eq!(media.segments.len(), 2);
        assert_eq!(media.segments[0].sequence, 100);
        assert_eq!(media.segments[0].uri.as_str(), "https://cdn.example/live/seg100.ts");
        assert_eq!(media.segments[1].uri.as_str(), "https://other.example/seg101.ts");
        assert_eq!(media.duration_ms(), 10_000);

        let vod = parse_playlist("#EXTM3U\n#EXTINF:2,\na.aac\n#EXT-X-ENDLIST\n", &base()).unwrap();
        assert!(matches!(vod, Playlist::Media(m) if m.ended && m.segments.len() == 1));
        let encrypted = parse_playlist(
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:2,\na.ts\n",
            &base(),
        );
        assert!(matches!(encrypted, Ok(Playlist::Media(m)) if m.encrypted));
        assert!(parse_playlist("<html>", &base()).is_err());
    }

    #[test]
    fn picks_audio_from_master_playlists() {
        let video_only = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720\nhigh.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=300000,CODECS=\"avc1.42c00d,mp4a.40.2\"\nlow.m3u8\n";
        assert_eq!(
            parse_playlist(video_only, &base()).unwrap(),
            Playlist::Master(base().join("low.m3u8").unwrap())
        );

        let with_audio_variant =
            format!("{video_only}#EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\naudio.m3u8\n");
        assert_eq!(
            parse_playlist(&with_audio_variant, &base()).unwrap(),
            Playlist::Master(base().join("audio.m3u8").unwrap())
        );

        let rendition = format!(
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English, main\",DEFAULT=YES,URI=\"en/audio.m3u8\"\n{}",
            video_only.trim_start_matches("#EXTM3U\n")
        );
        assert_eq!(
            parse_playlist(&rendition, &base()).unwrap(),
            Playlist::Master(base().join("en/audio.m3u8").unwrap())
        );
    }

    #[test]
    fn parses_quoted_attributes() {
        let attrs = attributes("TYPE=AUDIO,NAME=\"a, b\",URI=\"x.m3u8\",DEFAULT=YES");
        assert_eq!(attribute(&attrs, "NAME"), Some("a, b"));
        assert_eq!(attribute(&attrs, "URI"), Some("x.m3u8"));
        assert_eq!(attribute(&attrs, "DEFAULT"), Some("YES"));
    }
}
