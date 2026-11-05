//! Minimal MPEG-TS demuxer: extracts the first audio elementary stream (AAC in ADTS
//! framing, or MPEG audio) so symphonia can decode HLS segments.
//!
//! PAT -> PMT -> audio PID, then the PES payloads of that PID are emitted as one
//! continuous byte stream. Sections spanning several packets are not supported (PAT and
//! PMT of HLS streams fit in one packet).

pub const PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// ISO/IEC 13818-7 AAC with ADTS headers (stream_type 0x0F).
    AacAdts,
    /// MPEG-1/2 audio, usually MP3 (stream_type 0x03 / 0x04).
    MpegAudio,
}

impl AudioCodec {
    /// Container hint for the decoder.
    pub fn hint(self) -> &'static str {
        match self {
            AudioCodec::AacAdts => "aac",
            AudioCodec::MpegAudio => "mp3",
        }
    }

    fn from_stream_type(stream_type: u8) -> Option<Self> {
        match stream_type {
            0x0F => Some(AudioCodec::AacAdts),
            0x03 | 0x04 => Some(AudioCodec::MpegAudio),
            _ => None,
        }
    }
}

/// True when `data` looks like a transport stream (sync bytes every 188 bytes).
pub fn looks_like_ts(data: &[u8]) -> bool {
    let packets = (data.len() / PACKET_SIZE).clamp(0, 3);
    packets > 0 && (0..packets).all(|i| data[i * PACKET_SIZE] == SYNC_BYTE)
}

#[derive(Debug, Default)]
pub struct TsDemuxer {
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
    codec: Option<AudioCodec>,
    /// Bytes of the current PES header still to skip (headers may straddle packets).
    skip: usize,
    in_pes: bool,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn codec(&self) -> Option<AudioCodec> {
        self.codec
    }

