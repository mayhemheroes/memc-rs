use async_listen::{
    backpressure::Token, 
    error_hint, ListenExt
};
use futures::{
    future::FutureExt, // for `.fuse()`    
    select,
};
use futures_util::sink::SinkExt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::io;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs as TokioToSocketAddrs};
use tokio::stream::{StreamExt as TokioStreamExt};
use tokio::time::{interval_at, timeout, Instant};
use tokio_util::codec::{FramedRead, FramedWrite};

use super::handler;
use super::storage;
use super::timer;
use super::timer::{SetableTimer, Timer};
use crate::protocol::binary_codec;

pub struct TcpServer {
    timer: Arc<timer::SystemTimer>,
    storage: Arc<storage::Storage>,
    timeout_secs: u64,
    connection_limit: u32,
}

impl Default for TcpServer {
    fn default() -> Self {
        let timer = Arc::new(timer::SystemTimer::new());
        TcpServer {
            connection_limit: 100,
            timeout_secs: 60,
            timer: timer.clone(),
            storage: Arc::new(storage::Storage::new(timer)),
        }
    }
}

struct Client {
    store: Arc<storage::Storage>,
    socket: TcpStream,
    addr: SocketAddr,
    token: async_listen::backpressure::Token,
    rx_timeout_secs: u64,
    wx_timeout_secs: u64,
}

impl Client {
    pub fn new(
        store: Arc<storage::Storage>,
        socket: TcpStream,
        addr: SocketAddr,
        token: async_listen::backpressure::Token,
        rx_timeout_secs: u64,
        wx_timeout_secs: u64,
    ) -> Self {
        Client {
            store,
            socket,
            addr,
            token,
            rx_timeout_secs,
            wx_timeout_secs,
        }
    }
}

impl TcpServer {
    pub fn new() -> TcpServer {
        Default::default()
    }

    pub async fn run<A: ToSocketAddrs + TokioToSocketAddrs>(&mut self, addr: A) -> io::Result<()> {
        let mut listener = TcpListener::bind(addr).await?;
        // TODO: limit number of accepted connections just like memcache
        let mut incoming = listener
            .incoming()
            .log_warnings(log_accept_error)
            .handle_errors(Duration::from_millis(500)) // 1
            .backpressure(self.connection_limit as usize);

        let start = Instant::now();
        let mut interval = interval_at(start, Duration::from_secs(1));
        loop {
            select! {
                connection = incoming.next().fuse() => {

                    match connection {
                        Some((token, mut socket)) => {
                            let peer_addr = socket.peer_addr().unwrap();
                            let client = Client::new(
                                self.storage.clone(),
                                socket,
                                peer_addr,
                                token,
                                self.timeout_secs,
                                self.timeout_secs,
                            );
                            // Like with other small servers, we'll `spawn` this client to ensure it
                            // runs concurrently with all other clients. The `move` keyword is used
                            // here to move ownership of our db handle into the async closure.
                            tokio::spawn(async move { TcpServer::handle_client(client).await });
                        }
                        None => { }
                    }
                },
                _ = interval.tick().fuse() => {
                    self.timer.add_second();
                    debug!("Timer tick: {}", self.timer.secs());
                },
            }
        }
    }

    async fn handle_client(mut client: Client) {
        let mut handler = handler::BinaryHandler::new(client.store);
        //
        let (rx, tx) = client.socket.split();

        let mut reader = FramedRead::new(rx, binary_codec::MemcacheBinaryCodec::new());
        let mut writer = FramedWrite::new(tx, binary_codec::MemcacheBinaryCodec::new());

        // Here for every packet we get back from the `Framed` decoder,
        // we parse the request, and if it's valid we generate a response
        // based on the values in the storage.
        loop {
            match timeout(Duration::from_secs(client.rx_timeout_secs), reader.next()).await {
                Ok(req_or_none) => {
                    match req_or_none {
                        Some(req_or_error) => match req_or_error {
                            Ok(request) => {
                                let response = handler.handle_request(request);
                                if let Some(response) = response {
                                    if let Err(e) = timeout(
                                        Duration::from_secs(client.wx_timeout_secs),
                                        writer.send(response),
                                    )
                                    .await
                                    {
                                        error!("error on sending response; error = {:?}", e);
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error decoding msg from socket; error = {:?}", e);
                            }
                        },
                        None => {
                            // The connection will be closed at this point as `lines.next()` has returned `None`.
                            info!("Conneciton closed: {}", client.addr);
                            return;
                        }
                    }
                }
                Err(err) => {
                    info!(
                        "Timeout {}s elapsed, disconecting client: {}, error: {}",
                        client.rx_timeout_secs, client.addr, err
                    );
                    return;
                }
            }
        }
    }
}

fn log_accept_error(e: &io::Error) {
    error!("Error: {}. Listener paused for 0.5s. {}", e, error_hint(e)); // 3
}
