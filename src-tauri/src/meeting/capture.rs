use super::types::AudioSource;
use crate::audio_toolkit::audio::FrameResampler;
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use crate::audio_toolkit::{list_input_devices, CpalDeviceInfo};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub type FrameSender = Sender<(AudioSource, Vec<f32>)>;

pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads {
            let _ = handle.join();
        }
    }
}

/// Start microphone + system-audio capture. Both streams are resampled to
/// 16 kHz mono frames (30 ms) before they are sent.
pub fn start_dual_capture(
    microphone_name: Option<String>,
    selected_channel: Option<u16>,
    system_audio_device: Option<String>,
    tx: FrameSender,
) -> Result<CaptureHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    threads.push(start_cpal_input(
        resolve_input_device(microphone_name.as_deref())?,
        AudioSource::Microphone,
        selected_channel,
        tx.clone(),
        Arc::clone(&stop),
    )?);

    match start_system_capture(system_audio_device, tx, Arc::clone(&stop)) {
        Ok(handle) => threads.push(handle),
        Err(err) => {
            stop.store(true, Ordering::Relaxed);
            for handle in threads {
                let _ = handle.join();
            }
            return Err(err);
        }
    }

    Ok(CaptureHandle { stop, threads })
}

pub fn list_system_audio_candidates() -> Result<Vec<CpalDeviceInfo>> {
    let mut devices = list_input_devices().map_err(|e| anyhow!("{e}"))?;
    devices.retain(|device| looks_like_loopback_device(&device.name));
    Ok(devices)
}

fn start_system_capture(
    system_audio_device: Option<String>,
    tx: FrameSender,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    if let Some(name) = system_audio_device {
        if name != "auto" && !name.is_empty() {
            let device = resolve_input_device(Some(&name))?;
            return start_cpal_input(device, AudioSource::System, None, tx, stop);
        }
    }

    #[cfg(windows)]
    {
        return start_wasapi_loopback(tx, stop);
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(device) = find_pulse_monitor_device() {
            return start_cpal_input(device, AudioSource::System, None, tx, stop);
        }
    }

    if let Some(device) = find_virtual_loopback_device() {
        return start_cpal_input(device, AudioSource::System, None, tx, stop);
    }

    Err(anyhow!(system_audio_help()))
}

fn system_audio_help() -> String {
    #[cfg(windows)]
    {
        "Could not capture system audio. Check that speakers/headphones are the default playback device."
            .to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "Could not capture system audio. On macOS, install BlackHole (or similar) and select it here, or grant System Audio permission and try Auto again.".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        "Could not capture system audio. Enable a PulseAudio/PipeWire monitor source, or select a loopback input device.".to_string()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        "System audio capture is not supported on this platform.".to_string()
    }
}

fn resolve_input_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = crate::audio_toolkit::get_cpal_host();
    if let Some(name) = name {
        if let Ok(devices) = list_input_devices() {
            if let Some(found) = devices.into_iter().find(|d| d.name == name) {
                return Ok(found.device);
            }
        }
        warn!("Requested input device '{name}' not found; using default");
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("No microphone input device found"))
}

fn find_virtual_loopback_device() -> Option<cpal::Device> {
    let devices = list_input_devices().ok()?;
    devices
        .into_iter()
        .find(|d| looks_like_loopback_device(&d.name))
        .map(|d| d.device)
}

#[cfg(target_os = "linux")]
fn find_pulse_monitor_device() -> Option<cpal::Device> {
    let devices = list_input_devices().ok()?;
    devices
        .into_iter()
        .find(|d| {
            let name = d.name.to_lowercase();
            name.contains("monitor") || name.contains(".monitor")
        })
        .map(|d| d.device)
}

fn looks_like_loopback_device(name: &str) -> bool {
    let name = name.to_lowercase();
    const KEYWORDS: &[&str] = &[
        "blackhole",
        "loopback",
        "soundflower",
        "vb-audio",
        "vb cable",
        "cable output",
        "stereo mix",
        "what u hear",
        "wave out mix",
        "system audio",
        "aggregate",
    ];
    KEYWORDS.iter().any(|keyword| name.contains(keyword))
}

