use iced::keyboard;
use iced::mouse;
use iced::widget::canvas::{self, Event, Geometry, Path, Stroke, Text};
use iced::{Color, Pixels, Point, Rectangle, Renderer, Size, Theme};

use crate::audio::{format_time, AudioData, PEAK_BIN};
use crate::{Marker, Message};

/// Height of the marker/ruler strip at the top of the canvas.
pub const STRIP_H: f32 = 28.0;
/// Grab distance for marker flags, in pixels.
const HIT_PX: f32 = 6.0;

pub const MIN_FPP: f64 = 0.02;

#[derive(Clone, Copy)]
pub struct ViewParams {
    /// First visible sample frame (can be fractional while zooming).
    pub offset: f64,
    /// Frames per pixel (zoom level).
    pub fpp: f64,
}

impl Default for ViewParams {
    fn default() -> Self {
        Self {
            offset: 0.0,
            fpp: 1024.0,
        }
    }
}

impl ViewParams {
    pub fn frame_at(&self, x: f32) -> f64 {
        self.offset + x as f64 * self.fpp
    }

    pub fn x_of(&self, frame: f64) -> f32 {
        ((frame - self.offset) / self.fpp) as f32
    }
}

#[derive(Default)]
pub struct WfState {
    dragging: Option<u64>,
    scrubbing: bool,
    shift: bool,
    last_click: Option<(std::time::Instant, Point)>,
}

pub struct WaveformProgram<'a> {
    pub audio: &'a AudioData,
    pub markers: &'a [Marker],
    pub view: ViewParams,
    /// Vertical (amplitude) zoom factor; peaks are clamped to the lane.
    pub v_zoom: f32,
    pub playhead: usize,
    pub cache: &'a canvas::Cache,
}

impl WaveformProgram<'_> {
    fn clamp_frame(&self, frame: f64) -> usize {
        frame.max(0.0).min(self.audio.frames() as f64) as usize
    }

    fn hit_marker(&self, x: f32) -> Option<u64> {
        let mut best: Option<(f32, u64)> = None;
        for m in self.markers {
            let mx = self.view.x_of(m.pos as f64);
            let d = (mx - x).abs();
            if d <= HIT_PX && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, m.id));
            }
        }
        best.map(|(_, id)| id)
    }
}

