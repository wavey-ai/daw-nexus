use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tcp_changes::Client;

#[tokio::main]
async fn main() {
    const ENV_FILE: &str = include_str!("../.env");

    for line in ENV_FILE.lines() {
        if let Some((key, value)) = line.split_once('=') {
            env::set_var(key.trim(), value.trim());
        }
    }

    let cert = env::var("CERT_PEM").unwrap();
    let privkey = env::var("PRIVKEY_PEM").unwrap();
    let fullchain = env::var("FULLCHAIN_PEM").unwrap();

    let domain = "local.wavey.io".to_owned();
    let tcp_port = 4243;
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), tcp_port);

    let mb = Client::new(domain.clone(), socket, fullchain.to_owned());
    let (mut up_tcp, fin_tcp, mut shutdown_tcp, mut rx) = mb.start("HELLO").await.unwrap();

    //    let mq = daw_nexus::Server::new(cert, privkey);
    let mq = daw_nexus::server::Server::new();
    let addr: SocketAddr = ([0, 0, 0, 0], 4242).into();
    let (up, fin, shutdown) = mq.start(addr, rx).await.unwrap();
    fin.await.unwrap();
}