fn start_cpal_input(
    device: cpal::Device,
    source: AudioSource,
    selected_channel: Option<u16>,
    tx: FrameSender,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
    let device_name = device.name().unwrap_or_else(|_| "unknown".into());
    info!("Meeting capture ({source:?}) using input device '{device_name}'");

    let handle = thread::Builder::new()
        .name(format!("meeting-cpal-{source:?}"))
        .spawn(move || {
            let config = match device.default_input_config() {
                Ok(config) => config,
                Err(err) => {
                    warn!("Failed to get input config for {device_name}: {err}");
                    return;
                }
            };
            let sample_rate = config.sample_rate().0;
            let channels = config.channels() as usize;
            let selected_channel = selected_channel.map(usize::from);
            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => build_input_stream::<f32>(
                    &device,
                    &config,
                    channels,
                    selected_channel,
                    sample_tx,
                    Arc::clone(&stop),
                ),
                cpal::SampleFormat::I16 => build_input_stream::<i16>(
                    &device,
                    &config,
                    channels,
                    selected_channel,
                    sample_tx,
                    Arc::clone(&stop),
                ),
                cpal::SampleFormat::I32 => build_input_stream::<i32>(
                    &device,
                    &config,
                    channels,
                    selected_channel,
                    sample_tx,
                    Arc::clone(&stop),
                ),
                cpal::SampleFormat::I8 => build_input_stream::<i8>(
                    &device,
                    &config,
                    channels,
                    selected_channel,
                    sample_tx,
                    Arc::clone(&stop),
                ),
                cpal::SampleFormat::U8 => build_input_stream::<u8>(
                    &device,
                    &config,
                    channels,
                    selected_channel,
                    sample_tx,
                    Arc::clone(&stop),
                ),
                other => {
                    warn!("Unsupported sample format {other:?} on {device_name}");
                    return;
                }
            };
            let stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    warn!("Failed to open input stream on {device_name}: {err}");
                    return;
                }
            };
            if let Err(err) = stream.play() {
                warn!("Failed to start input stream on {device_name}: {err}");
                return;
            }

            let mut resampler = FrameResampler::new(
                sample_rate as usize,
                WHISPER_SAMPLE_RATE as usize,
                Duration::from_millis(30),
            );
            while !stop.load(Ordering::Relaxed) {
                match sample_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(samples) => {
                        resampler.push(&samples, |frame| {
                            let _ = tx.send((source, frame.to_vec()));
                        });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            resampler.finish(|frame| {
                let _ = tx.send((source, frame.to_vec()));
            });
            drop(stream);
        })
        .map_err(|e| anyhow!("Failed to spawn capture thread: {e}"))?;

    Ok(handle)
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    channels: usize,
    selected_channel: Option<usize>,
    sample_tx: Sender<Vec<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: Sample + Send + 'static + cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let use_channel = match selected_channel {
        Some(channel) if channel < channels => Some(channel),
        _ => None,
    };
    let mut output = Vec::new();
    device.build_input_stream(
        &config.clone().into(),
        move |data: &[T], _| {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            output.clear();
            if channels <= 1 {
                output.extend(data.iter().map(|&sample| sample.to_sample::<f32>()));
            } else if let Some(channel) = use_channel {
                for frame in data.chunks_exact(channels) {
                    output.push(frame[channel].to_sample::<f32>());
                }
            } else {
                for frame in data.chunks_exact(channels) {
                    let mono = frame
                        .iter()
                        .map(|&sample| sample.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32;
                    output.push(mono);
                }
            }
            let _ = sample_tx.send(output.clone());
        },
        |err| warn!("Meeting input stream error: {err}"),
        None,
    )
}

#[cfg(windows)]
fn start_wasapi_loopback(tx: FrameSender, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    windows_loopback::start(tx, stop)
}

