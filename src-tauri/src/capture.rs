//! Native microphone capture for dictation.
//!
//! The webview's getUserMedia + AudioContext takes a few hundred ms to come
//! up, which ate the first word of every dictation. A CoreAudio input stream
//! (through cpal) starts in tens of ms, and once it is running it also feeds a
//! short ring buffer, so a dictation started while the stream is warm begins
//! *before* the key was pressed. The stream stays open for a short grace
//! period after each dictation (the mic indicator stays on that long), then
//! closes.
//!
//! The stream lives on its own thread: `cpal::Stream` isn't `Send`.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Audio kept from before the key press, when the stream is already warm.
const PRE_ROLL: Duration = Duration::from_millis(400);
/// How long the stream stays open after a dictation, ready for the next one.
const WARM_GRACE: Duration = Duration::from_secs(20);
pub const OUT_RATE: u32 = 16_000;

#[derive(Default)]
struct Shared {
    recording: bool,
    rate: u32,
    buf: Vec<f32>,
    ring: VecDeque<f32>,
    /// RMS of the latest callback block, for the pill's waveform.
    level: f32,
}

enum Cmd {
    Start,
    Stop(Sender<Option<(Vec<f32>, u32)>>),
    Cancel,
}

struct Recorder {
    tx: Mutex<Sender<Cmd>>,
    shared: Arc<Mutex<Shared>>,
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();

fn recorder() -> &'static Recorder {
    RECORDER.get_or_init(|| {
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("vox-capture".into())
            .spawn(move || audio_thread(rx, sh))
            .expect("capture thread");
        Recorder { tx: Mutex::new(tx), shared }
    })
}

/// Begin recording (opening the device if it is cold). Never blocks.
pub fn start() {
    let _ = recorder().tx.lock().unwrap().send(Cmd::Start);
}

/// Throw the current recording away.
pub fn cancel() {
    let _ = recorder().tx.lock().unwrap().send(Cmd::Cancel);
}

/// Stop and return the recording resampled to 16 kHz mono.
pub fn stop() -> Option<Vec<f32>> {
    let (tx, rx) = channel();
    recorder().tx.lock().unwrap().send(Cmd::Stop(tx)).ok()?;
    let (samples, rate) = rx.recv_timeout(Duration::from_secs(3)).ok()??;
    Some(resample(&samples, rate, OUT_RATE))
}

/// Current input level, 0..1 (roughly — speech sits around 0.2–0.6).
pub fn level() -> f32 {
    let l = recorder().shared.lock().unwrap().level;
    (l * 6.0).sqrt().min(1.0)
}

fn audio_thread(rx: Receiver<Cmd>, shared: Arc<Mutex<Shared>>) {
    let mut stream: Option<cpal::Stream> = None;
    let mut idle_since: Option<Instant> = None;
    loop {
        let cmd = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match cmd {
            Some(Cmd::Start) => {
                if stream.is_none() {
                    let t = Instant::now();
                    match open_stream(shared.clone()) {
                        Ok(s) => {
                            println!("[vox] capture: mic opened in {:?}", t.elapsed());
                            stream = Some(s);
                        }
                        Err(e) => eprintln!("[vox] capture: cannot open mic: {e}"),
                    }
                }
                let mut sh = shared.lock().unwrap();
                sh.buf = sh.ring.drain(..).collect();
                sh.recording = true;
                idle_since = None;
            }
            Some(Cmd::Stop(reply)) => {
                let mut sh = shared.lock().unwrap();
                sh.recording = false;
                let out = std::mem::take(&mut sh.buf);
                let rate = sh.rate;
                drop(sh);
                let _ = reply.send((stream.is_some() && rate > 0).then_some((out, rate)));
                idle_since = Some(Instant::now());
            }
            Some(Cmd::Cancel) => {
                let mut sh = shared.lock().unwrap();
                sh.recording = false;
                sh.buf.clear();
                idle_since = Some(Instant::now());
            }
            None => {}
        }
        if idle_since.is_some_and(|t| t.elapsed() > WARM_GRACE) && stream.is_some() {
            stream = None; // drop → device closed, mic indicator off
            let mut sh = shared.lock().unwrap();
            sh.ring.clear();
            sh.level = 0.0;
            idle_since = None;
            println!("[vox] capture: mic closed");
        }
    }
}

