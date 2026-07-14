#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod cue;
mod detect;
mod flac_export;
mod player;
#[cfg(test)]
mod tests;
mod waveform;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::keyboard;
use iced::widget::{
    button, canvas, column, container, horizontal_space, pick_list, row, scrollable, slider, text,
    text_input, Canvas,
};
use iced::{Element, Length, Subscription, Task, Theme};

use audio::{format_time, AudioData};
use waveform::{ViewParams, WaveformProgram, MIN_FPP};

fn main() -> iced::Result {
    iced::application("Audio Split — CUE sheet editor", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .antialiasing(true)
        .window_size((1280.0, 860.0))
        .run()
}

#[derive(Debug, Clone)]
pub struct Marker {
    pub id: u64,
    pub pos: usize,
    pub title: String,
    pub artist: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    OpenFile,
    FileChosen(Option<PathBuf>),
    Loaded(Result<Arc<AudioData>, String>),

    Detect,
    ThresholdChanged(f32),
    MinGapChanged(f32),
    MinTrackChanged(f32),

    ViewChanged { offset: f64, fpp: f64 },
    ZoomIn,
    ZoomOut,
    ZoomFit,
    VZoomIn,
    VZoomOut,
    VZoomReset,
    WindowResized(f32),

    SetPlayhead(usize),
    PlayPause,
    Stop,
    /// Move the playhead by this fraction of the visible waveform span,
    /// so arrow-key steps scale with the zoom level.
    SeekVisible(f64),
    Tick,

    AddMarkerAtPlayhead,
    AddMarkerAt(usize),
    MoveMarker(u64, usize),
    DeleteMarker(u64),
    MarkerTitle(u64, String),
    MarkerArtist(u64, String),
    GotoMarker(u64),

    AlbumTitle(String),
    AlbumArtist(String),

    ExportCue,
    CuePathChosen(Option<PathBuf>),
    ExportFlac,
    FlacDirChosen(Option<PathBuf>),
    FlacDone(Result<usize, String>),
    ExportFlacCue,
    FlacCuePathChosen(Option<PathBuf>),
    FlacCueDone(Result<PathBuf, String>),

    DeviceSelected(String),
}

const DEFAULT_DEVICE: &str = "System default";

struct App {
    audio: Option<Arc<AudioData>>,
    loading: bool,
    exporting: bool,

    markers: Vec<Marker>,
    next_id: u64,

    album_title: String,
    album_artist: String,

    view: ViewParams,
    /// Vertical (amplitude) zoom factor, >= 1.
    v_zoom: f32,
    window_width: f32,
    wf_cache: canvas::Cache,

    /// Cue data waiting for its referenced audio file to finish loading.
    pending_cue: Option<cue::ParsedCue>,

    playhead: usize,
    playing: bool,
    play_anchor: Option<(Instant, usize)>,
    player: player::Player,
    devices: Vec<String>,
    selected_device: String,

    threshold_db: f32,
    min_gap_secs: f32,
    min_track_secs: f32,

    status: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            audio: None,
            loading: false,
            exporting: false,
            markers: Vec::new(),
            next_id: 0,
            album_title: String::new(),
            album_artist: String::new(),
            view: ViewParams::default(),
            v_zoom: 1.0,
            window_width: 1280.0,
            wf_cache: canvas::Cache::new(),
            pending_cue: None,
            playhead: 0,
            playing: false,
            play_anchor: None,
            player: player::Player::new(),
            devices: {
                let mut d = vec![DEFAULT_DEVICE.to_string()];
                d.extend(player::list_output_devices());
                d
            },
            selected_device: DEFAULT_DEVICE.to_string(),
            threshold_db: -40.0,
            min_gap_secs: 1.0,
            min_track_secs: 15.0,
            status: "Open a WAV or FLAC file to get started.".to_string(),
        }
    }
}

impl App {
    fn canvas_width(&self) -> f64 {
        (self.window_width - 24.0).max(100.0) as f64
    }

