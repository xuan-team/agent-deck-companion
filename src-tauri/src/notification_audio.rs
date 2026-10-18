//! Notification clips owned by the companion, cached in device RAM.
use serde::Serialize;
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub const RATE: usize = 16_000;
pub const MAX_PCM: usize = 96_000;
pub const CMD_CAPS: u8 = 24;
pub const CMD_BEGIN: u8 = 25;
pub const CMD_DATA: u8 = 26;
pub const CMD_COMMIT: u8 = 27;
pub const CMD_PLAY: u8 = 28;

pub fn data_body(
    slot: u8,
    offset: usize,
    chunk: &[u8],
    acknowledge: bool,
) -> Result<[u8; 53], String> {
    if slot > 2
        || chunk.is_empty()
        || chunk.len() > 46
        || offset
            .checked_add(chunk.len())
            .is_none_or(|end| end > MAX_PCM)
    {
        return Err("invalid audio fragment".into());
    }
    let mut body = [0; 53];
    body[0] = slot;
    body[1..5].copy_from_slice(&(offset as u32).to_le_bytes());
    body[5] = chunk.len() as u8;
    body[6] = u8::from(acknowledge);
    body[7..7 + chunk.len()].copy_from_slice(chunk);
    Ok(body)
}

#[derive(Clone)]
pub struct Clip {
    pub name: String,
    pub pcm: Arc<Vec<u8>>,
    pub crc: u32,
}

#[derive(Serialize)]
pub struct ClipView {
    pub slot: u8,
    pub name: String,
    pub duration_ms: usize,
    pub bytes: usize,
    pub synced: bool,
}

#[derive(Serialize)]
pub struct AudioView {
    pub status: String,
    pub detail: String,
    pub progress_percent: u8,
    pub clips: Vec<ClipView>,
}

pub struct AudioRuntime {
    pub clips: Mutex<Vec<Clip>>,
    pub generation: AtomicU64,
    pub play: Mutex<Option<u8>>,
    pub ack: Mutex<Option<std::sync::mpsc::Sender<(u8, u16, u8)>>>,
    /// Set by the audio worker. A preview can be requested at any time; the
    /// device falls back to its built-in SPIFFS clips for slots not yet cached.
    pub connected: AtomicBool,
    pub result: Mutex<(String, String, u8, [Option<u32>; 3])>,
}

fn directory() -> Result<PathBuf, String> {
    crate::profile::config_path()
        .and_then(|p| p.parent().map(|p| p.join("notification-audio")))
        .ok_or_else(|| "configuration directory unavailable".into())
}

fn default_clip(slot: usize) -> Clip {
    let (name, wav): (&str, &[u8]) = match slot {
        0 => (
            "Default completion",
            include_bytes!("../assets/audio/complete.wav"),
        ),
        1 => (
            "Default needs input",
            include_bytes!("../assets/audio/needs_input.wav"),
        ),
        _ => ("Default error", include_bytes!("../assets/audio/error.wav")),
    };
    let pcm = wav_to_pcm(wav).expect("bundled notification WAV must be valid");
    Clip {
        name: name.into(),
        crc: crc32(&pcm),
        pcm: Arc::new(pcm),
    }
}

impl AudioRuntime {
    pub fn new() -> Self {
        let clips = (0..3)
            .map(|slot| {
                if let Ok(root) = directory() {
                    if let Ok(pcm) = fs::read(root.join(format!("{slot}.pcm"))) {
                        if !pcm.is_empty() && pcm.len() <= MAX_PCM && pcm.len() % 2 == 0 {
                            let name = fs::read_to_string(root.join(format!("{slot}.txt")))
                                .unwrap_or_else(|_| "Custom WAV".into());
                            return Clip {
                                name,
                                crc: crc32(&pcm),
                                pcm: Arc::new(pcm),
                            };
                        }
                    }
                }
                default_clip(slot)
            })
            .collect();
        Self {
            clips: Mutex::new(clips),
            generation: AtomicU64::new(1),
            play: Mutex::new(None),
            ack: Mutex::new(None),
            connected: AtomicBool::new(false),
            result: Mutex::new(("disconnected".into(), String::new(), 0, [None; 3])),
        }
    }

    pub fn view(&self) -> AudioView {
        let clips = self.clips.lock().unwrap();
        let result = self.result.lock().unwrap();
        AudioView {
            status: result.0.clone(),
            detail: result.1.clone(),
            progress_percent: result.2,
            clips: clips
                .iter()
                .enumerate()
                .map(|(slot, clip)| ClipView {
                    slot: slot as u8,
                    name: clip.name.clone(),
                    duration_ms: clip.pcm.len() * 1000 / (RATE * 2),
                    bytes: clip.pcm.len(),
                    synced: result.3[slot] == Some(clip.crc),
                })
                .collect(),
        }
    }