#[cfg(windows)]
mod windows_loopback {
    use super::*;
    use crate::audio_toolkit::audio::FrameResampler;
    use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
    use crate::meeting::types::AudioSource;
    use anyhow::{anyhow, Result};
    use log::{info, warn};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
    };
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const REFTIMES_PER_MILLISEC: i64 = 10_000;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

    pub fn start(tx: FrameSender, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("meeting-wasapi-loopback".into())
            .spawn(move || {
                if let Err(err) = run(tx, stop_for_thread, ready_tx) {
                    warn!("WASAPI loopback failed: {err:#}");
                }
            })
            .map_err(|e| anyhow!("Failed to spawn WASAPI loopback thread: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(err)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
                Err(anyhow!(err))
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
                Err(anyhow!("WASAPI loopback did not start in time"))
            }
        }
    }

    fn run(
        tx: FrameSender,
        stop: Arc<AtomicBool>,
        ready_tx: mpsc::SyncSender<Result<(), String>>,
    ) -> Result<()> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| anyhow!("WASAPI enumerator: {e}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .map_err(|e| anyhow!("WASAPI default render endpoint: {e}"))?;
            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| anyhow!("WASAPI activate IAudioClient: {e}"))?;
            let mix_format_ptr = audio_client
                .GetMixFormat()
                .map_err(|e| anyhow!("WASAPI mix format: {e}"))?;
            if mix_format_ptr.is_null() {
                return Err(anyhow!("WASAPI mix format was null"));
            }
            let format = *mix_format_ptr;
            let sample_rate = format.nSamplesPerSec;
            let channels = format.nChannels.max(1) as usize;
            let bits = format.wBitsPerSample;
            let is_float = is_ieee_float(&format, mix_format_ptr);

            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    100 * REFTIMES_PER_MILLISEC,
                    0,
                    mix_format_ptr,
                    None,
                )
                .map_err(|e| anyhow!("WASAPI initialize loopback: {e}"))?;
            CoTaskMemFree(Some(mix_format_ptr as *const _ as *const std::ffi::c_void));

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .map_err(|e| anyhow!("WASAPI capture client: {e}"))?;
            audio_client
                .Start()
                .map_err(|e| anyhow!("WASAPI start: {e}"))?;
            let _ = ready_tx.send(Ok(()));

            info!(
                "WASAPI loopback started ({} Hz, {} ch, {}-bit, float={is_float})",
                sample_rate, channels, bits
            );

            let mut resampler = FrameResampler::new(
                sample_rate as usize,
                WHISPER_SAMPLE_RATE as usize,
                Duration::from_millis(30),
            );

            while !stop.load(Ordering::Relaxed) {
                let packet_frames = capture_client.GetNextPacketSize().unwrap_or(0);
                if packet_frames == 0 {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let mut data_ptr: *mut u8 = ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                if capture_client
                    .GetBuffer(
                        &mut data_ptr,
                        &mut num_frames,
                        &mut flags,
                        Some(ptr::null_mut()),
                        Some(ptr::null_mut()),
                    )
                    .is_err()
                {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let samples = if silent || data_ptr.is_null() {
                    vec![0.0; num_frames as usize]
                } else {
                    convert_wasapi_buffer(
                        data_ptr,
                        num_frames as usize,
                        channels,
                        bits,
                        is_float,
                    )
                };
                let _ = capture_client.ReleaseBuffer(num_frames);
                resampler.push(&samples, |frame| {
                    let _ = tx.send((AudioSource::System, frame.to_vec()));
                });
            }

            resampler.finish(|frame| {
                let _ = tx.send((AudioSource::System, frame.to_vec()));
            });
            let _ = audio_client.Stop();
            Ok(())
        }
    }

    unsafe fn is_ieee_float(format: &WAVEFORMATEX, ptr: *mut WAVEFORMATEX) -> bool {
        if format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16 {
            true
        } else if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            let ext = ptr as *const WAVEFORMATEXTENSIBLE;
            (*ext).SubFormat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            format.wFormatTag != WAVE_FORMAT_PCM as u16
        }
    }

    fn convert_wasapi_buffer(
        data: *mut u8,
        frames: usize,
        channels: usize,
        bits: u16,
        is_float: bool,
    ) -> Vec<f32> {
        let mut mono = Vec::with_capacity(frames);
        unsafe {
            if is_float && bits == 32 {
                let samples = std::slice::from_raw_parts(data as *const f32, frames * channels);
                downmix(samples, channels, &mut mono);
            } else if bits == 16 {
                let samples = std::slice::from_raw_parts(data as *const i16, frames * channels);
                for frame in samples.chunks_exact(channels) {
                    let sum: f32 = frame.iter().map(|s| *s as f32 / 32768.0).sum();
                    mono.push(sum / channels as f32);
                }
            } else if bits == 32 {
                let samples = std::slice::from_raw_parts(data as *const i32, frames * channels);
                for frame in samples.chunks_exact(channels) {
                    let sum: f32 = frame.iter().map(|s| *s as f32 / 2147483648.0).sum();
                    mono.push(sum / channels as f32);
                }
            } else {
                mono.resize(frames, 0.0);
            }
        }
        mono
    }

    fn downmix(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
        if channels <= 1 {
            out.extend_from_slice(samples);
            return;
        }
        for frame in samples.chunks_exact(channels) {
            out.push(frame.iter().sum::<f32>() / channels as f32);
        }
    }
}
