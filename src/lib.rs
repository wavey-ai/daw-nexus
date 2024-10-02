use bytes::BytesMut;
use futures::SinkExt;
use soundkit::audio_packet::{encode_audio_packet, Encoder, FrameHeader};
use soundkit::audio_types::EncodingFlag;
use soundkit_flac::FlacEncoder;
use soundkit_opus::OpusEncoder;
use srt_tokio::SrtSocket;
use std::net::SocketAddr;
use std::time::Instant;
use tls_helpers::tls_acceptor_from_base64;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::{oneshot, watch};
use tokio_rustls::server;
use tracing::{error, info};
use xmpegts::{define::epsi_stream_type, ts::TsMuxer};

struct Stream {}
impl Stream {
    async fn start(mut rx: mpsc::Receiver<Packet>, stream_id: &str) -> Result<(), std::io::Error> {
        let mut ts_muxer = TsMuxer::new();
        let audio_pid = ts_muxer
            .add_stream(epsi_stream_type::PSI_STREAM_AAC, BytesMut::new())
            .unwrap();

        let addr = "local.wavey.io:8000";
        let mut socket = SrtSocket::builder()
            .call(addr, Some(stream_id))
            .await
            .unwrap();

        while let Some(pkt) = rx.recv().await {
            match ts_muxer.write(audio_pid, pkt.dts as i64, pkt.dts as i64, 0, pkt.data) {
                Ok(_) => {}
                Err(e) => {
                    error!("error writing audio: {}", e)
                }
            }

            let data = ts_muxer.get_data();

            if let Err(e) = socket.send((Instant::now(), data.freeze())).await {
                error!("Error sending SRT packet {:?}", e);
            }
        }

        Ok(())
    }
}

pub struct Server {
    cert_pem: String,
    privkey_pem: String,
}

impl Server {
    pub fn new(cert_pem: String, privkey_pem: String) -> Self {
        Self {
            cert_pem,
            privkey_pem,
        }
    }

