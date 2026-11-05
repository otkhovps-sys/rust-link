//! Lavaplayer compatible track encoding (`MessageOutput` + `DataOutput` framing).
//!
//! Layout of a committed message:
//! `i32 header (size | flags << 30)` then the body:
//! `u8 version, UTF title, UTF author, i64 length, UTF identifier, bool isStream,
//!  nullable UTF uri (v2+), nullable UTF artworkUrl (v3), nullable UTF isrc (v3),
//!  UTF sourceName, <source specific data>, i64 position`.

use base64::{engine::general_purpose::STANDARD, Engine};

use super::models::TrackInfo;

const TRACK_INFO_VERSIONED: u32 = 1;
const TRACK_INFO_VERSION: u8 = 3;

#[derive(Debug, thiserror::Error)]
pub enum TrackCodecError {
    #[error("invalid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("unexpected end of track data")]
    Truncated,
    #[error("unsupported track version {0}")]
    UnsupportedVersion(u8),
    #[error("invalid message size")]
    InvalidSize,
}

pub fn encode_track(info: &TrackInfo) -> String {
    let mut body = Vec::with_capacity(256);
    body.push(TRACK_INFO_VERSION);
    write_utf(&mut body, &info.title);
    write_utf(&mut body, &info.author);
    body.extend_from_slice(&(info.length.min(i64::MAX as u64) as i64).to_be_bytes());
    write_utf(&mut body, &info.identifier);
    body.push(info.is_stream as u8);
    write_nullable_utf(&mut body, info.uri.as_deref());
    write_nullable_utf(&mut body, info.artwork_url.as_deref());
    write_nullable_utf(&mut body, info.isrc.as_deref());
    write_utf(&mut body, &info.source_name);
    if matches!(info.source_name.as_str(), "http" | "local") {
        // ProbingAudioSourceManager writes the container probe name.
        write_utf(&mut body, probe_name_for(info));
    }
    body.extend_from_slice(&(info.position as i64).to_be_bytes());

    let header = (body.len() as u32) | (TRACK_INFO_VERSIONED << 30);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&header.to_be_bytes());
    out.extend_from_slice(&body);
    STANDARD.encode(out)
}

pub fn decode_track(encoded: &str) -> Result<TrackInfo, TrackCodecError> {
    let raw = STANDARD.decode(encoded.trim())?;
    let mut reader = Reader::new(&raw);
    let header = reader.u32()?;
    let flags = header >> 30;
    let size = (header & 0x3FFF_FFFF) as usize;
    let body = raw.get(4..4 + size).ok_or(TrackCodecError::InvalidSize)?;
    if body.len() < 8 {
        return Err(TrackCodecError::Truncated);
    }

    let mut r = Reader::new(body);
    let version = if flags & TRACK_INFO_VERSIONED != 0 { r.u8()? } else { 1 };
    if !(1..=3).contains(&version) {
        return Err(TrackCodecError::UnsupportedVersion(version));
    }

    let title = r.utf()?;
    let author = r.utf()?;
    let length = r.i64()?.max(0) as u64;
    let identifier = r.utf()?;
    let is_stream = r.bool()?;
    let uri = if version >= 2 { r.nullable_utf()? } else { None };
    let (artwork_url, isrc) = if version >= 3 {
        (r.nullable_utf()?, r.nullable_utf()?)
    } else {
        (None, None)
    };
    let source_name = r.utf()?;
    // Source specific data (probe info, plugin fields, ...) sits between the source name and
    // the position; the position is always the trailing i64 of the message.
    let position_bytes: [u8; 8] = body[body.len() - 8..].try_into().expect("8 bytes");
    let position = i64::from_be_bytes(position_bytes).max(0) as u64;

    Ok(TrackInfo {
        identifier,
        is_seekable: !is_stream,
        author,
        length,
        is_stream,
        position,
        title,
        uri,
        artwork_url,
        isrc,
        source_name,
    })
}

fn probe_name_for(info: &TrackInfo) -> &'static str {
    let target = info.uri.as_deref().unwrap_or(&info.identifier).to_ascii_lowercase();
    let path = target.split(['?', '#']).next().unwrap_or_default();
    match path.rsplit('.').next().unwrap_or_default() {
        "flac" => "flac",
        "wav" => "wav",
        "ogg" | "oga" | "opus" => "ogg",
        "m4a" | "mp4" | "m4b" => "mp4",
        "webm" | "mkv" | "mka" => "matroska",
        "aac" => "adts",
        "m3u" | "m3u8" | "pls" => "playlist",
        _ => "mp3",
    }
}

