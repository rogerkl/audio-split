use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;

use rodio::cpal::traits::{DeviceTrait, HostTrait};

use crate::audio::AudioData;

enum Cmd {
    Play { audio: Arc<AudioData>, start_frame: usize },
    Stop,
    SetDevice(Option<String>),
}

/// Names of the available audio output devices.
pub fn list_output_devices() -> Vec<String> {
    rodio::cpal::default_host()
        .output_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

fn open_stream(
    device_name: &Option<String>,
) -> Option<(rodio::OutputStream, rodio::OutputStreamHandle)> {
    if let Some(name) = device_name {
        let device = rodio::cpal::default_host()
            .output_devices()
            .ok()?
            .find(|d| d.name().map(|n| n == *name).unwrap_or(false));
        if let Some(device) = device {
            if let Ok(s) = rodio::OutputStream::try_from_device(&device) {
                return Some(s);
            }
            eprintln!("Cannot open audio device {name:?}, falling back to default");
        } else {
            eprintln!("Audio device {name:?} not found, falling back to default");
        }
    }
    rodio::OutputStream::try_default().ok()
}

/// Audio playback on a dedicated thread; the rodio output stream is not Send,
/// so it lives entirely inside the thread and is driven by commands.
pub struct Player {
    tx: Sender<Cmd>,
}

impl Player {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Cmd>();
        std::thread::spawn(move || {
            let mut device_name: Option<String> = None;
            let mut stream = open_stream(&device_name);
            let mut sink: Option<rodio::Sink> = None;
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Play { audio, start_frame } => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        if stream.is_none() {
                            stream = open_stream(&device_name);
                        }
                        if let Some((_, handle)) = &stream {
                            if let Ok(s) = rodio::Sink::try_new(handle) {
                                let idx = start_frame * audio.channels;
                                s.append(PcmSource { audio, idx });
                                sink = Some(s);
                            }
                        }
                    }
                    Cmd::Stop => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                    }
                    Cmd::SetDevice(name) => {
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        device_name = name;
                        // Drop the old stream before opening the new one so
                        // exclusive backends release the device first.
                        drop(stream.take());
                        stream = open_stream(&device_name);
                    }
                }
            }
        });
        Self { tx }
    }

    pub fn play(&self, audio: Arc<AudioData>, start_frame: usize) {
        let _ = self.tx.send(Cmd::Play { audio, start_frame });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    /// `None` selects the system default device.
    pub fn set_device(&self, name: Option<String>) {
        let _ = self.tx.send(Cmd::SetDevice(name));
    }
}

struct PcmSource {
    audio: Arc<AudioData>,
    idx: usize,
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let s = self.audio.samples.get(self.idx).copied();
        self.idx += 1;
        s
    }
}

impl rodio::Source for PcmSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.audio.channels as u16
    }

    fn sample_rate(&self) -> u32 {
        self.audio.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
