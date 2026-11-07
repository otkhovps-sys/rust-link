//! RTP packetization and Discord transport encryption
//! (`aead_aes256_gcm_rtpsize`, `aead_xchacha20_poly1305_rtpsize`).
//!
//! Packet layout: `RTP header (12, AAD) | ciphertext | tag (16) | nonce counter (4, BE)`.
//! The AEAD nonce is the 32-bit counter in its first four bytes followed by zeros.

use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes256Gcm,
};
use anyhow::{anyhow, bail, ensure};
use chacha20poly1305::XChaCha20Poly1305;
use rand::Rng;

pub const RTP_HEADER_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const NONCE_SUFFIX_LEN: usize = 4;
const RTP_VERSION_FLAGS: u8 = 0x80;
const RTP_PAYLOAD_TYPE_OPUS: u8 = 0x78;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    AeadAes256GcmRtpSize,
    AeadXChaCha20Poly1305RtpSize,
}

impl EncryptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AeadAes256GcmRtpSize => "aead_aes256_gcm_rtpsize",
            Self::AeadXChaCha20Poly1305RtpSize => "aead_xchacha20_poly1305_rtpsize",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aead_aes256_gcm_rtpsize" => Some(Self::AeadAes256GcmRtpSize),
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305RtpSize),
            _ => None,
        }
    }

    /// Pick a mode offered by the voice server: the configured one when available,
    /// otherwise AES-GCM (preferred by Discord), otherwise XChaCha20-Poly1305 (mandatory).
    pub fn negotiate(offered: &[String], preference: &str) -> Option<Self> {
        let offered_mode = |m: Self| offered.iter().any(|o| o == m.as_str());
        if let Some(preferred) = Self::parse(preference) {
            if offered_mode(preferred) {
                return Some(preferred);
            }
        }
        [Self::AeadAes256GcmRtpSize, Self::AeadXChaCha20Poly1305RtpSize]
            .into_iter()
            .find(|m| offered_mode(*m))
    }
}

enum Cipher {
    Aes(Box<Aes256Gcm>),
    XChaCha(Box<XChaCha20Poly1305>),
}

impl Cipher {
    fn new(mode: EncryptionMode, key: &[u8]) -> anyhow::Result<Self> {
        ensure!(key.len() == 32, "voice secret key must be 32 bytes, got {}", key.len());
        Ok(match mode {
            EncryptionMode::AeadAes256GcmRtpSize => Cipher::Aes(Box::new(
                Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("bad key"))?,
            )),
            EncryptionMode::AeadXChaCha20Poly1305RtpSize => Cipher::XChaCha(Box::new(
                XChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow!("bad key"))?,
            )),
        })
    }

    fn seal(&self, counter: u32, aad: &[u8], buffer: &mut [u8]) -> anyhow::Result<[u8; TAG_LEN]> {
        let mut tag = [0u8; TAG_LEN];
        match self {
            Cipher::Aes(c) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                let t = c
                    .encrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), aad, buffer)
                    .map_err(|_| anyhow!("aes-gcm encryption failed"))?;
                tag.copy_from_slice(&t);
            }
            Cipher::XChaCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                let t = c
                    .encrypt_in_place_detached(chacha20poly1305::XNonce::from_slice(&nonce), aad, buffer)
                    .map_err(|_| anyhow!("xchacha20-poly1305 encryption failed"))?;
                tag.copy_from_slice(&t);
            }
        }
        Ok(tag)
    }

    fn open(&self, counter: u32, aad: &[u8], buffer: &mut [u8], tag: &[u8]) -> anyhow::Result<()> {
        match self {
            Cipher::Aes(c) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                c.decrypt_in_place_detached(
                    aes_gcm::Nonce::from_slice(&nonce),
                    aad,
                    buffer,
                    aes_gcm::Tag::from_slice(tag),
                )
                .map_err(|_| anyhow!("aes-gcm authentication failed"))
            }
            Cipher::XChaCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter.to_be_bytes());
                c.decrypt_in_place_detached(
                    chacha20poly1305::XNonce::from_slice(&nonce),
                    aad,
                    buffer,
                    chacha20poly1305::Tag::from_slice(tag),
                )
                .map_err(|_| anyhow!("xchacha20-poly1305 authentication failed"))
            }
        }
    }
}

/// Stateful RTP sender: sequence, timestamp and nonce counters for one SSRC.
pub struct RtpPacketizer {
    cipher: Cipher,
    mode: EncryptionMode,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    nonce: u32,
}