fn open_stream(shared: Arc<Mutex<Shared>>) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("no input device")?;
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let pre_roll = (rate as f64 * PRE_ROLL.as_secs_f64()) as usize;
    {
        let mut sh = shared.lock().unwrap();
        sh.rate = rate;
        sh.ring = VecDeque::with_capacity(pre_roll + 4096);
    }
    let err = |e| eprintln!("[vox] capture stream error: {e}");
    let push = move |mono: &mut dyn Iterator<Item = f32>| {
        let mut sh = shared.lock().unwrap();
        let mut sum = 0.0f32;
        let mut n = 0usize;
        for s in mono {
            sum += s * s;
            n += 1;
            if sh.recording {
                sh.buf.push(s);
            } else {
                sh.ring.push_back(s);
            }
        }
        while sh.ring.len() > pre_roll {
            sh.ring.pop_front();
        }
        if n > 0 {
            sh.level = (sum / n as f32).sqrt();
        }
    };
    let cfg: cpal::StreamConfig = config.clone().into();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            device.build_input_stream(
                &cfg,
                move |data: &[f32], _| push(&mut data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32)),
                err,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            device.build_input_stream(
                &cfg,
                move |data: &[i16], _| {
                    push(&mut data.chunks(channels).map(|f| f.iter().map(|&s| s as f32 / 32768.0).sum::<f32>() / channels as f32))
                },
                err,
                None,
            )
        }
        f => return Err(format!("unsupported sample format {f:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Linear-interpolation resampler, with a box pre-filter when downsampling so
/// 48 kHz → 16 kHz doesn't alias. Plenty for speech recognition.
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let src: Vec<f32> = if ratio > 1.0 {
        let w = ratio.round().max(1.0) as usize;
        let mut acc = 0.0f32;
        let mut out = Vec::with_capacity(input.len());
        for i in 0..input.len() {
            acc += input[i];
            if i >= w {
                acc -= input[i - w];
            }
            out.push(acc / (i + 1).min(w) as f32);
        }
        out
    } else {
        input.to_vec()
    };
    let n = (src.len() as f64 / ratio) as usize;
    (0..n)
        .map(|i| {
            let p = i as f64 * ratio;
            let j = p as usize;
            let f = (p - j as f64) as f32;
            let a = src[j.min(src.len() - 1)];
            let b = src[(j + 1).min(src.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

/// 16-bit PCM mono WAV. Quiet recordings are brought up (capped gain) —
/// there's no browser AGC on this path any more.
pub fn write_wav(path: &Path, samples: &[f32], rate: u32) -> std::io::Result<()> {
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let gain = if peak > 0.0 { (0.9 / peak).clamp(1.0, 8.0) } else { 1.0 };
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&(((s * gain).clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_lengths() {
        let x = vec![0.5f32; 48_000];
        let y = resample(&x, 48_000, 16_000);
        assert_eq!(y.len(), 16_000);
        assert!((y[8000] - 0.5).abs() < 1e-4);
        assert_eq!(resample(&x, 16_000, 16_000).len(), 48_000);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// `VOX_WAV_OUT=/path.wav cargo test --lib capture::live_tests -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn record_three_seconds() {
        start();
        std::thread::sleep(Duration::from_millis(3500));
        let s = stop().expect("samples");
        let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let rate = recorder().shared.lock().unwrap().rate;
        println!("device rate {rate} → {} samples @16k ({:.2}s), peak {peak:.3}", s.len(), s.len() as f64 / 16000.0);
        let out = std::env::var("VOX_WAV_OUT").unwrap_or("/tmp/vox_cap.wav".into());
        write_wav(Path::new(&out), &s, OUT_RATE).unwrap();
    }
}
