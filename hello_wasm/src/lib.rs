use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample, Stream, StreamConfig};
use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;
use web_sys::console;

// When the `wee_alloc` feature is enabled, this uses `wee_alloc` as the global
// allocator.
// If you don't want to use `wee_alloc`, you can safely delete this.
#[cfg(feature = "wee_alloc")]
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

// This is like the `main` function, except for JavaScript.
#[wasm_bindgen(start)]
pub fn main_js() -> Result<(), JsValue> {
    // This provides better error messages in debug mode.
    // It's disabled in release mode so it doesn't bloat up the file size.
    #[cfg(debug_assertions)]
    console_error_panic_hook::set_once();
    console::log_1(&"Wasm module started".into());
    Ok(())
}

// Global buffer to store audio data
static AUDIO_BUFFER: once_cell::sync::Lazy<Mutex<Vec<f32>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(Vec::new()));

#[wasm_bindgen]
pub struct Handle(Stream);

#[wasm_bindgen]
pub fn playback() -> Handle {
    console::log_1(&"Playback function called".into());

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("failed to find a default output device");
    let config = device.default_output_config().unwrap();

    let buffer = Arc::new(Mutex::new(AUDIO_BUFFER.lock().unwrap().clone()));
    Handle(match config.sample_format() {
        cpal::SampleFormat::F32 => run_playback::<f32>(&device, &config.into(), buffer),
        cpal::SampleFormat::I16 => run_playback::<i16>(&device, &config.into(), buffer),
        cpal::SampleFormat::U16 => run_playback::<u16>(&device, &config.into(), buffer),
        _ => panic!("Unsupported sample format!"),
    })
}

#[wasm_bindgen]
pub fn record() -> Handle {
    console::log_1(&"Record function called".into());

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .expect("failed to find a default input device");
    let config = device.default_input_config().unwrap();

    let buffer = Arc::new(Mutex::new(AUDIO_BUFFER.lock().unwrap().clone()));
    Handle(match config.sample_format() {
        cpal::SampleFormat::F32 => run_capture::<f32>(&device, &config.into(), buffer),
        cpal::SampleFormat::I16 => run_capture::<i16>(&device, &config.into(), buffer),
        cpal::SampleFormat::U16 => run_capture::<u16>(&device, &config.into(), buffer),
        _ => panic!("Unsupported sample format!"),
    })
}

fn run_capture<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Stream
where
    T: SizedSample + FromSample<f32> + cpal::Sample,
{
    let err_fn = |err| console::error_1(&format!("an error occurred on stream: {}", err).into());

    device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buffer = buffer.lock().unwrap();
                for &sample in data.iter() {
                    buffer.push(sample);
                }
            },
            err_fn,
            None,
        )
        .unwrap()
}

fn run_playback<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Stream
where
    T: SizedSample + FromSample<f32> + cpal::Sample,
{
    let err_fn = |err| console::error_1(&format!("an error occurred on stream: {}", err).into());

    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut buffer = buffer.lock().unwrap();
                for sample in data.iter_mut() {
                    if !buffer.is_empty() {
                        *sample = T::from_sample(buffer.remove(0));
                    } else {
                        *sample = T::from_sample(0.0);
                    }
                }
            },
            err_fn,
            None,
        )
        .unwrap()
}

// Beep code
#[wasm_bindgen]
pub fn beep() -> Handle {
    console::log_1(&"Beep function called".into());
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("failed to find a default output device");
    let config = device.default_output_config().unwrap();

    Handle(match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(&device, &config.into()),
        cpal::SampleFormat::I16 => run::<i16>(&device, &config.into()),
        cpal::SampleFormat::U16 => run::<u16>(&device, &config.into()),
        // not all supported sample formats are included in this example
        _ => panic!("Unsupported sample format!"),
    })
}

fn run<T>(device: &cpal::Device, config: &cpal::StreamConfig) -> Stream
where
    T: SizedSample + FromSample<f32>,
{
    let sample_rate = config.sample_rate.0 as f32;
    let channels = config.channels as usize;

    // Produce a sinusoid of maximum amplitude.
    let mut sample_clock = 0f32;
    let mut next_value = move || {
        sample_clock = (sample_clock + 1.0) % sample_rate;
        (sample_clock * 440.0 * 2.0 * 3.141592 / sample_rate).sin()
    };

    let err_fn = |err| console::error_1(&format!("an error occurred on stream: {}", err).into());

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _| write_data(data, channels, &mut next_value),
            err_fn,
            None,
        )
        .unwrap();
    stream.play().unwrap();
    stream
}

fn write_data<T>(output: &mut [T], channels: usize, next_sample: &mut dyn FnMut() -> f32)
where
    T: SizedSample + FromSample<f32>,
{
    for frame in output.chunks_mut(channels) {
        let value: T = T::from_sample(next_sample());
        for sample in frame.iter_mut() {
            *sample = value;
        }
    }
}
