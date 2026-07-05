use crate::app::AppEvent;
use crate::store::ASSETS_DIR;
use crate::types::AppResult;
use anyhow::anyhow;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use alloc::collections::VecDeque;
use core::fmt::Debug;
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    mpsc::{self, Receiver, Sender},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tinyaudio::{run_output_device, OutputDeviceParameters};
use tokio_util::sync::CancellationToken;
use url::Url;

const STREAMING_TIMEOUT_MILLIS: u64 = 2_000;
const OUTPUT_SAMPLE_RATE: usize = 44_100;
const OUTPUT_CHANNELS: usize = 2;
const OUTPUT_CHANNEL_SAMPLE_COUNT: usize = 2_048;
const BUFFER_SECONDS: usize = 8;

type RadioStream = HyperRadioStream;

#[derive(Debug)]
pub enum MusicPlayerEvent {
    StreamOk,
    StreamErr { error_message: String },
}

#[derive(Debug, Deserialize)]
struct Stream {
    name: String,
    url_string: String,
}

impl Stream {
    pub fn url(&self) -> AppResult<Url> {
        Ok(self.url_string.parse::<Url>()?)
    }
}

#[derive(Debug)]
struct HyperRadioStream {
    receiver: Mutex<Receiver<Result<Bytes, String>>>,
    current: Cursor<Bytes>,
}

impl HyperRadioStream {
    fn new(receiver: Receiver<Result<Bytes, String>>) -> Self {
        Self {
            receiver: Mutex::new(receiver),
            current: Cursor::new(Bytes::new()),
        }
    }
}

impl Read for HyperRadioStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.current.position() < self.current.get_ref().len() as u64 {
                return self.current.read(buf);
            }

            let received = self
                .receiver
                .lock()
                .expect("radio stream lock poisoned")
                .recv();
            match received {
                Ok(Ok(bytes)) => self.current = Cursor::new(bytes),
                Ok(Err(err)) => return Err(std::io::Error::other(err)),
                Err(_) => return Ok(0),
            }
        }
    }
}

enum AudioCommand {
    Append { data: RadioStream, generation: u64 },
    Clear { generation: u64 },
    Play,
    Pause,
}

#[derive(Debug, Default)]
enum StreamStatus {
    #[default]
    Uninitialized,
    Ready {
        sender: mpsc::Sender<AudioCommand>,
    },
}

#[derive(Debug)]
struct RadioBuffer {
    samples: Mutex<RadioSamples>,
    space_available: Condvar,
    capacity: usize,
}

#[derive(Debug, Default)]
struct RadioSamples {
    generation: u64,
    samples: VecDeque<f32>,
}

impl RadioBuffer {
    fn new() -> Self {
        Self {
            samples: Mutex::new(RadioSamples::default()),
            space_available: Condvar::new(),
            capacity: OUTPUT_SAMPLE_RATE * OUTPUT_CHANNELS * BUFFER_SECONDS,
        }
    }

    fn clear(&self, generation: u64) {
        let mut guard = self.samples.lock().expect("radio buffer lock poisoned");
        guard.generation = generation;
        guard.samples.clear();
        self.space_available.notify_all();
    }

    fn fill_output(&self, output: &mut [f32], is_playing: &AtomicBool, has_buffer: &AtomicBool) {
        output.fill(0.0);

        if !is_playing.load(Ordering::Relaxed) {
            return;
        }

        let mut guard = self.samples.lock().expect("radio buffer lock poisoned");
        let mut wrote = 0;
        while wrote < output.len() {
            let Some(sample) = guard.samples.pop_front() else {
                break;
            };
            output[wrote] = sample;
            wrote += 1;
        }

        let has_samples = !guard.samples.is_empty();
        has_buffer.store(has_samples, Ordering::Relaxed);
        if !has_samples {
            is_playing.store(false, Ordering::Relaxed);
        }
        self.space_available.notify_all();
    }

    fn push_samples(&self, generation: u64, samples: &[f32]) -> bool {
        let mut offset = 0;
        while offset < samples.len() {
            let mut guard = self.samples.lock().expect("radio buffer lock poisoned");
            if guard.generation != generation {
                return false;
            }

            while guard.samples.len() >= self.capacity {
                let result = self
                    .space_available
                    .wait_timeout(guard, Duration::from_millis(50))
                    .expect("radio buffer lock poisoned");
                guard = result.0;
                if guard.generation != generation {
                    return false;
                }
            }

            let available = self.capacity - guard.samples.len();
            let end = (offset + available).min(samples.len());
            guard.samples.extend(&samples[offset..end]);
            offset = end;
        }
        true
    }
}