fn write_nullable_utf(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(v) => {
            out.push(1);
            write_utf(out, v);
        }
        None => out.push(0),
    }
}

/// Java `DataOutput.writeUTF`: u16 length prefix + modified UTF-8.
fn write_utf(out: &mut Vec<u8>, value: &str) {
    let mut encoded = Vec::with_capacity(value.len());
    for unit in value.encode_utf16() {
        let mut tmp = [0u8; 3];
        let bytes: &[u8] = match unit {
            0x0001..=0x007F => {
                tmp[0] = unit as u8;
                &tmp[..1]
            }
            0x0000 | 0x0080..=0x07FF => {
                tmp[0] = 0xC0 | ((unit >> 6) & 0x1F) as u8;
                tmp[1] = 0x80 | (unit & 0x3F) as u8;
                &tmp[..2]
            }
            _ => {
                tmp[0] = 0xE0 | ((unit >> 12) & 0x0F) as u8;
                tmp[1] = 0x80 | ((unit >> 6) & 0x3F) as u8;
                tmp[2] = 0x80 | (unit & 0x3F) as u8;
                &tmp[..3]
            }
        };
        if encoded.len() + bytes.len() > u16::MAX as usize {
            break;
        }
        encoded.extend_from_slice(bytes);
    }
    out.extend_from_slice(&(encoded.len() as u16).to_be_bytes());
    out.extend_from_slice(&encoded);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TrackCodecError> {
        let end = self.pos.checked_add(n).ok_or(TrackCodecError::Truncated)?;
        let slice = self.data.get(self.pos..end).ok_or(TrackCodecError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, TrackCodecError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, TrackCodecError> {
        Ok(self.u8()? != 0)
    }

    fn u32(&mut self) -> Result<u32, TrackCodecError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64, TrackCodecError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn utf(&mut self) -> Result<String, TrackCodecError> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as usize;
        Ok(decode_modified_utf8(self.take(len)?))
    }

    fn nullable_utf(&mut self) -> Result<Option<String>, TrackCodecError> {
        if self.bool()? {
            self.utf().map(Some)
        } else {
            Ok(None)
        }
    }
}

fn decode_modified_utf8(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as u16;
        if b0 & 0x80 == 0 {
            units.push(b0);
            i += 1;
        } else if b0 & 0xE0 == 0xC0 && i + 1 < bytes.len() {
            units.push(((b0 & 0x1F) << 6) | (bytes[i + 1] as u16 & 0x3F));
            i += 2;
        } else if b0 & 0xF0 == 0xE0 && i + 2 < bytes.len() {
            units.push(((b0 & 0x0F) << 12) | ((bytes[i + 1] as u16 & 0x3F) << 6) | (bytes[i + 2] as u16 & 0x3F));
            i += 3;
        } else {
            units.push(0xFFFD);
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Example track from the Lavalink v4 documentation (track info version 2).
    const DOCS_TRACK: &str = "QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA==";

    #[test]
    fn decodes_lavalink_docs_track() {
        let info = decode_track(DOCS_TRACK).unwrap();
        assert_eq!(info.title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(info.author, "RickAstleyVEVO");
        assert_eq!(info.length, 212_000);
        assert_eq!(info.identifier, "dQw4w9WgXcQ");
        assert!(!info.is_stream);
        assert_eq!(info.uri.as_deref(), Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert_eq!(info.source_name, "youtube");
        assert_eq!(info.position, 0);
    }

    #[test]
    fn roundtrip_v3_with_unicode_and_http_probe() {
        let info = TrackInfo {
            identifier: "https://example.com/a.flac".into(),
            is_seekable: true,
            author: "Sơn Tùng M-TP \u{1F3B5}".into(),
            length: 123_456,
            is_stream: false,
            position: 42_000,
            title: "Chúng Ta Của Hiện Tại\0x".into(),
            uri: Some("https://example.com/a.flac".into()),
            artwork_url: Some("https://example.com/a.jpg".into()),
            isrc: Some("VNA0D2000001".into()),
            source_name: "http".into(),
        };
        let encoded = encode_track(&info);
        let decoded = decode_track(&encoded).unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_track("not base64 !!").is_err());
        assert!(decode_track("QAAAjQ==").is_err());
    }
}
