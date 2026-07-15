use std::io::Write;
use std::path::Path;

pub struct CueTrack {
    pub start_secs: f64,
    pub title: String,
    pub artist: String,
}

pub struct ParsedCue {
    /// File name from the (first) top-level FILE command, as written.
    pub audio_file: String,
    pub album_artist: String,
    pub album_title: String,
    pub tracks: Vec<CueTrack>,
}

/// True when a track title is just the placeholder we (and many rippers)
/// write for untitled tracks, e.g. "Track 01" / "Track 1" for track 1.
pub fn is_default_title(title: &str, number: usize) -> bool {
    let t = title.trim();
    t.eq_ignore_ascii_case(&format!("track {number:02}"))
        || t.eq_ignore_ascii_case(&format!("track {number}"))
}

/// The string argument of a cue command: quoted, or the rest of the line.
fn string_arg(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(q) = rest.strip_prefix('"') {
        q.split('"').next().unwrap_or("").to_string()
    } else {
        rest.to_string()
    }
}

/// "MM:SS:FF" (FF = 1/75 s) to seconds. MM may exceed 99.
fn parse_cue_time(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let mm: u64 = parts.next()?.parse().ok()?;
    let ss: u64 = parts.next()?.parse().ok()?;
    let ff: u64 = parts.next()?.parse().ok()?;
    if ss >= 60 || ff >= 75 || parts.next().is_some() {
        return None;
    }
    Some((mm * 60 + ss) as f64 + ff as f64 / 75.0)
}

pub fn parse_cue(text: &str) -> Result<ParsedCue, String> {
    let mut audio_file: Option<String> = None;
    let mut album_artist = String::new();
    let mut album_title = String::new();
    let mut tracks: Vec<CueTrack> = Vec::new();

    struct Current {
        title: String,
        artist: String,
        start: Option<f64>,
    }
    let mut current: Option<Current> = None;

    fn flush(current: &mut Option<Current>, tracks: &mut Vec<CueTrack>) {
        if let Some(cur) = current.take() {
            if let Some(start) = cur.start {
                tracks.push(CueTrack {
                    start_secs: start,
                    title: cur.title,
                    artist: cur.artist,
                });
            }
        }
    }

    for line in text.lines() {
        let line = line.trim();
        let (command, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (line, ""),
        };
        match command.to_ascii_uppercase().as_str() {
            "FILE" => {
                if audio_file.is_none() {
                    // The file type (WAVE, FLAC, …) trails the name; strip it
                    // when the name is unquoted.
                    let name = if rest.starts_with('"') {
                        string_arg(rest)
                    } else {
                        rest.rsplit_once(char::is_whitespace)
                            .map(|(n, _)| n.trim().to_string())
                            .unwrap_or_else(|| rest.to_string())
                    };
                    audio_file = Some(name);
                }
            }
            "TRACK" => {
                flush(&mut current, &mut tracks);
                current = Some(Current {
                    title: String::new(),
                    artist: String::new(),
                    start: None,
                });
            }
            "TITLE" => match &mut current {
                Some(cur) => cur.title = string_arg(rest),
                None => album_title = string_arg(rest),
            },
            "PERFORMER" => match &mut current {
                Some(cur) => cur.artist = string_arg(rest),
                None => album_artist = string_arg(rest),
            },
            "INDEX" => {
                if let Some(cur) = &mut current {
                    let mut parts = rest.split_whitespace();
                    let number = parts.next().and_then(|n| n.parse::<u32>().ok());
                    let time = parts.next().and_then(parse_cue_time);
                    if let (Some(number), Some(time)) = (number, time) {
                        // INDEX 01 is the track start; fall back to any other
                        // index (e.g. a lone INDEX 00 pre-gap) if 01 is absent.
                        if number == 1 || cur.start.is_none() {
                            cur.start = Some(time);
                        }
                    }
                }
            }
            _ => {} // REM, FLAGS, ISRC, CATALOG, …
        }
    }
    flush(&mut current, &mut tracks);

    let audio_file = audio_file.ok_or("Cue sheet has no FILE command")?;
    if tracks.is_empty() {
        return Err("Cue sheet has no tracks with an INDEX".to_string());
    }
    tracks.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));

    Ok(ParsedCue {
        audio_file,
        album_artist,
        album_title,
        tracks,
    })
}

/// Reads a cue sheet embedded in a FLAC file as a CUESHEET vorbis comment
/// (the convention we export, also used by foobar2000 and friends).
/// Returns `None` when the file is not FLAC, has no such comment, or the
/// comment does not parse as a cue sheet.
pub fn read_embedded_cue(path: &Path) -> Option<ParsedCue> {
    let tag = metaflac::Tag::read_from_path(path).ok()?;
    let comments = tag.vorbis_comments()?;
    // Vorbis comment keys are case-insensitive; metaflac's map is not.
    let text = comments
        .comments
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("CUESHEET"))
        .and_then(|(_, v)| v.first())?;
    parse_cue(text).ok()
}

fn escape(s: &str) -> String {
    s.replace('"', "'")
}

/// CUE time format: MM:SS:FF where FF is 1/75 of a second (CD frames).
fn cue_time(secs: f64) -> String {
    let total_frames = (secs * 75.0).round() as u64;
    let ff = total_frames % 75;
    let total_secs = total_frames / 75;
    let ss = total_secs % 60;
    let mm = total_secs / 60;
    format!("{mm:02}:{ss:02}:{ff:02}")
}

pub fn write_cue(
    cue_path: &Path,
    audio_file_name: &str,
    album_artist: &str,
    album_title: &str,
    tracks: &[CueTrack],
) -> Result<(), String> {
    let text = cue_text(audio_file_name, album_artist, album_title, tracks);
    std::fs::write(cue_path, text).map_err(|e| format!("Cannot write cue file: {e}"))
}

pub fn cue_text(
    audio_file_name: &str,
    album_artist: &str,
    album_title: &str,
    tracks: &[CueTrack],
) -> String {
    let mut out = Vec::new();

    if !album_artist.is_empty() {
        writeln!(out, "PERFORMER \"{}\"", escape(album_artist)).unwrap();
    }
    if !album_title.is_empty() {
        writeln!(out, "TITLE \"{}\"", escape(album_title)).unwrap();
    }
    writeln!(out, "FILE \"{}\" WAVE", escape(audio_file_name)).unwrap();

    for (i, track) in tracks.iter().enumerate() {
        writeln!(out, "  TRACK {:02} AUDIO", i + 1).unwrap();
        let title = if track.title.is_empty() {
            format!("Track {:02}", i + 1)
        } else {
            track.title.clone()
        };
        writeln!(out, "    TITLE \"{}\"", escape(&title)).unwrap();
        let performer = if track.artist.is_empty() {
            album_artist
        } else {
            &track.artist
        };
        if !performer.is_empty() {
            writeln!(out, "    PERFORMER \"{}\"", escape(performer)).unwrap();
        }
        writeln!(out, "    INDEX 01 {}", cue_time(track.start_secs)).unwrap();
    }

    String::from_utf8(out).expect("cue text is always valid UTF-8")
}
