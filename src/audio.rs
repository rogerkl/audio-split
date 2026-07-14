use std::path::{Path, PathBuf};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Number of frames aggregated into one waveform peak bin.
pub const PEAK_BIN: usize = 512;

impl std::fmt::Debug for AudioData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioData")
            .field("path", &self.path)
            .field("channels", &self.channels)
            .field("sample_rate", &self.sample_rate)
            .field("frames", &self.frames())
            .finish()
    }
}

pub struct AudioData {
    pub path: PathBuf,
    /// Interleaved samples, normalized to [-1.0, 1.0].
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    /// Per channel: (min, max) per PEAK_BIN frames.
    pub peaks: Vec<Vec<(f32, f32)>>,
}

impl AudioData {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels
        }
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn load(path: &Path) -> Result<AudioData, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {e}"))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "No audio track found".to_string())?;
    let track_id = track.id;
    let params = track.codec_params.clone();

    let sample_rate = params
        .sample_rate
        .ok_or_else(|| "Unknown sample rate".to_string())?;
    let channels = params
        .channels
        .ok_or_else(|| "Unknown channel layout".to_string())?
        .count();
    let bits_per_sample = params.bits_per_sample.unwrap_or(16);

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| format!("Cannot create decoder: {e}"))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("Read error: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                if sample_buf.is_none() {
                    sample_buf = Some(SampleBuffer::new(
                        decoded.capacity() as u64,
                        *decoded.spec(),
                    ));
                }
                let buf = sample_buf.as_mut().unwrap();
                buf.copy_interleaved_ref(decoded);
                samples.extend_from_slice(buf.samples());
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decode error: {e}")),
        }
    }

    if samples.is_empty() {
        return Err("File contains no audio".to_string());
    }

    let peaks = compute_peaks(&samples, channels);

    Ok(AudioData {
        path: path.to_path_buf(),
        samples,
        channels,
        sample_rate,
        bits_per_sample,
        peaks,
    })
}

fn compute_peaks(samples: &[f32], channels: usize) -> Vec<Vec<(f32, f32)>> {
    let frames = samples.len() / channels;
    let n_bins = frames.div_ceil(PEAK_BIN);
    let mut peaks = vec![Vec::with_capacity(n_bins); channels];

    for bin in 0..n_bins {
        let start = bin * PEAK_BIN;
        let end = ((bin + 1) * PEAK_BIN).min(frames);
        for (ch, channel_peaks) in peaks.iter_mut().enumerate() {
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            for frame in start..end {
                let s = samples[frame * channels + ch];
                min = min.min(s);
                max = max.max(s);
            }
            channel_peaks.push((min, max));
        }
    }
    peaks
}

pub fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let h = (secs / 3600.0) as u64;
    let m = ((secs / 60.0) as u64) % 60;
    let s = (secs as u64) % 60;
    let ms = ((secs - secs.floor()) * 1000.0) as u64;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{m:02}:{s:02}.{ms:03}")
    }
}
