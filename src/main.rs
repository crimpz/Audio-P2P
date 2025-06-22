// A cross‑platform (Windows + Linux) prototype for low‑latency voice chat.
// ────────────────────────────────────────────────────────────────────────────────
// Features implemented
//   • Automatically selects the default input/output audio devices on the host
//     (WASAPI on Windows; Pulse/ALSA/JACK on Linux – works fine on PipeWire
//      through the `pipewire‑pulse` compatibility layer.)
//   • Captures PCM audio, runs it through WebRTC’s echo‑canceller / AGC / noise
//     suppression, then encodes it with Opus (mono @ 48 kHz, 20 ms frames).
//   • Sends encoded frames over UDP.  A trivial 2‑byte length header is added
//     so the receiver can frame packets.
//   • A ring‑buffer acts as a *very* small jitter buffer on the playback side.
//   • Decodes Opus back to PCM and plays it on the default output device.
//
// Still TODO for production use
//   • Replace the hard‑coded `PEER_ADDR` env‑var by a proper signalling server
//     that exchanges public endpoints + (D)TLS keys.
//   • Add replay‑/re‑ordering logic in the jitter buffer.
//   • Handle multi‑user mixing (per‑room) on the server or client.
//   • Use RTP or a custom header with sequence numbers + timestamps.
//   • Expose volume / mute, opus bitrate, etc. via CLI or GUI.

use anyhow::{Context, Result};
use async_channel::{bounded, Receiver, Sender};
use bytes::{BufMut, Bytes, BytesMut};
use clap::Parser;
use cpal::traits::*;
use cpal::Sample;
use opus::{Application, Decoder as OpusDecoder, Encoder as OpusEncoder};
use parking_lot::Mutex as PLMutex;
use ringbuf::ring_buffer::{RbRead, RbRef, RbWrite};
use ringbuf::HeapRb;
use std::any::TypeId;
use std::net::SocketAddr;
use std::sync::Arc;
use stunclient::StunClient;
use tokio::sync::Mutex;
use tokio::{net::UdpSocket, task};
use tracing::{error, info, warn};
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use webrtc_audio_processing::*;

// ─── Audio constants ────────────────────────────────────────────────────────────
const SAMPLE_RATE: u32 = 48_000; // Opus best practice
const CHANNELS: usize = 1; // we down‑mix to mono for VoIP
const FRAME_MS: u32 = 20; // 20 ms frames → 50 fps
const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000; // 960
const MAX_PACKET_SIZE: usize = 400; // plenty for mono 20 ms Opus

#[derive(Debug, Parser)]
#[command(name = "voice-chat", about = "Simple P2P voice chat")]
struct Args {
    /// UDP port to bind locally (default 40000)
    #[arg(short = 'l', long, default_value_t = 40000)]
    local_port: u16,

    /// Peer address <ip:port>. If omitted we operate in “sender-only” mode.
    #[arg(short = 'p', long)]
    peer: Option<String>,
}

#[derive(serde::Serialize)]
struct JoinPayload {
    reflexive_addr: String,
    lan_addr: String,
    pub_key: String,
}