    /// Feed transport stream bytes; audio payload is appended to `out`.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(data);
        let mut offset = 0;
        while self.pending.len() - offset >= PACKET_SIZE {
            if self.pending[offset] != SYNC_BYTE {
                // Resynchronise on the next sync byte.
                offset += 1;
                continue;
            }
            let mut packet = [0u8; PACKET_SIZE];
            packet.copy_from_slice(&self.pending[offset..offset + PACKET_SIZE]);
            self.packet(&packet, out);
            offset += PACKET_SIZE;
        }
        self.pending.drain(..offset);
    }

    fn packet(&mut self, p: &[u8; PACKET_SIZE], out: &mut Vec<u8>) {
        let unit_start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let adaptation = (p[3] >> 4) & 0x03;
        let mut start = 4;
        if adaptation & 0x02 != 0 {
            start += 1 + p[4] as usize;
        }
        if adaptation & 0x01 == 0 || start >= PACKET_SIZE {
            return;
        }
        let payload = &p[start..];
        if pid == 0 {
            if unit_start {
                self.parse_pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if unit_start {
                self.parse_pmt(payload);
            }
        } else if Some(pid) == self.audio_pid {
            self.audio(payload, unit_start, out);
        }
    }

    /// Payload of a PSI section after the pointer field, bounded by `section_length`.
    fn section(payload: &[u8]) -> Option<&[u8]> {
        let pointer = *payload.first()? as usize;
        let section = payload.get(1 + pointer..)?;
        let length = ((usize::from(*section.get(1)?) & 0x0F) << 8) | usize::from(*section.get(2)?);
        // Excludes the trailing CRC32.
        section.get(..(3 + length).checked_sub(4)?)
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(section) = Self::section(payload) else { return };
        if section.first() != Some(&0x00) {
            return;
        }
        for entry in section.get(8..).unwrap_or_default().as_chunks::<4>().0 {
            let program = u16::from_be_bytes([entry[0], entry[1]]);
            if program != 0 {
                self.pmt_pid = Some((u16::from(entry[2] & 0x1F) << 8) | u16::from(entry[3]));
                return;
            }
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(section) = Self::section(payload) else { return };
        if section.first() != Some(&0x02) || section.len() < 12 {
            return;
        }
        let info_length = ((usize::from(section[10]) & 0x0F) << 8) | usize::from(section[11]);
        let mut i = 12 + info_length;
        while i + 5 <= section.len() {
            let stream_type = section[i];
            let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
            let es_info = ((usize::from(section[i + 3]) & 0x0F) << 8) | usize::from(section[i + 4]);
            if let Some(codec) = AudioCodec::from_stream_type(stream_type) {
                if self.audio_pid != Some(pid) {
                    self.audio_pid = Some(pid);
                    self.codec = Some(codec);
                    self.in_pes = false;
                }
                return;
            }
            i += 5 + es_info;
        }
    }

    fn audio(&mut self, payload: &[u8], unit_start: bool, out: &mut Vec<u8>) {
        let mut data = payload;
        if unit_start {
            // PES header: 00 00 01 | stream id | length(2) | flags(2) | header length | ...
            if data.len() < 9 || data[..3] != [0, 0, 1] {
                self.in_pes = false;
                return;
            }
            self.in_pes = true;
            self.skip = 9 + data[8] as usize;
        }
        if !self.in_pes {
            return;
        }
        let skip = self.skip.min(data.len());
        self.skip -= skip;
        data = &data[skip..];
        out.extend_from_slice(data);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Wrap `audio` into a transport stream: PAT, PMT and PES packets on PID 0x101.
    pub fn mux(audio: &[u8], stream_type: u8, pes_size: usize) -> Vec<u8> {
        fn packet(pid: u16, unit_start: bool, counter: &mut u8, payload: &[u8]) -> Vec<u8> {
            let mut p = vec![SYNC_BYTE, ((unit_start as u8) << 6) | (pid >> 8) as u8, pid as u8];
            let room = PACKET_SIZE - 4;
            if payload.len() < room {
                // Stuffing through an adaptation field.
                let stuffing = room - payload.len() - 1;
                p.push(0x30 | (*counter & 0x0F));
                p.push(stuffing as u8);
                if stuffing > 0 {
                    p.push(0x00);
                    p.extend(std::iter::repeat_n(0xFF, stuffing - 1));
                }
            } else {
                p.push(0x10 | (*counter & 0x0F));
            }
            *counter = counter.wrapping_add(1);
            p.extend_from_slice(payload);
            assert_eq!(p.len(), PACKET_SIZE);
            p
        }
        fn psi(table: u8, body: &[u8]) -> Vec<u8> {
            let length = body.len() + 5 + 4;
            let mut s = vec![
                0x00,
                table,
                0xB0 | ((length >> 8) as u8),
                length as u8,
                0x00,
                0x01,
                0xC1,
                0,
                0,
            ];
            s.extend_from_slice(body);
            s.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            s
        }
        let mut counters = [0u8; 3];
        let mut ts = packet(0, true, &mut counters[0], &psi(0x00, &[0x00, 0x01, 0xF0, 0x00]));
        let pmt_body = [0xE1, 0x01, 0xF0, 0x00, stream_type, 0xE1, 0x01, 0xF0, 0x00];
        ts.extend(packet(0x1000, true, &mut counters[1], &psi(0x02, &pmt_body)));
        for chunk in audio.chunks(pes_size) {
            let mut pes = vec![0, 0, 1, 0xC0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
            pes.extend_from_slice(chunk);
            for (i, part) in pes.chunks(PACKET_SIZE - 4).enumerate() {
                ts.extend(packet(0x101, i == 0, &mut counters[2], part));
            }
        }
        ts
    }

    #[test]
    fn extracts_audio_elementary_stream() {
        let audio: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let ts = mux(&audio, 0x0F, 1_500);
        assert!(looks_like_ts(&ts));
        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        // Feed in odd-sized pieces so packets straddle calls.
        for piece in ts.chunks(1_000) {
            demuxer.push(piece, &mut out);
        }
        assert_eq!(demuxer.codec(), Some(AudioCodec::AacAdts));
        assert_eq!(out, audio);
    }

    #[test]
    fn ignores_non_audio_streams_and_garbage() {
        let video = mux(&[1, 2, 3, 4], 0x1B, 100);
        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        demuxer.push(&video, &mut out);
        assert!(out.is_empty());
        assert_eq!(demuxer.codec(), None);
        assert!(!looks_like_ts(b"ID3\x04 not a transport stream at all, just bytes"));

        let mp3 = mux(&[9; 400], 0x03, 400);
        let mut noisy = vec![0x00, 0x13, 0x37];
        noisy.extend_from_slice(&mp3);
        let mut demuxer = TsDemuxer::new();
        let mut out = Vec::new();
        demuxer.push(&noisy, &mut out);
        assert_eq!(demuxer.codec(), Some(AudioCodec::MpegAudio));
        assert_eq!(out, vec![9; 400]);
    }
}