impl RtpPacketizer {
    pub fn new(mode: EncryptionMode, key: &[u8], ssrc: u32) -> anyhow::Result<Self> {
        let mut rng = rand::thread_rng();
        Ok(Self {
            cipher: Cipher::new(mode, key)?,
            mode,
            ssrc,
            sequence: rng.gen(),
            timestamp: rng.gen(),
            nonce: 0,
        })
    }

    pub fn mode(&self) -> EncryptionMode {
        self.mode
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Build one encrypted RTP packet carrying a 20 ms Opus payload into `out`.
    pub fn packetize(&mut self, payload: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
        out.clear();
        out.reserve(RTP_HEADER_LEN + payload.len() + TAG_LEN + NONCE_SUFFIX_LEN);
        out.push(RTP_VERSION_FLAGS);
        out.push(RTP_PAYLOAD_TYPE_OPUS);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(payload);

        let counter = self.nonce;
        self.nonce = self.nonce.wrapping_add(1);
        let (header, body) = out.split_at_mut(RTP_HEADER_LEN);
        let tag = self.cipher.seal(counter, header, body)?;
        out.extend_from_slice(&tag);
        out.extend_from_slice(&counter.to_be_bytes());

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self
            .timestamp
            .wrapping_add(crate::audio::encoder::SAMPLES_PER_FRAME as u32);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

/// Receiver side of [`RtpPacketizer`] (used by tests and the mock voice server).
pub fn decrypt_packet(mode: EncryptionMode, key: &[u8], packet: &[u8]) -> anyhow::Result<(RtpHeader, Vec<u8>)> {
    if packet.len() < RTP_HEADER_LEN + TAG_LEN + NONCE_SUFFIX_LEN {
        bail!("packet too short");
    }
    ensure!(packet[0] & 0xC0 == 0x80, "not an RTP v2 packet");
    let header = RtpHeader {
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes(packet[4..8].try_into().unwrap()),
        ssrc: u32::from_be_bytes(packet[8..12].try_into().unwrap()),
    };
    let nonce_start = packet.len() - NONCE_SUFFIX_LEN;
    let tag_start = nonce_start - TAG_LEN;
    let counter = u32::from_be_bytes(packet[nonce_start..].try_into().unwrap());
    let mut body = packet[RTP_HEADER_LEN..tag_start].to_vec();
    Cipher::new(mode, key)?.open(
        counter,
        &packet[..RTP_HEADER_LEN],
        &mut body,
        &packet[tag_start..nonce_start],
    )?;
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_prefers_configured_then_aes() {
        let offered = vec![
            "aead_xchacha20_poly1305_rtpsize".to_string(),
            "aead_aes256_gcm_rtpsize".to_string(),
            "xsalsa20_poly1305".to_string(),
        ];
        assert_eq!(
            EncryptionMode::negotiate(&offered, "auto"),
            Some(EncryptionMode::AeadAes256GcmRtpSize)
        );
        assert_eq!(
            EncryptionMode::negotiate(&offered, "aead_xchacha20_poly1305_rtpsize"),
            Some(EncryptionMode::AeadXChaCha20Poly1305RtpSize)
        );
        assert_eq!(EncryptionMode::negotiate(&["xsalsa20_poly1305".into()], "auto"), None);
    }

    #[test]
    fn roundtrip_both_modes() {
        let key = [7u8; 32];
        for mode in [
            EncryptionMode::AeadAes256GcmRtpSize,
            EncryptionMode::AeadXChaCha20Poly1305RtpSize,
        ] {
            let mut packetizer = RtpPacketizer::new(mode, &key, 0xDEADBEEF).unwrap();
            let mut out = Vec::new();
            let mut last: Option<RtpHeader> = None;
            for i in 0..3u8 {
                let payload = [i, 1, 2, 3, 4];
                packetizer.packetize(&payload, &mut out).unwrap();
                assert_eq!(out.len(), RTP_HEADER_LEN + payload.len() + TAG_LEN + NONCE_SUFFIX_LEN);
                assert_eq!(&out[out.len() - 4..], &(i as u32).to_be_bytes());
                let (header, plain) = decrypt_packet(mode, &key, &out).unwrap();
                assert_eq!(plain, payload);
                assert_eq!(header.ssrc, 0xDEADBEEF);
                if let Some(prev) = last {
                    assert_eq!(header.sequence, prev.sequence.wrapping_add(1));
                    assert_eq!(header.timestamp, prev.timestamp.wrapping_add(960));
                }
                last = Some(header);
            }
            // Tampering with the header (AAD) must fail authentication.
            out[2] ^= 1;
            assert!(decrypt_packet(mode, &key, &out).is_err());
        }
    }
}
