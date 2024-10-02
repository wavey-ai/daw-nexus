use rustls::pki_types::ServerName;
use soundkit::audio_bytes::s24le_to_i16;
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

#[tokio::main]
async fn main() {
    let cert = env::var("CERT_PEM").unwrap();
    let privkey = env::var("PRIVKEY_PEM").unwrap();

    let mq = daw_nexus::Server::new(cert, privkey);
    let addr: SocketAddr = ([0, 0, 0, 0], 4242).into();
    let (up, fin, shutdown) = mq.start(addr).await.unwrap();
    fin.await.unwrap();
}