impl canvas::Program<Message> for WaveformProgram<'_> {
    type State = WfState;

    fn update(
        &self,
        state: &mut WfState,
        event: Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        use canvas::event::Status;

        if let Event::Keyboard(keyboard::Event::ModifiersChanged(mods)) = event {
            state.shift = mods.shift();
            return (Status::Ignored, None);
        }

        let Event::Mouse(mouse_event) = event else {
            return (Status::Ignored, None);
        };

        let Some(pos) = cursor.position_in(bounds) else {
            if let mouse::Event::ButtonReleased(_) = mouse_event {
                state.dragging = None;
                state.scrubbing = false;
            }
            return (Status::Ignored, None);
        };

        match mouse_event {
            mouse::Event::WheelScrolled { delta } => {
                let (dx, dy) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (x * 40.0, y * 40.0),
                    mouse::ScrollDelta::Pixels { x, y } => (x, y),
                };
                let msg = if state.shift || dx.abs() > dy.abs() {
                    // Pan horizontally.
                    let d = if dx.abs() > dy.abs() { dx } else { dy };
                    Message::ViewChanged {
                        offset: self.view.offset - d as f64 * self.view.fpp,
                        fpp: self.view.fpp,
                    }
                } else {
                    // Zoom around the cursor position.
                    let factor = if dy > 0.0 { 1.0 / 1.3 } else { 1.3 };
                    let max_fpp =
                        (self.audio.frames() as f64 / bounds.width.max(1.0) as f64).max(MIN_FPP);
                    let new_fpp = (self.view.fpp * factor).clamp(MIN_FPP, max_fpp * 1.1);
                    let anchor = self.view.frame_at(pos.x);
                    Message::ViewChanged {
                        offset: anchor - pos.x as f64 * new_fpp,
                        fpp: new_fpp,
                    }
                };
                (Status::Captured, Some(msg))
            }
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                let frame = self.clamp_frame(self.view.frame_at(pos.x));
                if pos.y <= STRIP_H {
                    if let Some(id) = self.hit_marker(pos.x) {
                        state.dragging = Some(id);
                        (Status::Captured, None)
                    } else {
                        (Status::Captured, Some(Message::SetPlayhead(frame)))
                    }
                } else {
                    let is_double = state
                        .last_click
                        .map(|(t, p)| {
                            t.elapsed().as_millis() < 400 && p.distance(pos) < 6.0
                        })
                        .unwrap_or(false);
                    state.last_click = Some((std::time::Instant::now(), pos));
                    if is_double {
                        (Status::Captured, Some(Message::AddMarkerAt(frame)))
                    } else {
                        state.scrubbing = true;
                        (Status::Captured, Some(Message::SetPlayhead(frame)))
                    }
                }
            }
            mouse::Event::ButtonPressed(mouse::Button::Right) => {
                if pos.y <= STRIP_H {
                    if let Some(id) = self.hit_marker(pos.x) {
                        return (Status::Captured, Some(Message::DeleteMarker(id)));
                    }
                }
                (Status::Ignored, None)
            }
            mouse::Event::CursorMoved { .. } => {
                let frame = self.clamp_frame(self.view.frame_at(pos.x));
                if let Some(id) = state.dragging {
                    (Status::Captured, Some(Message::MoveMarker(id, frame)))
                } else if state.scrubbing {
                    (Status::Captured, Some(Message::SetPlayhead(frame)))
                } else {
                    (Status::Ignored, None)
                }
            }
            mouse::Event::ButtonReleased(_) => {
                state.dragging = None;
                state.scrubbing = false;
                (Status::Ignored, None)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        _state: &WfState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let waveform = self.cache.draw(renderer, bounds.size(), |frame| {
            self.draw_static(frame, bounds.size());
        });

        // Playhead + cursor readout are drawn uncached so playback stays cheap.
        let mut overlay = canvas::Frame::new(renderer, bounds.size());
        let px = self.view.x_of(self.playhead as f64);
        if px >= 0.0 && px <= bounds.width {
            overlay.stroke(
                &Path::line(Point::new(px, 0.0), Point::new(px, bounds.height)),
                Stroke::default()
                    .with_color(Color::from_rgb(0.30, 0.85, 0.40))
                    .with_width(1.5),
            );
        }
        if let Some(pos) = cursor.position_in(bounds) {
            let secs = self.view.frame_at(pos.x).max(0.0) / self.audio.sample_rate as f64;
            overlay.fill_text(Text {
                content: format_time(secs),
                position: Point::new(bounds.width - 8.0, bounds.height - 6.0),
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.7),
                size: Pixels(12.0),
                horizontal_alignment: iced::alignment::Horizontal::Right,
                vertical_alignment: iced::alignment::Vertical::Bottom,
                ..Text::default()
            });
        }

        vec![waveform, overlay.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &WfState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging.is_some() {
            return mouse::Interaction::Grabbing;
        }
        if let Some(pos) = cursor.position_in(bounds) {
            if pos.y <= STRIP_H && self.hit_marker(pos.x).is_some() {
                return mouse::Interaction::Grab;
            }
        }
        mouse::Interaction::default()
    }
}