    fn clamp_view(&self, offset: f64, fpp: f64) -> ViewParams {
        let frames = self.audio.as_ref().map(|a| a.frames()).unwrap_or(0) as f64;
        let width = self.canvas_width();
        let max_fpp = (frames / width).max(MIN_FPP) * 1.1;
        let fpp = fpp.clamp(MIN_FPP, max_fpp.max(MIN_FPP));
        let max_offset = (frames - width * fpp * 0.5).max(0.0);
        ViewParams {
            offset: offset.clamp(0.0, max_offset.max(0.0)),
            fpp,
        }
    }

    fn zoom_fit(&mut self) {
        if let Some(audio) = &self.audio {
            let fpp = (audio.frames() as f64 / self.canvas_width()).max(MIN_FPP);
            self.view = ViewParams { offset: 0.0, fpp };
            self.wf_cache.clear();
        }
    }

    fn zoom_by(&mut self, factor: f64) {
        // Keep the playhead anchored if visible, otherwise the view center.
        let width = self.canvas_width();
        let center_x = {
            let px = self.view.x_of(self.playhead as f64) as f64;
            if px >= 0.0 && px <= width {
                px
            } else {
                width / 2.0
            }
        };
        let anchor = self.view.offset + center_x * self.view.fpp;
        let fpp = self.view.fpp * factor;
        let offset = anchor - center_x * fpp;
        self.view = self.clamp_view(offset, fpp);
        self.wf_cache.clear();
    }

    fn sort_markers(&mut self) {
        self.markers.sort_by_key(|m| m.pos);
    }

    fn start_playback(&mut self) {
        if let Some(audio) = &self.audio {
            let start = self.playhead.min(audio.frames());
            self.player.play(audio.clone(), start);
            self.playing = true;
            self.play_anchor = Some((Instant::now(), start));
        }
    }