#[derive(Debug, Default)]
pub struct MusicPlayer {
    stream_status: StreamStatus,
    is_buffering: Arc<AtomicBool>,
    has_buffer: Arc<AtomicBool>,
    is_playing: Arc<AtomicBool>,
    stream_generation: Arc<AtomicU64>,
    streams: Vec<Stream>,
    index: usize,
}

impl MusicPlayer {
    fn current_url(&self) -> AppResult<Url> {
        self.streams
            .get(self.index)
            .ok_or_else(|| anyhow!("No streams available"))?
            .url()
    }

    fn next_generation(&self) -> u64 {
        self.stream_generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn clear_current_stream(&self) {
        let generation = self.next_generation();
        if let StreamStatus::Ready { sender } = &self.stream_status {
            let _ = sender.send(AudioCommand::Clear { generation });
        }
    }

    fn start_streaming(&self, url: Url, app_sender: tokio::sync::mpsc::Sender<AppEvent>) {
        let sender = match &self.stream_status {
            StreamStatus::Uninitialized => unreachable!("Stream should have been initialized."),
            StreamStatus::Ready { sender } => sender.clone(),
        };

        let generation = self.next_generation();
        let _ = sender.send(AudioCommand::Clear { generation });

        let is_buffering_clone = self.is_buffering.clone();

        tokio::spawn(async move {
            is_buffering_clone.store(true, Ordering::Relaxed);

            let result =
                tokio::time::timeout(Duration::from_millis(STREAMING_TIMEOUT_MILLIS), async {
                    connect_http_stream(url).await
                })
                .await;

            match result {
                Ok(Ok(data)) => {
                    if let Err(send_err) = sender.send(AudioCommand::Append { data, generation }) {
                        let error_message = format!("Audio thread receiver dropped: {send_err:?}");
                        log::error!("{error_message}");
                        let _ = app_sender
                            .send(AppEvent::AudioEvent(MusicPlayerEvent::StreamErr {
                                error_message,
                            }))
                            .await;
                    }
                }
                Ok(Err(err)) => {
                    let error_message = format!("Unable to start audio stream: {err}");
                    log::error!("{error_message}");
                    let _ = app_sender
                        .send(AppEvent::AudioEvent(MusicPlayerEvent::StreamErr {
                            error_message,
                        }))
                        .await;
                    is_buffering_clone.store(false, Ordering::Relaxed);
                }
                Err(_) => {
                    let error_message = "Audio streaming timed out".to_string();
                    log::error!("{error_message}");
                    let _ = app_sender
                        .send(AppEvent::AudioEvent(MusicPlayerEvent::StreamErr {
                            error_message,
                        }))
                        .await;
                    is_buffering_clone.store(false, Ordering::Relaxed);
                }
            }
        });
    }

    pub fn new() -> AppResult<MusicPlayer> {
        let file = ASSETS_DIR
            .get_file("data/stream_data.json")
            .expect("Could not find stream_data.json");
        let data = file
            .contents_utf8()
            .expect("Could not read stream_data.json");
        let streams: Vec<Stream> = serde_json::from_str::<Vec<Stream>>(data)?
            .into_iter()
            .filter(|stream| match stream.url() {
                Ok(url) if matches!(url.scheme(), "http" | "https") => true,
                Ok(url) => {
                    log::warn!(
                        "Skipping unsupported radio stream scheme {}: {}",
                        stream.name,
                        url
                    );
                    false
                }
                Err(err) => {
                    log::warn!("Skipping invalid radio stream {}: {err}", stream.name);
                    false
                }
            })
            .collect();

        Ok(MusicPlayer {
            streams,
            ..Default::default()
        })
    }

    pub fn start_audio_event_loop(
        &mut self,
        cancellation_token: CancellationToken,
    ) -> AppResult<()> {
        let is_buffering_clone = self.is_buffering.clone();
        let has_buffer_clone = self.has_buffer.clone();
        let is_playing_clone = self.is_playing.clone();
        let stream_generation_clone = self.stream_generation.clone();

        let radio_buffer = Arc::new(RadioBuffer::new());
        let output_buffer = radio_buffer.clone();
        let output_is_playing = is_playing_clone.clone();
        let output_has_buffer = has_buffer_clone.clone();

        let (sender, receiver): (Sender<AudioCommand>, Receiver<AudioCommand>) = mpsc::channel();
        self.stream_status = StreamStatus::Ready { sender };

        thread::Builder::new()
            .name("audio-thread".into())
            .spawn(move || {
                let params = OutputDeviceParameters {
                    sample_rate: OUTPUT_SAMPLE_RATE,
                    channels_count: OUTPUT_CHANNELS,
                    channel_sample_count: OUTPUT_CHANNEL_SAMPLE_COUNT,
                };

                let mut device = match run_output_device(params, move |output| {
                    output_buffer.fill_output(output, &output_is_playing, &output_has_buffer);
                }) {
                    Ok(device) => device,
                    Err(err) => {
                        log::error!("Failed to create tiny audio output device: {err}");
                        return;
                    }
                };

                while let Ok(cmd) = receiver.recv() {
                    if cancellation_token.is_cancelled() {
                        log::info!("Music player loop shutting down.");
                        break;
                    }

                    match cmd {
                        AudioCommand::Append { data, generation } => {
                            if stream_generation_clone.load(Ordering::Relaxed) == generation {
                                spawn_decode_thread(
                                    data,
                                    generation,
                                    radio_buffer.clone(),
                                    is_buffering_clone.clone(),
                                    has_buffer_clone.clone(),
                                    is_playing_clone.clone(),
                                    stream_generation_clone.clone(),
                                );
                            }
                        }
                        AudioCommand::Play => {
                            if has_buffer_clone.load(Ordering::Relaxed) {
                                is_playing_clone.store(true, Ordering::Relaxed);
                            }
                        }
                        AudioCommand::Pause => {
                            is_playing_clone.store(false, Ordering::Relaxed);
                        }
                        AudioCommand::Clear { generation } => {
                            radio_buffer.clear(generation);
                            has_buffer_clone.store(false, Ordering::Relaxed);
                            is_playing_clone.store(false, Ordering::Relaxed);
                            is_buffering_clone.store(false, Ordering::Relaxed);
                        }
                    }
                }

                device.close();
            })?;

        Ok(())
    }

    pub fn is_buffering(&self) -> bool {
        self.is_buffering.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }

    pub fn previous_radio_stream(
        &mut self,
        app_sender: tokio::sync::mpsc::Sender<AppEvent>,
    ) -> AppResult<()> {
        if matches!(self.stream_status, StreamStatus::Uninitialized) {
            return Err(anyhow!("Stream is not initialized."));
        }

        if self.streams.is_empty() {
            return Err(anyhow!("No streams available"));
        }
        self.index = (self.index + self.streams.len() - 1) % self.streams.len();
        if self.is_playing() {
            let url = self.current_url()?;
            self.start_streaming(url, app_sender);
        } else {
            self.clear_current_stream();
        }
        Ok(())
    }

    pub fn next_radio_stream(
        &mut self,
        app_sender: tokio::sync::mpsc::Sender<AppEvent>,
    ) -> AppResult<()> {
        if matches!(self.stream_status, StreamStatus::Uninitialized) {
            return Err(anyhow!("Stream is not initialized."));
        }

        if self.streams.is_empty() {
            return Err(anyhow!("No streams available"));
        }
        self.index = (self.index + 1) % self.streams.len();
        if self.is_playing() {
            let url = self.current_url()?;
            self.start_streaming(url, app_sender);
        } else {
            self.clear_current_stream();
        }
        Ok(())
    }

    pub fn toggle_state(
        &mut self,
        app_sender: tokio::sync::mpsc::Sender<AppEvent>,
    ) -> AppResult<()> {
        let sender = match &self.stream_status {
            StreamStatus::Uninitialized => return Err(anyhow!("Stream is not initialized.")),
            StreamStatus::Ready { sender } => sender,
        };

        if self.is_playing() {
            let _ = sender.send(AudioCommand::Pause);
        } else if !self.has_buffer.load(Ordering::Relaxed) {
            if !self.is_buffering() {
                let url = self.current_url()?;
                self.start_streaming(url, app_sender);
            }
        } else {
            let _ = sender.send(AudioCommand::Play);
        }

        Ok(())
    }

    pub fn currently_playing(&self) -> Option<String> {
        Some(self.streams.get(self.index)?.name.clone())
    }
}

async fn connect_http_stream(url: Url) -> AppResult<HyperRadioStream> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!(
            "Unsupported radio stream URL scheme: {}",
            url.scheme()
        ));
    }

    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_webpki_roots(rustls_rustcrypto::provider())?
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    let request = Request::builder()
        .method(Method::GET)
        .uri(url.as_str())
        .header(hyper::header::USER_AGENT, "rebels-in-the-sky")
        .header(hyper::header::ACCEPT, "*/*")
        .header("Icy-MetaData", "0")
        .body(Empty::<Bytes>::new())?;

    let response = client.request(request).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "Radio stream returned HTTP status {}",
            response.status()
        ));
    }

    let (sender, receiver) = mpsc::sync_channel(16);
    tokio::spawn(async move {
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => {
                    if let Some(bytes) = frame.data_ref() {
                        if sender.send(Ok(bytes.clone())).is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = sender.send(Err(err.to_string()));
                    return;
                }
            }
        }
    });

    Ok(HyperRadioStream::new(receiver))
}

