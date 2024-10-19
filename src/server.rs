use access_unit::AccessUnit;
use bytes::{Bytes, BytesMut};
use futures::SinkExt;
use regex::Regex;
use soundkit::audio_packet::{encode_audio_packet, Decoder, Encoder, FrameHeader};
use soundkit::audio_types::EncodingFlag;
use soundkit_flac::{FlacDecoder, FlacEncoder};
use soundkit_opus::OpusEncoder;
use srt_tokio::SrtSocket;
use std::net::SocketAddr;
use std::str;
use std::time::Instant;
use tcp_changes::Payload;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::{oneshot, watch};
use tokio_stream::StreamExt;
use tracing::{error, info};
use ts::demuxer::TsDemuxer;
use xmpegts::{define::epsi_stream_type, ts::TsMuxer};

struct Stream {}
impl Stream {
    async fn start(mut rx: mpsc::Receiver<Bytes>, stream_key: &str) -> Result<(), std::io::Error> {
        let (tx, mut rrx) = mpsc::channel::<AccessUnit>(32);

        let demux_tx = TsDemuxer::start(tx.clone());

        let addr: &str = "local.wavey.io:8000";
        let mut socket = SrtSocket::builder()
            .call(addr, Some(stream_key))
            .await
            .unwrap();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut decoder = FlacDecoder::new();
                decoder.init().expect("Decoder initialization failed");

                while let Some(au) = rrx.recv().await {
                    let mut decoded_samples = vec![0i32; 1024 * 2];
                    let header = FrameHeader::decode(&mut &au.data[..20]).unwrap();
                    match decoder.decode_i32(&au.data, &mut decoded_samples, false) {
                        Ok(sample_count) => {
                            let mut result = Vec::with_capacity(sample_count);
                            for &s32_sample in &decoded_samples[..sample_count] {
                                let f32_sample = (s32_sample as f32) / (2.0f32.powi(23) - 1.0);
                                result.extend_from_slice(&f32_sample.to_le_bytes());
                            }
                            //   if let Err(e) = stx.try_send(result.clone()) {
                            //       error!("error sending: {}", e);
                            //   }

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
                   if let Err(e) = socket.send((Instant::now(), data)).await {
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

        tokio::task::spawn(async move {
            while let Some(payload) = rx.recv().await {
                if let Some(id) = extract_id(payload.val) {}
            }
        });

        tokio::task::spawn(async move {
            Stream::start(srt_rx, "flac").await;
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
                                tokio::task::spawn(async move {
                                    stream_handler(stream, srt_tx).await;
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

async fn stream_handler(mut stream: TcpStream, tx: mpsc::Sender<Bytes>) {
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

        // Send the sine wave data to the server
        match send_audio_data(server_addr, &sine_wave).await {
            Ok(_) => println!("Successfully sent sine wave to the server."),
            Err(e) => eprintln!("Failed to send sine wave: {:?}", e),
        }
    }
}
