use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, ensure};
use tracing::info;

pub const ENC_MEDIA_STREAM: i32 = 1010;
pub const KEY_LEN: usize = 46;
const FIXED_LEN: usize = 0xd8;
const VIDEO2_EXTRA: usize = 2 * KEY_LEN;

/// UDP ports and per-stream flags for the 1010 type-1 message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaPorts {
    pub flags: u32,
    pub audio_port: u16,
    pub audio_flags: u32,
    pub video1_port: u16,
    pub video1_flags: u32,
    pub video2_port: u16,
    pub video2_flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamOffer {
    pub key_viewer_to_server: [u8; KEY_LEN],
    pub key_server_to_viewer: [u8; KEY_LEN],
    pub offer: Vec<u8>,
}

/// Client message 28.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaStreamConfig {
    pub version: u16,
    pub flags: u32,
    pub v3: u16,
    pub v4: u16,
    pub session_id: [u8; 16],
    pub audio: StreamOffer,
    pub video1: StreamOffer,
    pub video2: Option<StreamOffer>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaAnswer {
    /// Geometry used to build this answer; not part of the media wire payload.
    pub display: Option<super::AppliedDisplay>,
    pub flags: u32,
    pub audio: Vec<u8>,
    pub video1: Vec<u8>,
    pub video2: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputResize {
    pub pixels: (u16, u16),
    pub first_frame_mismatch: bool,
}

/// Implemented by the High Performance media module.
pub trait HpBackend: Send + Sync {
    fn ports(&self) -> MediaPorts;
    /// Whether a viewer at `peer` may start HP now. Checked before ServerInit to decide
    /// whether to advertise messages 28/29.
    fn available(&self, _peer: IpAddr) -> bool {
        true
    }
    /// `Ok(None)` sends no reply (screensharingd's behaviour when HP is not allowed).
    fn configure(
        &self,
        session: super::SessionId,
        peer: SocketAddr,
        config: MediaStreamConfig,
    ) -> Result<Option<MediaAnswer>>;
    /// Called only after the negotiated media answer has been written successfully.
    fn answer_sent(&self, _session: super::SessionId) {}
    /// False once media started by `session` has stopped on its own (e.g. RTCP timeout).
    fn active(&self, _session: super::SessionId) -> bool {
        true
    }
    /// Returns a stopped stream's resize once.
    fn take_output_resize(&self, _session: super::SessionId) -> Option<OutputResize> {
        None
    }
    /// Stops media started by `session`, if any. Called on every session exit.
    fn stop(&self, _session: super::SessionId) {}
    /// Client message 29, whole message. Advertising HP also advertises this message.
    fn set_display_configuration(&self, _msg: &[u8]) -> Result<()> {
        Ok(())
    }
}

/// Answers nothing; saves each message 28/29 under `dir` for offline work.
pub struct StubHpBackend {
    dir: PathBuf,
    ports: MediaPorts,
    seq: AtomicU32,
}

impl StubHpBackend {
    pub fn new(dir: PathBuf, udp_port: u16) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let ports = MediaPorts {
            flags: 0,
            audio_port: udp_port,
            audio_flags: 1,
            video1_port: udp_port,
            video1_flags: 1,
            video2_port: 0,
            video2_flags: 0,
        };
        Ok(Self {
            dir,
            ports,
            seq: AtomicU32::new(0),
        })
    }

    fn save(&self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{name}-{n}.bin"));
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

impl HpBackend for StubHpBackend {
    fn ports(&self) -> MediaPorts {
        self.ports
    }

    fn configure(
        &self,
        _session: super::SessionId,
        peer: SocketAddr,
        config: MediaStreamConfig,
    ) -> Result<Option<MediaAnswer>> {
        let path = self.save("msg28", &config.encode())?;
        info!(
            %peer,
            path = %path.display(),
            session_id = ?config.session_id,
            audio_offer = config.audio.offer.len(),
            video1_offer = config.video1.offer.len(),
            video2_offer = config.video2.as_ref().map(|s| s.offer.len()),
            "stub HP backend saved MediaStreamConfiguration; not answering"
        );
        Ok(None)
    }

    fn set_display_configuration(&self, msg: &[u8]) -> Result<()> {
        let path = self.save("msg29", msg)?;
        info!(path = %path.display(), len = msg.len(), "stub HP backend saved SetDisplayConfiguration");
        Ok(())
    }
}

impl MediaStreamConfig {
    /// `msg` is the whole message including the type byte and the u16 size.
    pub fn parse(msg: &[u8]) -> Result<Self> {
        ensure!(
            msg.len() >= 4 && msg[0] == 28,
            "not a MediaStreamConfiguration message"
        );
        let size = u16::from_be_bytes([msg[2], msg[3]]) as usize;
        ensure!(
            msg.len() == size + 4,
            "message size {size} does not match {}",
            msg.len() - 4
        );
        ensure!(
            msg.len() >= FIXED_LEN + 4,
            "MediaStreamConfiguration too short"
        );
        let be16 = |o: usize| u16::from_be_bytes([msg[o], msg[o + 1]]);
        let (a, v1, v2) = (be16(10) as usize, be16(12) as usize, be16(14) as usize);
        let min = a + v1 + if v2 != 0 { v2 + VIDEO2_EXTRA } else { 0 } + FIXED_LEN;
        ensure!(min <= size, "invalid message size {size}, need {min}");

        let stream = |at: usize, len: usize| StreamOffer {
            key_viewer_to_server: msg[at..at + KEY_LEN].try_into().unwrap(),
            key_server_to_viewer: msg[at + KEY_LEN..at + 2 * KEY_LEN].try_into().unwrap(),
            offer: msg[at + 2 * KEY_LEN..at + 2 * KEY_LEN + len].to_vec(),
        };
        let audio = stream(36, a);
        let v1_at = 36 + 2 * KEY_LEN + a;
        let video1 = stream(v1_at, v1);
        let video2 = (v2 != 0).then(|| stream(v1_at + 2 * KEY_LEN + v1, v2));
        Ok(Self {
            version: be16(4),
            flags: u32::from_be_bytes(msg[6..10].try_into().unwrap()),
            v3: be16(16),
            v4: be16(18),
            session_id: msg[20..36].try_into().unwrap(),
            audio,
            video1,
            video2,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let v2_len = self.video2.as_ref().map_or(0, |s| s.offer.len());
        let mut out = vec![28, 0, 0, 0];
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
        for len in [self.audio.offer.len(), self.video1.offer.len(), v2_len] {
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        out.extend_from_slice(&self.v3.to_be_bytes());
        out.extend_from_slice(&self.v4.to_be_bytes());
        out.extend_from_slice(&self.session_id);
        for s in [Some(&self.audio), Some(&self.video1), self.video2.as_ref()]
            .into_iter()
            .flatten()
        {
            out.extend_from_slice(&s.key_viewer_to_server);
            out.extend_from_slice(&s.key_server_to_viewer);
            out.extend_from_slice(&s.offer);
        }
        let size = (out.len() - 4) as u16;
        out[2..4].copy_from_slice(&size.to_be_bytes());
        out
    }
}

/// FramebufferUpdate with one 0x0+0+0 rect of encoding 1010 carrying `u16 size` + `body`.
fn update(body: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(&ENC_MEDIA_STREAM.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub fn ports_update(p: &MediaPorts) -> Vec<u8> {
    let mut b = Vec::with_capacity(36);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&p.flags.to_be_bytes());
    for (port, flags) in [
        (p.audio_port, p.audio_flags),
        (p.video1_port, p.video1_flags),
        (p.video2_port, p.video2_flags),
    ] {
        b.extend_from_slice(&port.to_be_bytes());
        b.extend_from_slice(&flags.to_be_bytes());
    }
    b.resize(36, 0);
    update(&b)
}

pub fn answer_update(a: &MediaAnswer) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&2u16.to_be_bytes());
    b.extend_from_slice(&2u16.to_be_bytes());
    b.extend_from_slice(&a.flags.to_be_bytes());
    for blob in [&a.audio, &a.video1, &a.video2] {
        b.extend_from_slice(&(blob.len() as u16).to_be_bytes());
    }
    b.extend_from_slice(&[0; 4]);
    for blob in [&a.audio, &a.video1, &a.video2] {
        b.extend_from_slice(blob);
    }
    update(&b)
}

pub fn error_update(error_type: u32, sub_code: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&error_type.to_be_bytes());
    b.extend_from_slice(&sub_code.to_be_bytes());
    update(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(seed: u8, len: usize) -> StreamOffer {
        StreamOffer {
            key_viewer_to_server: [seed; KEY_LEN],
            key_server_to_viewer: [seed + 1; KEY_LEN],
            offer: vec![seed + 2; len],
        }
    }

    fn config(video2: bool) -> MediaStreamConfig {
        MediaStreamConfig {
            version: 3,
            flags: 0b101,
            v3: 0,
            v4: 0,
            session_id: [0xab; 16],
            audio: offer(1, 40),
            video1: offer(10, 300),
            video2: video2.then(|| offer(20, 7)),
        }
    }

    #[test]
    fn msg28_layout_matches_client_builder() {
        let c = config(false);
        let msg = c.encode();
        assert_eq!(msg.len(), 0xdc + 40 + 300);
        assert_eq!(
            &msg[..4],
            &[28, 0, ((msg.len() - 4) >> 8) as u8, (msg.len() - 4) as u8]
        );
        assert_eq!(&msg[20..36], &[0xab; 16]);
        assert_eq!(msg[36], 1);
        assert_eq!(msg[82], 2);
        assert_eq!(msg[128], 3);
        assert_eq!(msg[128 + 40], 10);
        assert_eq!(msg[220 + 40], 12);
        assert_eq!(MediaStreamConfig::parse(&msg).unwrap(), c);

        let c2 = config(true);
        let msg = c2.encode();
        assert_eq!(msg.len(), 0xdc + 40 + 300 + 0x5c + 7);
        assert_eq!(MediaStreamConfig::parse(&msg).unwrap(), c2);
    }

    #[test]
    fn msg28_rejects_bad_sizes() {
        let mut msg = config(false).encode();
        msg[13] = msg[13].wrapping_add(1);
        assert!(MediaStreamConfig::parse(&msg).is_err());
        assert!(MediaStreamConfig::parse(&msg[..100]).is_err());
    }

    #[test]
    fn msg28_larger_size_than_needed_is_accepted() {
        let mut msg = config(false).encode();
        msg.extend_from_slice(&[0; 5]);
        let size = (msg.len() - 4) as u16;
        msg[2..4].copy_from_slice(&size.to_be_bytes());
        assert_eq!(MediaStreamConfig::parse(&msg).unwrap(), config(false));
    }

    #[test]
    fn ports_layout() {
        let msg = ports_update(&MediaPorts {
            flags: 0,
            audio_port: 5900,
            audio_flags: 1,
            video1_port: 5900,
            video1_flags: 3,
            video2_port: 0,
            video2_flags: 0,
        });
        assert_eq!(&msg[..4], &[0, 0, 0, 1]);
        assert_eq!(&msg[4..12], &[0; 8]);
        assert_eq!(i32::from_be_bytes(msg[12..16].try_into().unwrap()), 1010);
        let body = &msg[18..];
        assert_eq!(u16::from_be_bytes([msg[16], msg[17]]) as usize, body.len());
        assert_eq!(body.len(), 36);
        assert_eq!(&body[0..2], &[0, 1]);
        assert_eq!(&body[8..10], &5900u16.to_be_bytes());
        assert_eq!(&body[10..14], &1u32.to_be_bytes());
        assert_eq!(&body[14..16], &5900u16.to_be_bytes());
        assert_eq!(&body[16..20], &3u32.to_be_bytes());
    }

    #[test]
    fn answer_layout_matches_screensharingd() {
        let msg = answer_update(&MediaAnswer {
            display: None,
            flags: 1,
            audio: vec![0xa; 3],
            video1: vec![0xb; 5],
            video2: vec![],
        });
        let size = u16::from_be_bytes([msg[16], msg[17]]) as usize;
        let body = &msg[18..];
        assert_eq!(size, body.len());
        assert_eq!(size, 18 + 8);
        assert_eq!(&body[0..4], &[0, 2, 0, 2]);
        assert_eq!(&body[4..8], &1u32.to_be_bytes());
        assert_eq!(&body[8..14], &[0, 3, 0, 5, 0, 0]);
        assert_eq!(&body[18..21], &[0xa; 3]);
        assert_eq!(&body[21..26], &[0xb; 5]);
    }

    #[test]
    fn error_layout_matches_screensharingd() {
        let msg = error_update(2, 0);
        assert_eq!(
            &msg[16..],
            &[0, 16, 0, 3, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0]
        );
    }
}