    pub async fn start(
        &self,
        addr: SocketAddr,
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

        let (flac_tx, mut flac_rx) = mpsc::channel::<Packet>(100);
        let (opus_tx, mut opus_rx) = mpsc::channel::<Packet>(100);

        tokio::task::spawn(async move {
            //Stream::start(opus_rx, "opus").await;
        });

        tokio::task::spawn(async move {
            Stream::start(flac_rx, "flac").await;
        });

        let tls_acceptor =
            tls_acceptor_from_base64(&self.cert_pem, &self.privkey_pem, false, false).unwrap();
        let incoming = TcpListener::bind(addr).await.unwrap();
        up_tx.send(()).unwrap();

        let srv = async move {
            loop {
                tokio::select! {
                    result = incoming.accept() => {
                        match result {
                            Ok((stream, _)) => {
                                let tls_acceptor = tls_acceptor.clone();
                                let flac_tx = flac_tx.clone();
                                let opus_tx = opus_tx.clone();

                                tokio::task::spawn(async move {
                                    match tls_acceptor.accept(stream).await {
                                        Ok(stream) => {
                                            stream_handler(stream, flac_tx, opus_tx).await;
                                        }
                                        Err(err) => {
                                           error!("tcp stream error: {:?}", err);
                                        }
                                    }
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

struct Packet {
    data: BytesMut,
    dts: u64,
}

struct EncodeRequest {
    buffer: Vec<u8>,
    dts: u64,
}

struct EncodeResponse {
    result: BytesMut,
    encoding: EncodingFlag,
    dts: u64,
}

async fn stream_handler(
    mut stream: server::TlsStream<TcpStream>,
    flac_tx: mpsc::Sender<Packet>,
    opus_tx: mpsc::Sender<Packet>,
) {
    let (encode_tx, mut encode_rx) = mpsc::channel::<EncodeRequest>(100);
    let (response_tx, mut response_rx) = mpsc::channel::<EncodeResponse>(100);
    std::thread::spawn(move || {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let mut flac = FlacEncoder::new(48000, 24, 2, 256 * 2, 1);
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
                            panic!("error getting flac packet!");
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
                        dbg!(e);
                    }
                }
            }
        });
    });

    let mut buffer = [0; 4048 * 20];

    loop {
        select! {
            result = stream.read(&mut buffer) => {
                match result {
                    Ok(n) if n == 0 => {
                        break;
                    }
                    Ok(n) => {
                        if n > 8 {
                            let dts = u64::from_le_bytes(buffer[..8].try_into().unwrap());
                            let request = EncodeRequest {
                                buffer: buffer[8..n].to_vec(),
                                dts,
                            };

                            if encode_tx.send(request).await.is_err() {
                                error!("Failed to send encode request");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading from stream: {:?}", e);
                        break;
                    }
                }
            }
            Some(response) = response_rx.recv() => {
                let packet = Packet {
                    data: response.result.clone(),
                    dts: response.dts,
                };

                match response.encoding {
                    EncodingFlag::Opus => {
                        if let Err(e) = opus_tx.send(packet).await {
                            error!("error sending to opus output chan: {:?}", e);
                        }
                    }
                    EncodingFlag::FLAC => {
                        if let Err(e) = flac_tx.send(packet).await {
                            error!("error sending to flac output chan: {:?}", e);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use soundkit::audio_bytes::s24le_to_i32;
    use soundkit::audio_packet::FrameHeader;
    use soundkit::audio_types::{EncodingFlag, Endianness};
    use soundkit::wav::WavStreamProcessor;
    use std::env;
    use std::fs::File;
    use std::io::Read;
    use std::net::SocketAddr;
    use tls_helpers::tls_connector_from_base64;
    use tokio;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_server() {
        let ca_cert = env::var("FULLCHAIN_PEM").unwrap();

        let connector = tls_connector_from_base64(&ca_cert).unwrap();
        let addr: SocketAddr = ([0, 0, 0, 0], 4242).into();
        let stream = TcpStream::connect(addr).await.unwrap();
        let dns_name = "local.wavey.io".to_string();
        let dns = dns_name.clone();
        let domain = ServerName::try_from(dns)
            .map_err(|_| "Invalid DNS name")
            .unwrap();
        let mut stream = connector.connect(domain, stream).await.unwrap();

        let file_path = "testdata/lori.wav";
        let frame_len = 256 * 2;
        let mut file = File::open(file_path.clone()).unwrap();
        let mut file_buffer = Vec::new();
        file.read_to_end(&mut file_buffer).unwrap();

        let mut processor = WavStreamProcessor::new();
        let audio_data = processor.add(&file_buffer).unwrap().unwrap();
        let chunk_size = frame_len * audio_data.channel_count() as usize;
        // Calculate the time duration of each chunk in seconds
        let chunk_duration = chunk_size as f64
            / (audio_data.sampling_rate() as f64 * audio_data.channel_count() as f64);
        // Convert chunk duration to 90 MHz clock cycles
        let dts_increment = (chunk_duration * 90_000.0) as u64;

        let mut dts: u64 = 0;

        let i32_samples = match audio_data.bits_per_sample() {
            24 => s24le_to_i32(audio_data.data()),
            _ => unreachable!(),
        };
        let header = FrameHeader::new(
            EncodingFlag::PCMSigned,
            0,
            audio_data.sampling_rate(),
            audio_data.channel_count(),
            32,
            Endianness::LittleEndian,
            Some(123456789),
        );

        for (i, chunk) in i32_samples.chunks(chunk_size).enumerate() {
            let mut packet_data = Vec::new();
            packet_data.extend_from_slice(&dts.to_le_bytes());
            header
                .encode(&mut packet_data)
                .expect("Failed to encode header");
            packet_data.extend(chunk.iter().flat_map(|&sample| sample.to_le_bytes()));
            stream.write_all(&packet_data).await.unwrap();
            println!("Sent packet {}", i);
            dts += dts_increment;
            sleep(Duration::from_millis(1)).await;
        }

        println!("All packets sent. Waiting for 5 seconds before shutting down.");
        sleep(Duration::from_secs(5)).await;
    }
}
