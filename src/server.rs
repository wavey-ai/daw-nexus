use crate::allocator::{Channels, Streams};
use access_unit::AccessUnit;
use bytes::{Bytes, BytesMut};
use futures::SinkExt;
use gen_id::{ConfigPreset::ShortEpochMaxNodes, IdGenerator, DEFAULT_EPOCH};
use regex::Regex;
use rtrb::{Consumer, Producer, RingBuffer};
use soundkit::audio_packet::{encode_audio_packet, Decoder, Encoder, FrameHeader};
use soundkit::audio_types::EncodingFlag;
use soundkit_flac::{FlacDecoder, FlacEncoder};
use soundkit_opus::OpusEncoder;
use srt_tokio::SrtSocket;
use std::net::SocketAddr;
use std::str;
use std::sync::Arc;
use tcp_changes::Payload;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::{oneshot, watch};
use tokio::time::{self, Duration, Instant};
use tokio_stream::StreamExt;
use tracing::{error, info};
use ts::demuxer::TsDemuxer;
use xmpegts::{define::epsi_stream_type, ts::TsMuxer};

const NUM_CHANNELS: usize = 16;

struct Stream {}
impl Stream {
    async fn start(
        mut rx: mpsc::Receiver<Bytes>,
        stream_key: &str,
        streams: Arc<Streams>,
    ) -> Result<(), std::io::Error> {
        let (tx, mut rrx) = mpsc::channel::<AccessUnit>(32);

        let demux_tx = TsDemuxer::start(tx.clone());

        let addr: &str = "local.wavey.io:8000";
        let mut socket = SrtSocket::builder()
            .call(addr, Some(stream_key))
            .await
            .unwrap();

        let streams_clone = streams.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);

            rt.block_on(async {
                let mut decoder = FlacDecoder::new();
                decoder.init().expect("Decoder initialization failed");

                let mut producers: Vec<Option<Producer<Bytes>>> =
                    (0..NUM_CHANNELS).map(|_| None).collect();

                while let Some(au) = rrx.recv().await {
                    let mut decoded_samples = vec![0i32; 1024 * 2];
                    let header =
                        FrameHeader::decode(&mut &au.data[..20]).expect("failed to decode header");
                    let id = header.id().expect("failed to find id in header");
                    let node_id = gen.decode_id(id).node_id as usize;
                    if producers[node_id].is_none() {
                        producers[node_id] = Some(streams_clone.add(node_id as usize));
                    }

                    match decoder.decode_i32(&au.data, &mut decoded_samples, false) {
                        Ok(sample_count) => {
                            let mut result = Vec::with_capacity(sample_count);
                            for &s32_sample in &decoded_samples[..sample_count] {
                                let f32_sample = (s32_sample as f32) / (2.0f32.powi(23) - 1.0);
                                result.extend_from_slice(&f32_sample.to_le_bytes());
                            }

                            if let Some(producer) = producers[node_id].as_mut() {
                                match producer.write_chunk_uninit(result.len()) {
                                    Ok(chunk) => {
                                        let bytes_result = Bytes::copy_from_slice(&result);
                                        chunk.fill_from_iter(std::iter::once(bytes_result));
                                    }
                                    Err(e) => {
                                        eprintln!("Error adding to rtrb: {}", e);
                                    }
                                }
                            }

                            result.clear();
                        }
                        Err(e) => panic!("Decoding failed: {}", e),
                    }
                }
            });
        });

        loop {
            tokio::select! {
                result = socket.next() => {
                    match result {
                        Some(Ok((_, data))) => {

                            if let Err(e) = demux_tx.send(data).await {
                                 error!("Error sending to demuxer: {:?}", e);
                            }
                        }
                        Some(Err(e)) => {
                            error!("Error receiving SRT data: {:?}", e);
                        }
                        None => {
                            error!("Connection closed.");
                            break;
                        }
                    }
                },

                Some(data) = rx.recv() => {
                   if let Err(e) = socket.send((std::time::Instant::now(), data)).await {
                        error!("Error sending SRT packet {:?}", e);
                    }
                }
            }
        }

        Ok(())
    }
}

fn extract_id(payload: Bytes) -> Option<u64> {
    if let Ok(message) = str::from_utf8(&payload) {
        let re = Regex::new(r"id=(\d+) addr=").unwrap();

        if let Some(caps) = re.captures(message) {
            if let Some(id_match) = caps.get(1) {
                if let Ok(id) = id_match.as_str().parse::<u64>() {
                    return Some(id);
                }
            }
        }
    } else {
        println!("Failed to convert Bytes to a valid UTF-8 string.");
    }

    None
}

pub struct Server {}