    pub fn set(&self, slot: u8, name: String, wav: Option<Vec<u8>>) -> Result<(), String> {
        if slot > 2 {
            return Err("unknown notification slot".into());
        }
        let clip = if let Some(wav) = wav {
            let pcm = wav_to_pcm(&wav)?;
            Clip {
                name: name.chars().take(120).collect(),
                crc: crc32(&pcm),
                pcm: Arc::new(pcm),
            }
        } else {
            default_clip(slot as usize)
        };
        let root = directory()?;
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        // The PCM is the authoritative asset; replace it atomically after validation.
        let temp = root.join(format!("{slot}.pcm.tmp"));
        let mut clips = self.clips.lock().unwrap();
        fs::write(&temp, clip.pcm.as_slice()).map_err(|e| e.to_string())?;
        fs::write(root.join(format!("{slot}.txt")), &clip.name).map_err(|e| e.to_string())?;
        fs::rename(temp, root.join(format!("{slot}.pcm"))).map_err(|e| e.to_string())?;
        clips[slot as usize] = clip;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// PCM16 WAV only. Decode stereo and resample on the host to keep device code small.
pub fn wav_to_pcm(wav: &[u8]) -> Result<Vec<u8>, String> {
    let invalid = || "请选择不超过 3 秒的 PCM 16-bit WAV 音频".to_string();
    if wav.len() > 4_000_000 || wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(invalid());
    }
    let u16_at = |p: usize| u16::from_le_bytes([wav[p], wav[p + 1]]);
    let u32_at = |p: usize| u32::from_le_bytes(wav[p..p + 4].try_into().unwrap()) as usize;
    let end = u32_at(4)
        .checked_add(8)
        .filter(|&n| n <= wav.len())
        .ok_or_else(invalid)?;
    let mut pos = 12;
    let mut format = None;
    let mut data = None;
    while pos + 8 <= end {
        let size = u32_at(pos + 4);
        let start = pos + 8;
        let stop = start
            .checked_add(size)
            .filter(|&n| n <= end)
            .ok_or_else(invalid)?;
        match &wav[pos..pos + 4] {
            b"fmt " if size >= 16 => {
                if u16_at(start) != 1 || u16_at(start + 14) != 16 {
                    return Err(invalid());
                }
                let channels = u16_at(start + 2) as usize;
                let rate = u32_at(start + 4);
                if !(channels == 1 || channels == 2)
                    || !(8000..=192000).contains(&rate)
                    || u16_at(start + 12) as usize != channels * 2
                {
                    return Err(invalid());
                }
                format = Some((channels, rate));
            }
            b"data" => data = Some(&wav[start..stop]),
            _ => {}
        }
        pos = stop.checked_add(size & 1).ok_or_else(invalid)?;
    }
    let (channels, rate) = format.ok_or_else(invalid)?;
    let data = data.ok_or_else(invalid)?;
    let frames = data.len() / (channels * 2);
    if frames == 0 || data.len() % (channels * 2) != 0 || frames > rate * 3 {
        return Err(invalid());
    }
    let samples = frames * RATE / rate;
    if samples == 0 || samples * 2 > MAX_PCM {
        return Err(invalid());
    }
    let mono = |index: usize| -> f64 {
        (0..channels)
            .map(|ch| {
                let p = (index.min(frames - 1) * channels + ch) * 2;
                i16::from_le_bytes([data[p], data[p + 1]]) as f64
            })
            .sum::<f64>()
            / channels as f64
    };
    let mut pcm = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let source = i as f64 * rate as f64 / RATE as f64;
        let index = source as usize;
        let value = mono(index) + (mono(index + 1) - mono(index)) * source.fract();
        pcm.extend_from_slice(
            &(value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16).to_le_bytes(),
        );
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_fragments_preserve_offset_length_and_ack_boundary() {
        let data = vec![0x9a; MAX_PCM];
        let mut reconstructed: Vec<u8> = Vec::new();
        for (batch, bytes) in data.chunks(46 * 8).enumerate() {
            let count = bytes.len().div_ceil(46);
            for (index, chunk) in bytes.chunks(46).enumerate() {
                let offset = batch * 46 * 8 + index * 46;
                let body = data_body(2, offset, chunk, index + 1 == count).unwrap();
                assert_eq!(body[0], 2);
                assert_eq!(
                    u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize,
                    reconstructed.len()
                );
                assert_eq!(body[6], u8::from(index + 1 == count));
                reconstructed.extend_from_slice(&body[7..7 + body[5] as usize]);
            }
        }
        assert_eq!(reconstructed, data);
        assert!(data_body(0, MAX_PCM, &[1, 2], false).is_err());
        assert!(data_body(0, 0, &[0; 47], false).is_err());
        assert!(data_body(3, 0, &[0; 2], false).is_err());
    }
    #[test]
    fn defaults_fit_and_crc_matches_standard() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        for slot in 0..3 {
            let clip = default_clip(slot);
            assert!(!clip.pcm.is_empty());
            assert!(clip.pcm.len() <= MAX_PCM);
        }
    }
    #[test]
    fn rejects_invalid_truncated_and_oversized_wavs() {
        assert!(wav_to_pcm(b"not a wav").is_err());
        let wav = include_bytes!("../assets/audio/complete.wav");
        assert!(wav_to_pcm(&wav[..wav.len() - 1]).is_err());
        let mut bad = wav.to_vec();
        bad[20] = 3;
        assert!(wav_to_pcm(&bad).is_err());
    }
    #[test]
    fn resamples_and_downmixes() {
        let mut wav = Vec::from(&b"RIFF"[..]);
        wav.extend(44u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(32000u32.to_le_bytes());
        wav.extend(4u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(8u32.to_le_bytes());
        for sample in [100i16, 300, 500, 700] {
            wav.extend(sample.to_le_bytes());
        }
        let pcm = wav_to_pcm(&wav).unwrap();
        let values: Vec<_> = pcm
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]))
            .collect();
        assert_eq!(values, [200, 400, 600, 600]);
    }
}