impl WaveformProgram<'_> {
    fn draw_static(&self, frame: &mut canvas::Frame, size: Size) {
        let bg = Color::from_rgb(0.10, 0.11, 0.13);
        let strip_bg = Color::from_rgb(0.15, 0.16, 0.19);
        let wave_color = Color::from_rgb(0.35, 0.60, 0.90);
        let center_color = Color::from_rgba(1.0, 1.0, 1.0, 0.15);
        let marker_color = Color::from_rgb(0.95, 0.60, 0.15);
        let tick_color = Color::from_rgba(1.0, 1.0, 1.0, 0.25);
        let label_color = Color::from_rgba(1.0, 1.0, 1.0, 0.55);

        frame.fill_rectangle(Point::ORIGIN, size, bg);
        frame.fill_rectangle(Point::ORIGIN, Size::new(size.width, STRIP_H), strip_bg);

        let channels = self.audio.channels.max(1);
        let wave_top = STRIP_H;
        let lane_h = (size.height - wave_top) / channels as f32;
        let frames_total = self.audio.frames();

        // Time ruler ticks.
        let secs_per_px = self.view.fpp / self.audio.sample_rate as f64;
        let target = secs_per_px * 90.0; // aim for a tick roughly every 90 px
        let intervals = [
            0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
            600.0, 1200.0, 1800.0,
        ];
        let tick = intervals
            .iter()
            .copied()
            .find(|&i| i >= target)
            .unwrap_or(3600.0);
        let start_secs = self.view.offset.max(0.0) / self.audio.sample_rate as f64;
        let mut t = (start_secs / tick).floor() * tick;
        let end_secs = self.view.frame_at(size.width) / self.audio.sample_rate as f64;
        while t <= end_secs {
            if t >= 0.0 {
                let x = self.view.x_of(t * self.audio.sample_rate as f64);
                if x >= 0.0 && x <= size.width {
                    frame.stroke(
                        &Path::line(
                            Point::new(x, STRIP_H - 6.0),
                            Point::new(x, size.height),
                        ),
                        Stroke::default().with_color(tick_color).with_width(1.0),
                    );
                    let label = if tick >= 1.0 {
                        let total = t.round() as u64;
                        format!("{}:{:02}", total / 60, total % 60)
                    } else {
                        format!("{:.2}", t)
                    };
                    frame.fill_text(Text {
                        content: label,
                        position: Point::new(x + 3.0, STRIP_H - 4.0),
                        color: label_color,
                        size: Pixels(10.0),
                        vertical_alignment: iced::alignment::Vertical::Bottom,
                        ..Text::default()
                    });
                }
            }
            t += tick;
        }

        // Waveform, one min/max column per pixel.
        for ch in 0..channels {
            let lane_top = wave_top + ch as f32 * lane_h;
            let mid = lane_top + lane_h / 2.0;
            let half = lane_h / 2.0 - 2.0;

            frame.stroke(
                &Path::line(Point::new(0.0, mid), Point::new(size.width, mid)),
                Stroke::default().with_color(center_color).with_width(1.0),
            );

            let path = Path::new(|builder| {
                for x in 0..size.width as usize {
                    let f0 = self.view.frame_at(x as f32);
                    let f1 = f0 + self.view.fpp;
                    if f1 < 0.0 || f0 >= frames_total as f64 {
                        continue;
                    }
                    let begin = f0.max(0.0) as usize;
                    let end = (f1.max(0.0) as usize).min(frames_total).max(begin + 1);
                    let (min, max) = self.min_max(ch, begin, end.min(frames_total));
                    let y0 = mid - (max * self.v_zoom).clamp(-1.0, 1.0) * half;
                    let y1 = mid - (min * self.v_zoom).clamp(-1.0, 1.0) * half;
                    builder.move_to(Point::new(x as f32 + 0.5, y0));
                    builder.line_to(Point::new(x as f32 + 0.5, y1.max(y0 + 0.5)));
                }
            });
            frame.stroke(
                &path,
                Stroke::default().with_color(wave_color).with_width(1.0),
            );
        }

        // Markers: full-height line + numbered flag in the strip.
        for (i, m) in self.markers.iter().enumerate() {
            let x = self.view.x_of(m.pos as f64);
            if x < -30.0 || x > size.width + 30.0 {
                continue;
            }
            frame.stroke(
                &Path::line(Point::new(x, 0.0), Point::new(x, size.height)),
                Stroke::default().with_color(marker_color).with_width(1.0),
            );
            frame.fill_rectangle(
                Point::new(x, 2.0),
                Size::new(22.0, STRIP_H - 10.0),
                marker_color,
            );
            frame.fill_text(Text {
                content: format!("{:02}", i + 1),
                position: Point::new(x + 4.0, 4.0),
                color: Color::from_rgb(0.1, 0.1, 0.1),
                size: Pixels(12.0),
                ..Text::default()
            });
        }
    }

    /// Min/max of samples in [begin, end) for one channel, using the peak
    /// pyramid when the range is large and raw samples when zoomed in.
    fn min_max(&self, ch: usize, begin: usize, end: usize) -> (f32, f32) {
        if end <= begin {
            return (0.0, 0.0);
        }
        if end - begin >= PEAK_BIN {
            let b0 = begin / PEAK_BIN;
            let b1 = (end / PEAK_BIN).min(self.audio.peaks[ch].len());
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            for &(lo, hi) in &self.audio.peaks[ch][b0..b1.max(b0 + 1)] {
                min = min.min(lo);
                max = max.max(hi);
            }
            (min, max)
        } else {
            let channels = self.audio.channels;
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            for f in begin..end {
                let s = self.audio.samples[f * channels + ch];
                min = min.min(s);
                max = max.max(s);
            }
            (min, max)
        }
    }
}
