use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::{audio, cue, detect, flac_export};

/// Writes a 16-bit stereo PCM WAV file from normalized f32 samples.
fn write_wav(path: &Path, samples: &[f32], sample_rate: u32, channels: u16) {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;

    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sample_rate.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for &s in samples {
        let v = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
        f.write_all(&v.to_le_bytes()).unwrap();
    }
}

/// Three 20 s tone "tracks" separated by 2 s of silence, 44.1 kHz stereo.
fn make_test_audio(dir: &Path) -> std::path::PathBuf {
    let sr = 44100u32;
    let mut samples: Vec<f32> = Vec::new();
    let tone_secs = 20;
    let gap_secs = 2;
    for track in 0..3 {
        if track > 0 {
            samples.extend(std::iter::repeat(0.0).take((sr as usize * gap_secs) * 2));
        }
        let freq = 220.0 * (track + 1) as f32;
        for i in 0..(sr as usize * tone_secs) {
            let t = i as f32 / sr as f32;
            let v = 0.5 * (2.0 * std::f32::consts::PI * freq * t).sin();
            samples.push(v);
            samples.push(v);
        }
    }
    let path = dir.join("test_album.wav");
    write_wav(&path, &samples, sr, 2);
    path
}

#[test]
fn end_to_end() {
    let dir = std::env::temp_dir().join("audio_split_test");
    std::fs::create_dir_all(&dir).unwrap();
    let wav_path = make_test_audio(&dir);

    // Load.
    let audio = audio::load(&wav_path).expect("load wav");
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.sample_rate, 44100);
    let expected_frames = 44100 * (3 * 20 + 2 * 2);
    assert_eq!(audio.frames(), expected_frames);
    assert!(!audio.peaks[0].is_empty());

    // Detect: should find exactly the 3 track starts.
    let starts = detect::detect_track_starts(&audio, &detect::DetectParams::default());
    assert_eq!(starts.len(), 3, "expected 3 tracks, got {starts:?}");
    assert_eq!(starts[0], 0);
    // Track 2 starts at 22 s (20 s tone + 2 s gap), minus the 50 ms backoff.
    let t2 = starts[1] as f64 / 44100.0;
    assert!((t2 - 22.0).abs() < 0.2, "track 2 at {t2}");
    let t3 = starts[2] as f64 / 44100.0;
    assert!((t3 - 44.0).abs() < 0.2, "track 3 at {t3}");

    // CUE export.
    let cue_path = dir.join("test_album.cue");
    let tracks: Vec<cue::CueTrack> = starts
        .iter()
        .enumerate()
        .map(|(i, &pos)| cue::CueTrack {
            start_secs: pos as f64 / 44100.0,
            title: format!("Song {}", i + 1),
            artist: "Tester".to_string(),
        })
        .collect();
    cue::write_cue(&cue_path, "test_album.wav", "Tester", "Test Album", &tracks).unwrap();
    let cue_text = std::fs::read_to_string(&cue_path).unwrap();
    assert!(cue_text.contains("FILE \"test_album.wav\" WAVE"));
    assert!(cue_text.contains("TRACK 01 AUDIO"));
    assert!(cue_text.contains("TRACK 03 AUDIO"));
    assert!(cue_text.contains("INDEX 01 00:00:00"));
    // 21.95 s (22 s minus 50 ms backoff) ≈ 21 s + 71 frames.
    assert!(
        cue_text.contains("INDEX 01 00:21:7"),
        "cue times wrong:\n{cue_text}"
    );

    // FLAC export.
    let out_dir = dir.join("flac_out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let audio = Arc::new(audio);
    let export_tracks: Vec<flac_export::ExportTrack> = starts
        .iter()
        .enumerate()
        .map(|(i, &pos)| flac_export::ExportTrack {
            start_frame: pos,
            end_frame: starts.get(i + 1).copied().unwrap_or(audio.frames()),
            title: format!("Song {}", i + 1),
            artist: "Tester".to_string(),
        })
        .collect();
    let n = flac_export::export_tracks(&audio, &out_dir, "Tester", "Test Album", &export_tracks)
        .expect("flac export");
    assert_eq!(n, 3);

    // Each FLAC must decode to the right length and carry the right tags.
    for (i, track) in export_tracks.iter().enumerate() {
        let path = out_dir.join(format!("{:02} - Tester - Song {}.flac", i + 1, i + 1));
        assert!(path.exists(), "missing {path:?}");

        let decoded = audio::load(&path).expect("re-load exported flac");
        assert_eq!(decoded.frames(), track.end_frame - track.start_frame);

        let tag = metaflac::Tag::read_from_path(&path).unwrap();
        let comments = tag.vorbis_comments().unwrap();
        assert_eq!(comments.title().unwrap()[0], format!("Song {}", i + 1));
        assert_eq!(comments.album().unwrap()[0], "Test Album");
        assert_eq!(comments.track().unwrap(), (i + 1) as u32);
    }

    // Single FLAC with embedded cue sheet.
    let single_path = dir.join("album_with_cue.flac");
    flac_export::export_single_with_cue(
        &audio,
        &single_path,
        "Tester",
        "Test Album",
        &export_tracks,
    )
    .expect("flac+cue export");

    let decoded = audio::load(&single_path).expect("re-load flac+cue");
    assert_eq!(decoded.frames(), audio.frames());

    let tag = metaflac::Tag::read_from_path(&single_path).unwrap();
    let comments = tag.vorbis_comments().unwrap();
    assert_eq!(comments.album().unwrap()[0], "Test Album");
    let embedded = &comments.get("CUESHEET").unwrap()[0];
    assert!(embedded.contains("FILE \"album_with_cue.flac\" WAVE"));
    assert!(embedded.contains("TRACK 03 AUDIO"));
    assert!(embedded.contains("TITLE \"Song 2\""));

    // The embedded cue sheet must read back like an opened .cue file.
    let parsed = cue::read_embedded_cue(&single_path).expect("embedded cue");
    assert_eq!(parsed.album_artist, "Tester");
    assert_eq!(parsed.album_title, "Test Album");
    assert_eq!(parsed.tracks.len(), 3);
    assert_eq!(parsed.tracks[1].title, "Song 2");
    assert!((parsed.tracks[1].start_secs - starts[1] as f64 / 44100.0).abs() < 1.0 / 75.0);
    // Plain audio files carry no cue sheet.
    assert!(cue::read_embedded_cue(&wav_path).is_none());
    let plain_flac = out_dir.join("01 - Tester - Song 1.flac");
    assert!(cue::read_embedded_cue(&plain_flac).is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cue_parse_roundtrip() {
    let tracks = vec![
        cue::CueTrack {
            start_secs: 0.0,
            title: "Song 1".to_string(),
            artist: "Someone Else".to_string(),
        },
        cue::CueTrack {
            start_secs: 21.95,
            title: String::new(), // exported as placeholder "Track 02"
            artist: String::new(), // exported as the album artist
        },
    ];
    let text = cue::cue_text("album.flac", "Tester", "Test Album", &tracks);
    let parsed = cue::parse_cue(&text).unwrap();

    assert_eq!(parsed.audio_file, "album.flac");
    assert_eq!(parsed.album_artist, "Tester");
    assert_eq!(parsed.album_title, "Test Album");
    assert_eq!(parsed.tracks.len(), 2);
    assert_eq!(parsed.tracks[0].title, "Song 1");
    assert_eq!(parsed.tracks[0].artist, "Someone Else");
    assert!((parsed.tracks[1].start_secs - 21.95).abs() < 1.0 / 75.0);
    // The parser is faithful; the placeholder rules are applied on load.
    assert_eq!(parsed.tracks[1].title, "Track 02");
    assert!(cue::is_default_title(&parsed.tracks[1].title, 2));
    assert!(!cue::is_default_title(&parsed.tracks[1].title, 3));
    assert!(cue::is_default_title("track 2", 2));
    assert!(!cue::is_default_title("Track Two", 2));
    assert_eq!(parsed.tracks[1].artist, "Tester"); // == album artist → emptied on load

    // Unquoted FILE argument and INDEX 00 fallback.
    let alt = "FILE recording.wav WAVE\n  TRACK 01 AUDIO\n    INDEX 00 01:02:37\n";
    let parsed = cue::parse_cue(alt).unwrap();
    assert_eq!(parsed.audio_file, "recording.wav");
    assert!((parsed.tracks[0].start_secs - (62.0 + 37.0 / 75.0)).abs() < 1e-9);
}

#[test]
fn open_cue_populates_app() {
    let dir = std::env::temp_dir().join("audio_split_cue_open_test");
    std::fs::create_dir_all(&dir).unwrap();
    let wav_path = make_test_audio(&dir);

    let cue_path = dir.join("album.cue");
    let tracks = vec![
        cue::CueTrack {
            start_secs: 0.0,
            title: "First".to_string(),
            artist: String::new(),
        },
        cue::CueTrack {
            start_secs: 22.0,
            title: String::new(),
            artist: "Guest".to_string(),
        },
    ];
    cue::write_cue(&cue_path, "test_album.wav", "Tester", "Test Album", &tracks).unwrap();

    let mut app = crate::App::default();
    let audio_path = app.read_cue_file(&cue_path).expect("resolve cue");
    assert_eq!(audio_path, wav_path);

    let audio = Arc::new(audio::load(&audio_path).unwrap());
    let _ = app.update(crate::Message::Loaded(Ok(audio)));

    assert_eq!(app.album_artist, "Tester");
    assert_eq!(app.album_title, "Test Album");
    assert_eq!(app.markers.len(), 2);
    assert_eq!(app.markers[0].title, "First");
    assert_eq!(app.markers[0].artist, "");
    assert_eq!(app.markers[1].title, ""); // "Track 02" placeholder stripped
    assert_eq!(app.markers[1].artist, "Guest");
    assert_eq!(app.markers[1].pos, 22 * 44100);

    // Prev/Next track navigation over the marker positions (0 s and 22 s).
    let _ = app.update(crate::Message::NextTrack);
    assert_eq!(app.playhead, 22 * 44100);
    let _ = app.update(crate::Message::NextTrack); // no next track: stays put
    assert_eq!(app.playhead, 22 * 44100);
    let _ = app.update(crate::Message::SetPlayhead(30 * 44100));
    let _ = app.update(crate::Message::PrevTrack); // >3 s in: restart track 2
    assert_eq!(app.playhead, 22 * 44100);
    let _ = app.update(crate::Message::PrevTrack); // near start: previous track
    assert_eq!(app.playhead, 0);
    let _ = app.update(crate::Message::PrevTrack); // nothing before: stays at 0
    assert_eq!(app.playhead, 0);

    // Cue-derived tracklists open read-only: editing messages are dropped.
    assert!(app.read_only);
    let marker_id = app.markers[0].id;
    let _ = app.update(crate::Message::DeleteMarker(marker_id));
    let _ = app.update(crate::Message::MarkerTitle(marker_id, "Changed".into()));
    let _ = app.update(crate::Message::AddMarkerAt(1000));
    let _ = app.update(crate::Message::AlbumTitle("Changed".into()));
    assert_eq!(app.markers.len(), 2);
    assert_eq!(app.markers[0].title, "First");
    assert_eq!(app.album_title, "Test Album");

    // Toggling read-only off re-enables editing.
    let _ = app.update(crate::Message::SetReadOnly(false));
    let _ = app.update(crate::Message::MarkerTitle(marker_id, "Changed".into()));
    assert_eq!(app.markers[0].title, "Changed");

    // Re-loading raw audio (no pending cue) resets to editable.
    let audio = Arc::new(audio::load(&wav_path).unwrap());
    let _ = app.update(crate::Message::SetReadOnly(true));
    let _ = app.update(crate::Message::Loaded(Ok(audio)));
    assert!(!app.read_only);

    std::fs::remove_dir_all(&dir).ok();
}
