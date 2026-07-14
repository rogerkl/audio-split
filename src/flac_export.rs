use std::path::Path;
use std::sync::Arc;

use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::error::Verify;

use crate::audio::AudioData;

pub struct ExportTrack {
    pub start_frame: usize,
    pub end_frame: usize,
    pub title: String,
    pub artist: String,
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Splits the audio into one FLAC file per track, with vorbis-comment tags.
/// Returns the number of files written.
pub fn export_tracks(
    audio: &Arc<AudioData>,
    dir: &Path,
    album_artist: &str,
    album_title: &str,
    tracks: &[ExportTrack],
) -> Result<usize, String> {
    let bits = audio.bits_per_sample.clamp(16, 24);
    let scale = (1i64 << (bits - 1)) as f32;
    let max_val = (1i64 << (bits - 1)) as f32 - 1.0;
    let total = tracks.len();

    for (i, track) in tracks.iter().enumerate() {
        let number = i + 1;
        let title = if track.title.is_empty() {
            format!("Track {number:02}")
        } else {
            track.title.clone()
        };
        let artist = if track.artist.is_empty() {
            album_artist.to_string()
        } else {
            track.artist.clone()
        };

        let begin = track.start_frame * audio.channels;
        let end = (track.end_frame * audio.channels).min(audio.samples.len());
        if begin >= end {
            continue;
        }

        let samples: Vec<i32> = audio.samples[begin..end]
            .iter()
            .map(|&s| (s * scale).round().clamp(-scale, max_val) as i32)
            .collect();

        let file_name = if artist.is_empty() {
            format!("{number:02} - {}.flac", sanitize_filename(&title))
        } else {
            format!(
                "{number:02} - {} - {}.flac",
                sanitize_filename(&artist),
                sanitize_filename(&title)
            )
        };
        let path = dir.join(file_name);

        encode_flac(
            &samples,
            audio.channels,
            bits as usize,
            audio.sample_rate as usize,
            &path,
        )?;
        tag_flac(
            &path,
            &title,
            &artist,
            album_artist,
            album_title,
            number as u32,
            total as u32,
        )?;
    }

    Ok(total)
}

/// Encodes the whole recording into a single FLAC file with the cue sheet
/// embedded as a CUESHEET vorbis comment (the convention used by foobar2000,
/// DeaDBeeF and friends), plus album-level tags.
pub fn export_single_with_cue(
    audio: &Arc<AudioData>,
    path: &Path,
    album_artist: &str,
    album_title: &str,
    tracks: &[ExportTrack],
) -> Result<(), String> {
    let bits = audio.bits_per_sample.clamp(16, 24);
    let scale = (1i64 << (bits - 1)) as f32;
    let max_val = (1i64 << (bits - 1)) as f32 - 1.0;

    let samples: Vec<i32> = audio
        .samples
        .iter()
        .map(|&s| (s * scale).round().clamp(-scale, max_val) as i32)
        .collect();

    encode_flac(
        &samples,
        audio.channels,
        bits as usize,
        audio.sample_rate as usize,
        path,
    )?;
    drop(samples);

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sr = audio.sample_rate as f64;
    let cue_tracks: Vec<crate::cue::CueTrack> = tracks
        .iter()
        .map(|t| crate::cue::CueTrack {
            start_secs: t.start_frame as f64 / sr,
            title: t.title.clone(),
            artist: t.artist.clone(),
        })
        .collect();
    let cue = crate::cue::cue_text(&file_name, album_artist, album_title, &cue_tracks);

    let mut tag = metaflac::Tag::read_from_path(path)
        .map_err(|e| format!("Cannot read back {path:?} for tagging: {e}"))?;
    {
        let comments = tag.vorbis_comments_mut();
        if !album_title.is_empty() {
            comments.set_album(vec![album_title.to_string()]);
            comments.set_title(vec![album_title.to_string()]);
        }
        if !album_artist.is_empty() {
            comments.set_artist(vec![album_artist.to_string()]);
            comments.set("ALBUMARTIST", vec![album_artist.to_string()]);
        }
        comments.set("CUESHEET", vec![cue]);
    }
    tag.save()
        .map_err(|e| format!("Cannot write tags to {path:?}: {e}"))
}

fn encode_flac(
    samples: &[i32],
    channels: usize,
    bits: usize,
    sample_rate: usize,
    path: &Path,
) -> Result<(), String> {
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| format!("Encoder config error: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(samples, channels, bits, sample_rate);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| format!("FLAC encoding failed: {e:?}"))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| format!("FLAC serialization failed: {e:?}"))?;

    // flacenc records the (shorter) final block in STREAMINFO's min_blocksize,
    // but the FLAC spec excludes the last block: a fixed-blocksize stream must
    // have min == max, and strict decoders (e.g. symphonia) reject the frames
    // otherwise. Patch min_blocksize (bytes 8..10) to max_blocksize (10..12).
    let mut bytes = sink.as_slice().to_vec();
    if bytes.len() > 12 && &bytes[0..4] == b"fLaC" {
        let (min_bs, max_bs) = bytes.split_at_mut(10);
        min_bs[8..10].copy_from_slice(&max_bs[..2]);
    }

    std::fs::write(path, bytes).map_err(|e| format!("Cannot write {path:?}: {e}"))
}

fn tag_flac(
    path: &Path,
    title: &str,
    artist: &str,
    album_artist: &str,
    album_title: &str,
    number: u32,
    total: u32,
) -> Result<(), String> {
    let mut tag = metaflac::Tag::read_from_path(path)
        .map_err(|e| format!("Cannot read back {path:?} for tagging: {e}"))?;
    {
        let comments = tag.vorbis_comments_mut();
        comments.set_title(vec![title.to_string()]);
        if !artist.is_empty() {
            comments.set_artist(vec![artist.to_string()]);
        }
        if !album_title.is_empty() {
            comments.set_album(vec![album_title.to_string()]);
        }
        if !album_artist.is_empty() {
            comments.set("ALBUMARTIST", vec![album_artist.to_string()]);
        }
        comments.set_track(number);
        comments.set_total_tracks(total);
    }
    tag.save()
        .map_err(|e| format!("Cannot write tags to {path:?}: {e}"))
}