impl Server {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn start(
        &self,
        addr: SocketAddr,
        mut rx: mpsc::Receiver<Payload>,
    ) -> Result<
        (
            oneshot::Receiver<()>,
            oneshot::Receiver<()>,
            watch::Sender<()>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        let (up_tx, up_rx) = oneshot::channel();
        let (fin_tx, fin_rx) = oneshot::channel();

        let (srt_tx, mut srt_rx) = mpsc::channel::<Bytes>(100);

        let chans = Channels::new(NUM_CHANNELS);
        let streams = Streams::new(NUM_CHANNELS);

        tokio::task::spawn(async move {
            while let Some(payload) = rx.recv().await {
                if let Some(id) = extract_id(payload.val) {}
            }
        });

        let streams_clone = streams.clone();
        tokio::task::spawn(async move {
            Stream::start(srt_rx, "flac", streams_clone).await;
        });

        let incoming = TcpListener::bind(addr).await.unwrap();
        up_tx.send(()).unwrap();

        let srv = async move {
            loop {
                tokio::select! {
                    result = incoming.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                println!("accepting new connection!");
                                let srt_tx = srt_tx.clone();
                                let chans = chans.clone();
                                let streams = streams.clone();
                                tokio::task::spawn(async move {
                                    stream_handler(stream, srt_tx, chans, streams).await;
                                });
                            }
                            Err(err) => {
                                eprintln!("Error accepting connection: {:?}", err);
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        info!("Received shutdown signal, exiting...");
                        break;
                    }
                }
            }

            fin_tx.send(()).unwrap();
        };

        tokio::spawn(srv);

        Ok((up_rx, fin_rx, shutdown_tx))
    }
}

struct EncodeRequest {
    buffer: Bytes,
    dts: u64,
}

struct EncodeResponse {
    result: BytesMut,
    encoding: EncodingFlag,
    dts: u64,
}