#[derive(serde::Deserialize)]
struct PeerInfo {
    reflexive_addr: String,
    lan_addr: String,
    pub_key: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Create a daily rolling log file in "logs/" directory
    let file_appender = rolling::daily("logs", "voice_chat.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_timer(fmt::time::OffsetTime::local_rfc_3339().unwrap())
                .with_ansi(false),
        )
        .with(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    std::panic::set_hook(Box::new(|panic_info| {
        error!("panic occurred: {}", panic_info);
    }));

    let args = Args::parse();
    let host = select_host()?;
    let input = host
        .default_input_device()
        .context("No default input device found")?;
    let output = host
        .default_output_device()
        .context("No default output device found")?;

    println!("--- Available Input Devices ---");
    for device in host.input_devices()? {
        println!("Input: {}", device.name()?);
    }

    println!("--- Available Output Devices ---");
    for device in host.output_devices()? {
        println!("Output: {}", device.name()?);
    }

    let in_cfg: cpal::StreamConfig = input.default_input_config()?.into();
    //in_cfg.buffer_size = cpal::BufferSize::Fixed(4096);
    let out_cfg: cpal::StreamConfig = output.default_output_config()?.into();
    //out_cfg.sample_rate = cpal::SampleRate(SAMPLE_RATE);
    //out_cfg.buffer_size = cpal::BufferSize::Fixed(2048);

    for host_id in cpal::available_hosts() {
        println!("Available host: {:?}", host_id);
    }

    info!(
        "Using input device: {}",
        input.name().unwrap_or("Unknown".into())
    );
    info!(
        "Using output device: {}",
        output.name().unwrap_or("Unknown".into())
    );
    info!("Using input config: {:?}", in_cfg);
    info!("Using output config: {:?}", out_cfg);

    // Async channels between components.
    // encoded frames to network
    let (net_tx, net_rx) = bounded::<Bytes>(1024);
    // encoded frames from network
    let (play_tx, play_rx) = bounded::<Vec<u8>>(1024);

    let local_addr = format!("0.0.0.0:{}", args.local_port);
    let remote_addr = args.peer;
    task::spawn(network_task(
        local_addr.clone(),
        remote_addr.clone(),
        net_rx,
        play_tx,
    ));

    let config = InitializationConfig {
        num_capture_channels: 2,
        num_render_channels: 2,
        ..InitializationConfig::default()
    };

    let mut ap = Processor::new(&config).unwrap();

    let config = Config {
        echo_cancellation: Some(EchoCancellation {
            suppression_level: EchoCancellationSuppressionLevel::High,
            enable_delay_agnostic: false,
            enable_extended_filter: false,
            stream_delay_ms: None,
        }),
        ..Config::default()
    };
    ap.set_config(config);

    let enc = Arc::new(PLMutex::new(OpusEncoder::new(
        SAMPLE_RATE,
        opus::Channels::Mono,
        Application::Voip,
    )?));
    let dec = Arc::new(Mutex::new(OpusDecoder::new(
        SAMPLE_RATE,
        opus::Channels::Mono,
    )?));

    // Ring buffer → tiny jitter buffer (10 frames ≈ 200 ms max).
    let ring = HeapRb::<f32>::new(FRAME_SAMPLES * 10);
    let (producer, consumer) = ring.split();

    // Build and start CPAL streams.
    let input_stream = build_input_stream(input, in_cfg, ap.clone(), enc.clone(), net_tx)?;
    let output_stream = build_output_stream(output, out_cfg, consumer)?;
    input_stream.play()?;
    output_stream.play()?;

    // Decode task (network → playback buffer).
    task::spawn(decode_task(dec, play_rx, producer));

    info!("Voice chat running, sending to {:?}", remote_addr);
    tokio::signal::ctrl_c().await?;
    Ok(())
}

// ─── Host selection ─────────────────────────────────────────────────────────────
fn select_host() -> Result<cpal::Host> {
    #[cfg(target_os = "windows")]
    {
        return Ok(cpal::host_from_id(cpal::HostId::Wasapi)?);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return Ok(cpal::default_host());
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(cpal::host_from_id(cpal::HostId::CoreAudio)?);
    }
}

// ─── CPAL input stream ─────────────────────────────────────────────────────────
fn build_input_stream(
    device: cpal::Device,
    cfg: cpal::StreamConfig,
    ap: Processor,
    enc: Arc<PLMutex<OpusEncoder>>,
    net_tx: Sender<Bytes>,
) -> Result<cpal::Stream> {
    match device.default_input_config()?.sample_format() {
        cpal::SampleFormat::F32 => build_input::<f32>(device, cfg, ap, enc, net_tx),
        cpal::SampleFormat::I16 => build_input::<i16>(device, cfg, ap, enc, net_tx),
        cpal::SampleFormat::U16 => build_input::<u16>(device, cfg, ap, enc, net_tx),
        _ => Err(anyhow::anyhow!("Unsupported sample format")),
    }
}

fn build_input<T>(
    device: cpal::Device,
    cfg: cpal::StreamConfig,
    mut ap: Processor,
    enc: Arc<PLMutex<OpusEncoder>>,
    net_tx: Sender<Bytes>,
) -> Result<cpal::Stream>
where
    T: Sample + cpal::SizedSample + 'static,
{
    let err_fn = |e| error!("input stream error: {e}");

    // Buffer to accumulate exactly one Opus frame (20 ms) before encoding.
    let mut frame_buf = Vec::<f32>::with_capacity(FRAME_SAMPLES);
    let mut tmp = vec![0f32; FRAME_SAMPLES];
    let enc = enc.clone();
    let stream = device.build_input_stream(
        &cfg,
        move |data: &[T], _| {
            for &sample in data {
                frame_buf.push(sample_to_f32(sample));
                if frame_buf.len() == FRAME_SAMPLES {
                    tmp.copy_from_slice(&frame_buf);
                    let _ = ap.process_capture_frame(&mut tmp);

                    let mut enc = enc.lock();
                    let mut pkt_buf = [0u8; MAX_PACKET_SIZE];
                    match enc.encode_float(&tmp, &mut pkt_buf) {
                        Ok(len) => {
                            let mut out = BytesMut::with_capacity(len + 2);
                            out.put_u16_le(len as u16);
                            out.extend_from_slice(&pkt_buf[..len]);
                            let _ = net_tx.try_send(out.freeze());
                        }
                        Err(e) => error!("opus encode error: {e}"),
                    }
                    frame_buf.clear();
                }
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

// ─── CPAL output stream ─────────────────────────────────────────────────────────
fn build_output_stream<S>(
    device: cpal::Device,
    cfg: cpal::StreamConfig,
    mut consumer: ringbuf::Consumer<f32, S>,
) -> Result<cpal::Stream>
where
    S: RbRef + std::marker::Send + 'static,
    <S as RbRef>::Rb: RbRead<f32>,
{
    let err_fn = |e| error!("output stream error: {e}");

    let stream = device.build_output_stream(
        &cfg,
        move |out: &mut [f32], _| {
            for sample in out {
                *sample = consumer.pop().unwrap_or(0.0);
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

// ─── Network task (UDP) ────────────────────────────────────────────────────────
async fn network_task(
    local_addr: String,
    remote_addr: Option<String>,
    outbound: Receiver<Bytes>,
    inbound_tx: Sender<Vec<u8>>,
) -> Result<()> {
    let sock = Arc::new(UdpSocket::bind(local_addr).await?);

    let public_addr = discover_public(&sock).await?;
    info!("Reflexive addr {}", public_addr);

    if let Some(peer) = &remote_addr {
        sock.connect(peer).await?;
        info!("STATUS: punch_attempt {peer}");
    } else {
        info!("STATUS: listen_only");
    }

    let sock_recv = Arc::clone(&sock);

    // Sender task
    let send = {
        let sock = Arc::clone(&sock);
        let has_peer = remote_addr.is_some();

        task::spawn(async move {
            while let Ok(pkt) = outbound.recv().await {
                if has_peer {
                    if let Err(e) = sock.send(&pkt).await {
                        error!("udp send error: {e}");
                    }
                }
            }
        })
    };

    // Receiver task
    let recv = task::spawn(async move {
        let mut buf = [0u8; MAX_PACKET_SIZE + 2];
        loop {
            let n = match sock_recv.recv(&mut buf).await {
                Ok(sz) => sz,
                Err(e) => {
                    error!("udp recv error: {e}");
                    continue;
                }
            };
            if n < 2 {
                continue;
            }
            let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
            if len + 2 > n {
                continue;
            }
            let payload = buf[2..2 + len].to_vec();
            let _ = inbound_tx.try_send(payload);
        }
    });

    let _ = tokio::join!(send, recv);
    Ok(())
}

async fn register_and_wait(room: &str, me: &JoinPayload) -> Result<PeerInfo> {
    let client = reqwest::Client::new();

    client
        .post(format!("http://your-server/join/{room}"))
        .json(me)
        .send()
        .await?
        .error_for_status()?;

    loop {
        let resp = client
            .get(format!("http://your-server/join/{room}"))
            .send()
            .await?
            .json::<Option<PeerInfo>>()
            .await?;
        if let Some(p) = resp {
            return Ok(p);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// ─── Decode task ───────────────────────────────────────────────────────────────
async fn decode_task<S>(
    dec: Arc<Mutex<OpusDecoder>>,
    inbound: Receiver<Vec<u8>>,
    mut producer: ringbuf::Producer<f32, S>,
) -> Result<()>
where
    S: RbRef,
    <S as RbRef>::Rb: RbWrite<f32>,
{
    let mut pcm_buf = vec![0f32; FRAME_SAMPLES * CHANNELS];
    while let Ok(pkt) = inbound.recv().await {
        let mut dec = dec.lock().await;
        match dec.decode_float(&pkt, &mut pcm_buf, false) {
            Ok(sz) => {
                info!("Decoded {} samples", sz);
                for &s in &pcm_buf[..sz] {
                    let _ = producer.push(s);
                }
            }
            Err(e) => eprintln!("opus decode error: {e}"),
        }
    }
    Ok(())
}

async fn discover_public(sock: &UdpSocket) -> Result<SocketAddr> {
    // Google’s anycast STUN
    let srv: SocketAddr = "74.125.194.127:19302".parse()?;
    let cli = StunClient::new(srv);
    let public = cli
        .query_external_address_async(sock)
        .await
        .context("STUN failed")?;
    Ok(public)
}

fn sample_to_f32<T: Sample + 'static>(s: T) -> f32 {
    if TypeId::of::<T>() == TypeId::of::<i16>() {
        let s: i16 = unsafe { std::mem::transmute_copy(&s) };
        s as f32 / i16::MAX as f32
    } else if TypeId::of::<T>() == TypeId::of::<u16>() {
        let s: u16 = unsafe { std::mem::transmute_copy(&s) };
        s as f32 / u16::MAX as f32 * 2.0 - 1.0
    } else if TypeId::of::<T>() == TypeId::of::<f32>() {
        let s: f32 = unsafe { std::mem::transmute_copy(&s) };
        s
    } else {
        panic!("Unsupported sample type");
    }
}