    fn stop_playback(&mut self) {
        self.player.stop();
        self.playing = false;
        self.play_anchor = None;
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFile => {
                if self.loading {
                    return Task::none();
                }
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .add_filter("Audio or CUE (WAV/FLAC/CUE)", &["wav", "flac", "cue"])
                            .add_filter("CUE sheet", &["cue"])
                            .pick_file()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::FileChosen,
                );
            }
            Message::FileChosen(Some(path)) => {
                let is_cue = path
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("cue"))
                    .unwrap_or(false);
                let audio_path = if is_cue {
                    match self.read_cue_file(&path) {
                        Ok(p) => p,
                        Err(e) => {
                            self.status = e;
                            return Task::none();
                        }
                    }
                } else {
                    self.pending_cue = None;
                    path
                };
                self.loading = true;
                self.status = format!("Loading {}…", audio_path.display());
                return Task::perform(
                    async move { audio::load(&audio_path).map(Arc::new) },
                    Message::Loaded,
                );
            }
            Message::FileChosen(None) => {}
            Message::Loaded(Ok(audio)) => {
                self.loading = false;
                self.stop_playback();
                let info = format!(
                    "Loaded {} — {} ch, {} Hz, {} bit, {}",
                    audio.file_name(),
                    audio.channels,
                    audio.sample_rate,
                    audio.bits_per_sample,
                    format_time(audio.duration_secs()),
                );
                self.next_id = 0;
                if let Some(parsed) = self.pending_cue.take() {
                    let sr = audio.sample_rate as f64;
                    let frames = audio.frames();
                    self.markers = parsed
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let id = self.next_id;
                            self.next_id += 1;
                            // Placeholder titles ("Track 01") mean the title
                            // was never filled in; a track performer equal to
                            // the album artist is our "empty = album artist"
                            // export convention read back.
                            let title = if cue::is_default_title(&t.title, i + 1) {
                                String::new()
                            } else {
                                t.title.clone()
                            };
                            let artist = if t.artist == parsed.album_artist {
                                String::new()
                            } else {
                                t.artist.clone()
                            };
                            Marker {
                                id,
                                pos: ((t.start_secs * sr) as usize).min(frames),
                                title,
                                artist,
                            }
                        })
                        .collect();
                    self.album_artist = parsed.album_artist;
                    self.album_title = parsed.album_title;
                    self.status = format!("{info} — {} tracks from cue sheet", self.markers.len());
                } else {
                    self.markers = vec![Marker {
                        id: 0,
                        pos: 0,
                        title: String::new(),
                        artist: String::new(),
                    }];
                    self.next_id = 1;
                    self.status = info;
                }
                self.audio = Some(audio);
                self.playhead = 0;
                self.zoom_fit();
            }
            Message::Loaded(Err(e)) => {
                self.loading = false;
                self.pending_cue = None;
                self.status = format!("Load failed: {e}");
            }

            Message::Detect => {
                if let Some(audio) = &self.audio {
                    let params = detect::DetectParams {
                        threshold_db: self.threshold_db,
                        min_gap_secs: self.min_gap_secs,
                        min_track_secs: self.min_track_secs,
                    };
                    let starts = detect::detect_track_starts(audio, &params);
                    self.markers = starts
                        .iter()
                        .map(|&pos| {
                            let id = self.next_id;
                            self.next_id += 1;
                            Marker {
                                id,
                                pos,
                                title: String::new(),
                                artist: String::new(),
                            }
                        })
                        .collect();
                    self.status = format!(
                        "Detected {} tracks (threshold {} dB, min gap {:.1} s). Existing markers were replaced.",
                        self.markers.len(),
                        self.threshold_db,
                        self.min_gap_secs
                    );
                    self.wf_cache.clear();
                }
            }
            Message::ThresholdChanged(v) => self.threshold_db = v,
            Message::MinGapChanged(v) => self.min_gap_secs = v,
            Message::MinTrackChanged(v) => self.min_track_secs = v,

            Message::ViewChanged { offset, fpp } => {
                self.view = self.clamp_view(offset, fpp);
                self.wf_cache.clear();
            }
            Message::ZoomIn => self.zoom_by(1.0 / 1.6),
            Message::ZoomOut => self.zoom_by(1.6),
            Message::ZoomFit => self.zoom_fit(),
            Message::VZoomIn => {
                self.v_zoom = (self.v_zoom * 1.5).min(200.0);
                self.wf_cache.clear();
            }
            Message::VZoomOut => {
                self.v_zoom = (self.v_zoom / 1.5).max(1.0);
                self.wf_cache.clear();
            }
            Message::VZoomReset => {
                self.v_zoom = 1.0;
                self.wf_cache.clear();
            }
            Message::WindowResized(w) => {
                self.window_width = w;
                self.wf_cache.clear();
            }

            Message::SetPlayhead(frame) => {
                self.playhead = frame;
                if self.playing {
                    self.start_playback();
                }
            }
            Message::PlayPause => {
                if self.playing {
                    self.stop_playback();
                } else {
                    self.start_playback();
                }
            }
            Message::Stop => {
                self.stop_playback();
            }
            Message::SeekVisible(fraction) => {
                if let Some(audio) = &self.audio {
                    let delta = fraction * self.canvas_width() * self.view.fpp;
                    let new = (self.playhead as f64 + delta).clamp(0.0, audio.frames() as f64);
                    self.playhead = new as usize;
                    // Keep the playhead in view when it steps past an edge.
                    let x = (self.playhead as f64 - self.view.offset) / self.view.fpp;
                    if x < 0.0 || x > self.canvas_width() {
                        self.view = self.clamp_view(
                            self.view.offset + delta,
                            self.view.fpp,
                        );
                        self.wf_cache.clear();
                    }
                    if self.playing {
                        self.start_playback();
                    }
                }
            }
            Message::Tick => {
                if let (Some(audio), Some((start, frame))) = (&self.audio, self.play_anchor) {
                    let elapsed = start.elapsed().as_secs_f64();
                    let pos = frame + (elapsed * audio.sample_rate as f64) as usize;
                    if pos >= audio.frames() {
                        self.playhead = audio.frames();
                        self.stop_playback();
                    } else {
                        self.playhead = pos;
                        // Follow the playhead when it leaves the visible area.
                        let width = self.canvas_width();
                        let x = (self.playhead as f64 - self.view.offset) / self.view.fpp;
                        if x < 0.0 || x > width {
                            self.view = self.clamp_view(
                                self.playhead as f64 - 0.1 * width * self.view.fpp,
                                self.view.fpp,
                            );
                            self.wf_cache.clear();
                        }
                    }
                }
            }

            Message::AddMarkerAtPlayhead => {
                return self.update(Message::AddMarkerAt(self.playhead));
            }
            Message::AddMarkerAt(frame) => {
                if self.audio.is_some() {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.markers.push(Marker {
                        id,
                        pos: frame,
                        title: String::new(),
                        artist: String::new(),
                    });
                    self.sort_markers();
                    self.wf_cache.clear();
                    self.status = format!("Marker added at {}.", self.format_frame(frame));
                }
            }
            Message::MoveMarker(id, frame) => {
                if let Some(m) = self.markers.iter_mut().find(|m| m.id == id) {
                    m.pos = frame;
                }
                self.sort_markers();
                self.wf_cache.clear();
            }
            Message::DeleteMarker(id) => {
                self.markers.retain(|m| m.id != id);
                self.wf_cache.clear();
            }
            Message::MarkerTitle(id, s) => {
                if let Some(m) = self.markers.iter_mut().find(|m| m.id == id) {
                    m.title = s;
                }
            }
            Message::MarkerArtist(id, s) => {
                if let Some(m) = self.markers.iter_mut().find(|m| m.id == id) {
                    m.artist = s;
                }
            }
            Message::GotoMarker(id) => {
                if let Some(m) = self.markers.iter().find(|m| m.id == id) {
                    let pos = m.pos;
                    self.playhead = pos;
                    let width = self.canvas_width();
                    let x = (pos as f64 - self.view.offset) / self.view.fpp;
                    if x < 0.0 || x > width {
                        self.view = self
                            .clamp_view(pos as f64 - 0.1 * width * self.view.fpp, self.view.fpp);
                    }
                    self.wf_cache.clear();
                    self.start_playback();
                }
            }

            Message::AlbumTitle(s) => self.album_title = s,
            Message::AlbumArtist(s) => self.album_artist = s,

            Message::ExportCue => {
                let Some(audio) = &self.audio else {
                    return Task::none();
                };
                if self.markers.is_empty() {
                    self.status = "No markers to export.".to_string();
                    return Task::none();
                }
                let dir = audio.path.parent().map(|p| p.to_path_buf());
                let stem = audio
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "album".to_string());
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .add_filter("CUE sheet", &["cue"])
                            .set_file_name(format!("{stem}.cue"));
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|h| h.path().to_path_buf())
                    },
                    Message::CuePathChosen,
                );
            }
            Message::CuePathChosen(Some(path)) => {
                if let Some(audio) = &self.audio {
                    let sr = audio.sample_rate as f64;
                    let tracks: Vec<cue::CueTrack> = self
                        .markers
                        .iter()
                        .map(|m| cue::CueTrack {
                            start_secs: m.pos as f64 / sr,
                            title: m.title.clone(),
                            artist: m.artist.clone(),
                        })
                        .collect();
                    match cue::write_cue(
                        &path,
                        &audio.file_name(),
                        &self.album_artist,
                        &self.album_title,
                        &tracks,
                    ) {
                        Ok(()) => {
                            self.status =
                                format!("Wrote {} ({} tracks).", path.display(), tracks.len())
                        }
                        Err(e) => self.status = e,
                    }
                }
            }
            Message::CuePathChosen(None) => {}

            Message::ExportFlac => {
                if self.audio.is_none() || self.markers.is_empty() || self.exporting {
                    return Task::none();
                }
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::FlacDirChosen,
                );
            }
            Message::FlacDirChosen(Some(dir)) => {
                if let Some(audio) = &self.audio {
                    let audio = audio.clone();
                    let album_artist = self.album_artist.clone();
                    let album_title = self.album_title.clone();
                    let tracks = self.export_tracks();
                    self.exporting = true;
                    self.status = format!("Exporting {} FLAC files…", tracks.len());
                    return Task::perform(
                        async move {
                            flac_export::export_tracks(
                                &audio,
                                &dir,
                                &album_artist,
                                &album_title,
                                &tracks,
                            )
                        },
                        Message::FlacDone,
                    );
                }
            }
            Message::FlacDirChosen(None) => {}
            Message::FlacDone(result) => {
                self.exporting = false;
                self.status = match result {
                    Ok(n) => format!("Exported {n} FLAC files."),
                    Err(e) => format!("FLAC export failed: {e}"),
                };
            }

            Message::ExportFlacCue => {
                let Some(audio) = &self.audio else {
                    return Task::none();
                };
                if self.markers.is_empty() || self.exporting {
                    return Task::none();
                }
                let dir = audio.path.parent().map(|p| p.to_path_buf());
                let stem = audio
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "album".to_string());
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .add_filter("FLAC", &["flac"])
                            .set_file_name(format!("{stem} (with cue).flac"));
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|h| h.path().to_path_buf())
                    },
                    Message::FlacCuePathChosen,
                );
            }
            Message::FlacCuePathChosen(Some(path)) => {
                if let Some(audio) = &self.audio {
                    if audio.path == path {
                        self.status =
                            "Refusing to overwrite the loaded file — pick another name."
                                .to_string();
                        return Task::none();
                    }
                    let audio = audio.clone();
                    let album_artist = self.album_artist.clone();
                    let album_title = self.album_title.clone();
                    let tracks = self.export_tracks();
                    self.exporting = true;
                    self.status = "Exporting FLAC with embedded cue sheet…".to_string();
                    return Task::perform(
                        async move {
                            flac_export::export_single_with_cue(
                                &audio,
                                &path,
                                &album_artist,
                                &album_title,
                                &tracks,
                            )
                            .map(|()| path)
                        },
                        Message::FlacCueDone,
                    );
                }
            }
            Message::FlacCuePathChosen(None) => {}
            Message::FlacCueDone(result) => {
                self.exporting = false;
                self.status = match result {
                    Ok(path) => format!("Wrote {} with embedded cue sheet.", path.display()),
                    Err(e) => format!("FLAC+CUE export failed: {e}"),
                };
            }

            Message::DeviceSelected(name) => {
                self.stop_playback();
                self.selected_device = name.clone();
                self.player.set_device(if name == DEFAULT_DEVICE {
                    None
                } else {
                    Some(name)
                });
            }
        }
        Task::none()
    }

    /// Parses a .cue file, stores it as pending, and returns the resolved
    /// path of the referenced audio file.
    fn read_cue_file(&mut self, cue_path: &std::path::Path) -> Result<PathBuf, String> {
        let text = std::fs::read_to_string(cue_path)
            .map_err(|e| format!("Cannot read {}: {e}", cue_path.display()))?;
        let parsed = cue::parse_cue(&text)?;

        let cue_dir = cue_path.parent().unwrap_or(std::path::Path::new("."));
        let referenced = PathBuf::from(&parsed.audio_file);
        // Try the name as written (absolute, or relative to the cue file),
        // then fall back to just the file name next to the cue sheet.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if referenced.is_absolute() {
            candidates.push(referenced.clone());
        } else {
            candidates.push(cue_dir.join(&referenced));
        }
        if let Some(name) = referenced.file_name() {
            candidates.push(cue_dir.join(name));
        }
        let audio_path = candidates
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| {
                format!(
                    "Audio file \"{}\" referenced by the cue sheet was not found",
                    parsed.audio_file
                )
            })?;

        self.pending_cue = Some(parsed);
        Ok(audio_path)
    }

    /// Marker list as export tracks: each runs to the next marker or EOF.
    fn export_tracks(&self) -> Vec<flac_export::ExportTrack> {
        let frames = self.audio.as_ref().map(|a| a.frames()).unwrap_or(0);
        self.markers
            .iter()
            .enumerate()
            .map(|(i, m)| flac_export::ExportTrack {
                start_frame: m.pos,
                end_frame: self.markers.get(i + 1).map(|n| n.pos).unwrap_or(frames),
                title: m.title.clone(),
                artist: m.artist.clone(),
            })
            .collect()
    }

    fn format_frame(&self, frame: usize) -> String {
        let sr = self
            .audio
            .as_ref()
            .map(|a| a.sample_rate as f64)
            .unwrap_or(44100.0);
        format_time(frame as f64 / sr)
    }

    fn subscription(&self) -> Subscription<Message> {
        let keys = keyboard::on_key_press(|key, mods| match key.as_ref() {
            keyboard::Key::Named(keyboard::key::Named::Space) => Some(Message::PlayPause),
            keyboard::Key::Named(keyboard::key::Named::ArrowLeft) => {
                Some(Message::SeekVisible(-0.05))
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowRight) => {
                Some(Message::SeekVisible(0.05))
            }
            keyboard::Key::Character("m") | keyboard::Key::Character("M") => {
                Some(Message::AddMarkerAtPlayhead)
            }
            // The key is the logical (layout-dependent) character, so with
            // shift held the +/- keys may report their shifted symbols:
            // "?" (Nordic +), "*" (German +), "_" (shifted -).
            keyboard::Key::Character(c @ ("+" | "=" | "?" | "*")) => {
                if mods.shift() {
                    Some(Message::VZoomIn)
                } else if c == "+" || c == "=" {
                    Some(Message::ZoomIn)
                } else {
                    None
                }
            }
            keyboard::Key::Character("-" | "_") => {
                if mods.shift() {
                    Some(Message::VZoomOut)
                } else {
                    Some(Message::ZoomOut)
                }
            }
            _ => None,
        });

        let resize = iced::event::listen_with(|event, _status, _id| match event {
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size.width))
            }
            _ => None,
        });

        let mut subs = vec![keys, resize];
        if self.playing {
            subs.push(iced::time::every(Duration::from_millis(33)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Message> {
        let file_label = self
            .audio
            .as_ref()
            .map(|a| a.file_name())
            .unwrap_or_else(|| "No file loaded".to_string());

        let toolbar = row![
            button(text("Open…")).on_press(Message::OpenFile),
            text(file_label).size(14),
            horizontal_space(),
            button(text(if self.playing { "Pause" } else { "Play" }))
                .on_press_maybe(self.audio.as_ref().map(|_| Message::PlayPause)),
            button(text("Stop")).on_press_maybe(self.audio.as_ref().map(|_| Message::Stop)),
            button(text("+ Marker"))
                .on_press_maybe(self.audio.as_ref().map(|_| Message::AddMarkerAtPlayhead)),
            horizontal_space(),
            button(text("Export .cue")).on_press_maybe(
                (self.audio.is_some() && !self.markers.is_empty()).then_some(Message::ExportCue)
            ),
            button(text("Export FLAC tracks")).on_press_maybe(
                (self.audio.is_some() && !self.markers.is_empty() && !self.exporting)
                    .then_some(Message::ExportFlac)
            ),
            button(text("Export FLAC+CUE")).on_press_maybe(
                (self.audio.is_some() && !self.markers.is_empty() && !self.exporting)
                    .then_some(Message::ExportFlacCue)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let detect_bar = row![
            text("Silence threshold").size(13),
            slider(-70.0..=-20.0, self.threshold_db, Message::ThresholdChanged)
                .step(1.0f32)
                .width(160),
            text(format!("{:.0} dB", self.threshold_db)).size(13),
            text("Min gap").size(13),
            slider(0.2..=5.0, self.min_gap_secs, Message::MinGapChanged)
                .step(0.1f32)
                .width(120),
            text(format!("{:.1} s", self.min_gap_secs)).size(13),
            text("Min track").size(13),
            slider(1.0..=120.0, self.min_track_secs, Message::MinTrackChanged)
                .step(1.0f32)
                .width(120),
            text(format!("{:.0} s", self.min_track_secs)).size(13),
            button(text("Detect tracks"))
                .on_press_maybe(self.audio.as_ref().map(|_| Message::Detect)),
            horizontal_space(),
            text("Zoom").size(13),
            button(text("-")).on_press(Message::ZoomOut),
            button(text("+")).on_press(Message::ZoomIn),
            button(text("Fit")).on_press(Message::ZoomFit),
            text(format!("Amp ×{:.1}", self.v_zoom)).size(13),
            button(text("-")).on_press(Message::VZoomOut),
            button(text("+")).on_press(Message::VZoomIn),
            button(text("1:1")).on_press(Message::VZoomReset),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let waveform_view: Element<'_, Message> = if let Some(audio) = &self.audio {
            Canvas::new(WaveformProgram {
                audio,
                markers: &self.markers,
                view: self.view,
                v_zoom: self.v_zoom,
                playhead: self.playhead,
                cache: &self.wf_cache,
            })
            .width(Length::Fill)
            .height(Length::Fixed(300.0))
            .into()
        } else {
            container(
                text(if self.loading {
                    "Loading…"
                } else {
                    "Open a WAV or FLAC file to see its waveform here."
                })
                .size(16),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fixed(300.0))
            .into()
        };

        // Horizontal scrollbar for the waveform: maps the view offset onto
        // the scrollable range at the current zoom level.
        let scroll_bar: Element<'_, Message> = if let Some(audio) = &self.audio {
            let frames = audio.frames() as f64;
            let visible = self.canvas_width() * self.view.fpp;
            let max_offset = (frames - visible).max(0.0);
            let fpp = self.view.fpp;
            slider(
                0.0..=max_offset.max(1.0),
                self.view.offset.min(max_offset.max(1.0)),
                move |offset| Message::ViewChanged { offset, fpp },
            )
            .step(self.view.fpp.max(1.0))
            .width(Length::Fill)
            .into()
        } else {
            iced::widget::Space::with_height(0).into()
        };

        let position_bar = row![
            text(format!(
                "Playhead: {}  /  {}",
                self.format_frame(self.playhead),
                self.audio
                    .as_ref()
                    .map(|a| format_time(a.duration_secs()))
                    .unwrap_or_else(|| "--:--".to_string())
            ))
            .size(13),
            horizontal_space(),
            text("Click: playhead · Double-click: add marker · Drag flag: move · Right-click flag: delete · Wheel / +/-: zoom · Shift +/-: amp zoom · Shift+wheel: pan · Space: play/pause · ←/→: step · M: marker")
                .size(12),
        ]
        .spacing(8);

        let album_bar = row![
            text("Album artist").size(13),
            text_input("Album artist", &self.album_artist)
                .on_input(Message::AlbumArtist)
                .width(300),
            text("Album title").size(13),
            text_input("Album title", &self.album_title)
                .on_input(Message::AlbumTitle)
                .width(300),
            horizontal_space(),
            text("Output device").size(13),
            pick_list(
                self.devices.clone(),
                Some(self.selected_device.clone()),
                Message::DeviceSelected,
            )
            .text_size(13)
            .width(280),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let sr = self
            .audio
            .as_ref()
            .map(|a| a.sample_rate as f64)
            .unwrap_or(44100.0);
        let track_rows: Vec<Element<'_, Message>> = self
            .markers
            .iter()
            .enumerate()
            .map(|(i, m)| {
                row![
                    text(format!("{:02}", i + 1)).size(14).width(30),
                    text(format_time(m.pos as f64 / sr)).size(13).width(90),
                    text_input("Title", &m.title)
                        .on_input({
                            let id = m.id;
                            move |s| Message::MarkerTitle(id, s)
                        })
                        .width(Length::FillPortion(3)),
                    text_input("Artist (album artist if empty)", &m.artist)
                        .on_input({
                            let id = m.id;
                            move |s| Message::MarkerArtist(id, s)
                        })
                        .width(Length::FillPortion(2)),
                    button(text("Play").size(13)).on_press(Message::GotoMarker(m.id)),
                    button(text("Delete").size(13))
                        .style(button::danger)
                        .on_press(Message::DeleteMarker(m.id)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into()
            })
            .collect();

        let track_list = scrollable(
            iced::widget::Column::with_children(track_rows)
                .spacing(4)
                .padding(iced::Padding::from([0, 8])),
        )
        .height(Length::Fill);

        let status_bar = text(&self.status).size(13);

        column![
            toolbar,
            detect_bar,
            waveform_view,
            scroll_bar,
            position_bar,
            album_bar,
            track_list,
            status_bar,
        ]
        .spacing(10)
        .padding(12)
        .into()
    }
}