async fn stream_handler(
    mut stream: TcpStream,
    tx: mpsc::Sender<Bytes>,
    chans: Arc<Channels>,
    streams: Arc<Streams>,
) {
    let (encode_tx, mut encode_rx) = mpsc::channel::<EncodeRequest>(100);
    let (response_tx, mut response_rx) = mpsc::channel::<EncodeResponse>(100);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut flac = FlacEncoder::new(48000, 32, 2, 1024, 1);
            flac.init().unwrap();
            let mut opus = OpusEncoder::new(48000, 32, 2, 480, 5);
            opus.init().unwrap();

            while let Some(request) = encode_rx.recv().await {
                if false {
                    match encode_audio_packet(EncodingFlag::Opus, &mut opus, &request.buffer) {
                        Ok(result) => {
                            let opus_response = EncodeResponse {
                                result,
                                encoding: EncodingFlag::Opus,
                                dts: request.dts,
                            };
                            if response_tx.send(opus_response).await.is_err() {
                                error!("Failed to send Opus encode response");
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Error encoding Opus packet: {:?}", e);
                        }
                    }
                }
                match encode_audio_packet(EncodingFlag::FLAC, &mut flac, &request.buffer) {
                    Ok(result) => {
                        let flac_response = EncodeResponse {
                            result,
                            encoding: EncodingFlag::FLAC,
                            dts: request.dts,
                        };

                        if response_tx.send(flac_response).await.is_err() {
                            error!("Failed to send FLAC encode response");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Error encoding FLAC packet: {:?}", e);
                    }
                }
            }
        });
    });

    // Variables for tracking frame parsing
    let mut buffer = BytesMut::with_capacity(8192);
    let mut read_buf = [0u8; 1024];

    let mut current_frame_length: Option<usize> = None;
    let mut dts: u64 = 0;
    let mut ms: u64 = 0;

    let mut ts_muxer: TsMuxer = TsMuxer::new();

    let audio_pid: u16 = ts_muxer
        .add_stream(epsi_stream_type::PSI_STREAM_AAC, BytesMut::new())
        .unwrap();

    let mut initial_buffer = [0u8; 4]; // Buffer to read "HELO"
    if let Err(e) = stream.read_exact(&mut initial_buffer).await {
        error!("Failed to read initial HELO message: {:?}", e);
        return;
    }

    let id = chans.next();

    if let Some(id) = id {
        match &initial_buffer {
            b"SEND" => {
                if let Err(e) = stream.write_all(&(id as u16).to_le_bytes()).await {
                    error!("Failed to send ID back to client: {:?}", e);
                    chans.rm(id).expect("failed to remove channel counter");
                }
            }
            b"PLAY" => {
                if let Some(mut consumer) = streams.take_consumer(id) {
                    let mut next_instant = Instant::now() + Duration::from_millis(5);
                    loop {
                        tokio::time::sleep_until(next_instant).await;
                        next_instant += Duration::from_millis(2);
                        if let Ok(data) = consumer.pop() {
                            if let Err(e) = stream.write_all(&data).await {
                                error!("Failed to send ID back to client: {:?}", e);
                                chans.rm(id).expect("failed to remove channel counter");
                                return;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    } else {
        error!("No available channels");
        return;
    }

    loop {
        tokio::select! {
            read_result = stream.read(&mut read_buf) => {
                match read_result {
                    Ok(0) => {
                        // Client has closed the connection
                        break;
                    }
                    Ok(n) => {
                        buffer.extend_from_slice(&read_buf[..n]);

                        loop {
                            if current_frame_length.is_none() {
                                if buffer.len() < 12 {
                                    break;
                                }
                                let mut buf_cursor = &buffer[..];
                                let h = FrameHeader::decode(&mut buf_cursor).unwrap();
                                                             current_frame_length = Some(h.size() + (h.sample_size() * h.channels() as u16 * 4) as usize);
                                ms = (h.sample_size() as u64 * 1000) / h.sample_rate() as u64;
                            }
                            if let Some(len) = current_frame_length {
                                if buffer.len() >= len {
                                    let pkt_bytes = buffer.split_to(len).freeze();
                                    let request = EncodeRequest {
                                        buffer: pkt_bytes,
                                        dts,
                                    };
                                    if let Err(e) = encode_tx.send(request).await {
                                        error!("send error: {:?}", e);
                                    }
                                    dts += ms;
                                    current_frame_length = None;
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Read error: {:?}", e);
                        break;
                    }
                }
            }
            Some(response) = response_rx.recv() => {
                match ts_muxer.write(audio_pid, response.dts as i64, response.dts as i64, 0,  response.result.clone()) {
                        Ok(_) => {},
                        Err(e) => {
                            error!("Error writing audio: {}", e);
                        }
                    }

                let data = ts_muxer.get_data().freeze();

                if let Err(e) = tx.send(data).await {
                    error!("error sending to flac output chan: {:?}", e);
                }
            }
        }
    }

    if let Some(id) = id {
        chans.rm(id).expect("failed to remove channel counter");
    }

    println!("closed connection");
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundkit::audio_types::Endianness;
    use std::f32::consts::PI;
    use tokio::io::{AsyncWriteExt, BufWriter};
    use tokio::net::TcpStream;
    use tokio::test;

    /// Generate a sine wave as PCM data.
    fn generate_sine_wave(sample_rate: usize, frequency: f32, duration: usize) -> Vec<f32> {
        let total_samples = sample_rate * duration;
        (0..total_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (t * frequency * 2.0 * PI).sin()
            })
            .collect()
    }

    async fn send_audio_data(addr: &str, data: &[f32]) -> Result<(), Box<dyn std::error::Error>> {
        let samples_per_channel: usize = 1024;
        let num_channels: u8 = 1;
        let header = FrameHeader::new(
            EncodingFlag::PCMFloat,
            samples_per_channel as u16,
            48000,
            num_channels,
            32,
            Endianness::LittleEndian,
            Some(123456789),
        );

        let pcm_capacity: usize = samples_per_channel * num_channels as usize;

        let mut stream = TcpStream::connect(addr).await?;
        let mut writer = BufWriter::new(&mut stream);

        for chunk in data.chunks(pcm_capacity) {
            let mut header_data: Vec<u8> = Vec::new();
            header
                .encode(&mut header_data)
                .expect("Failed to encode header");

            for &sample in chunk {
                header_data.extend_from_slice(&sample.to_le_bytes());
            }
            writer.write_all(&header_data).await?;
        }

        writer.flush().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_send_sine_wave_to_server() {
        // Configuration for the sine wave
        let sample_rate = 48000; // Hz
        let frequency = 440.0; // A note in Hz
        let duration = 30; // Duration in seconds

        // Generate a 30-second sine wave at 440 Hz
        let sine_wave = generate_sine_wave(sample_rate, frequency, duration);

        // Optionally encode the sine wave if necessary
        // let encoded_data = encode_audio(&sine_wave, sample_rate, 2, EncodingFlag::FLAC).await.unwrap();

        // Address of the server (change to the actual server address)
        let server_addr = "127.0.0.1:4242";

        // Sed the sine wave data to the server
        match send_audio_data(server_addr, &sine_wave).await {
            Ok(_) => println!("Successfully sent sine wave to the server."),
            Err(e) => eprintln!("Failed to send sine wave: {:?}", e),
        }
    }
}