fn spawn_decode_thread(
    data: RadioStream,
    generation: u64,
    radio_buffer: Arc<RadioBuffer>,
    is_buffering: Arc<AtomicBool>,
    has_buffer: Arc<AtomicBool>,
    is_playing: Arc<AtomicBool>,
    stream_generation: Arc<AtomicU64>,
) {
    if let Err(err) = thread::Builder::new()
        .name("audio-decode-thread".into())
        .spawn(move || {
            if let Err(err) = decode_radio_stream(
                data,
                generation,
                &radio_buffer,
                &is_buffering,
                &has_buffer,
                &is_playing,
                &stream_generation,
            ) {
                if stream_generation.load(Ordering::Relaxed) == generation {
                    log::error!("Failed to decode radio stream: {err}");
                    is_buffering.store(false, Ordering::Relaxed);
                    has_buffer.store(false, Ordering::Relaxed);
                    is_playing.store(false, Ordering::Relaxed);
                }
            }
        })
    {
        log::error!("Failed to spawn audio decode thread: {err}");
    }
}

fn decode_radio_stream(
    data: RadioStream,
    generation: u64,
    radio_buffer: &RadioBuffer,
    is_buffering: &AtomicBool,
    has_buffer: &AtomicBool,
    is_playing: &AtomicBool,
    stream_generation: &AtomicU64,
) -> AppResult<()> {
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(data)), Default::default());
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No audio track in radio stream"))?;
    let track_id = track.id;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
    let mut sample_buf = None;

    loop {
        if stream_generation.load(Ordering::Relaxed) != generation {
            return Ok(());
        }

        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(err)) => {
                log::warn!("Skipping malformed radio packet: {err}");
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let spec = *decoded.spec();
        let duration = decoded.capacity() as u64;
        let sample_buf = sample_buf.get_or_insert_with(|| SampleBuffer::<f32>::new(duration, spec));
        sample_buf.copy_interleaved_ref(decoded);

        let output_samples = to_output_stereo(
            sample_buf.samples(),
            spec.rate as usize,
            spec.channels.count(),
        );
        if output_samples.is_empty() {
            continue;
        }

        if !radio_buffer.push_samples(generation, &output_samples) {
            return Ok(());
        }

        has_buffer.store(true, Ordering::Relaxed);
        is_buffering.store(false, Ordering::Relaxed);
        is_playing.store(true, Ordering::Relaxed);
    }
}

fn to_output_stereo(samples: &[f32], sample_rate: usize, channels: usize) -> Vec<f32> {
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Vec::new();
    }

    let source_frames = samples.len() / channels;
    if source_frames == 0 {
        return Vec::new();
    }

    let output_frames = (source_frames * OUTPUT_SAMPLE_RATE).div_ceil(sample_rate);
    let mut output = Vec::with_capacity(output_frames * OUTPUT_CHANNELS);

    for output_frame in 0..output_frames {
        let source_frame =
            ((output_frame * sample_rate) / OUTPUT_SAMPLE_RATE).min(source_frames - 1);
        let source_offset = source_frame * channels;
        let left = samples[source_offset];
        let right = if channels > 1 {
            samples[source_offset + 1]
        } else {
            left
        };

        output.push(left);
        output.push(right);
    }

    output
}
