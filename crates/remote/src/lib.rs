//! shards on another machine (D180): a home's identity, and the TLS its connections to
//! another home's daemon run under, over QUIC (RFC 9000, 9001, 9002), as ../hyper-raft's
//! hyper-quic and hyper-tls conform quinn-proto and rustls.

pub mod client;
pub mod frame;
pub mod identity;
pub mod invite;
pub mod net;
pub mod server;
pub mod tls;

/// What went wrong, said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod handshake {
    use std::net::SocketAddr;
    use std::time::Instant;

    use bytes::BytesMut;
    use hyper_quic::rustls::pki_types::CertificateDer;
    use hyper_quic::{
        Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, Event, StreamEvent,
    };

    use crate::identity::Identity;
    use crate::tls;

    /// An endpoint at `address`, and the connection it holds, once it holds one.
    struct Side {
        endpoint: Endpoint,
        address: SocketAddr,
        connection: Option<(ConnectionHandle, Connection)>,
        connected: bool,
        lost: Option<String>,
    }

    impl Side {
        fn events(&mut self) {
            let Some((handle, connection)) = self.connection.as_mut() else {
                return;
            };
            while let Some(event) = connection.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    connection.handle_event(event, self.endpoint.configs_mut());
                }
            }
            while let Some(event) = connection.poll() {
                match event {
                    Event::Connected => self.connected = true,
                    Event::ConnectionLost { reason } => self.lost = Some(reason.to_string()),
                    _ => {}
                }
            }
        }
    }

    /// Delivers what `from` has to send to `to`, as a network would; whether anything moved.
    fn deliver(now: Instant, from: &mut Side, to: &mut Side) -> bool {
        let mut moved = false;
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            let Some((_, connection)) = from.connection.as_mut() else {
                return moved;
            };
            let Some(transmit) = connection.poll_transmit(now, 1, &mut buffer, from.endpoint.configs())
            else {
                return moved;
            };
            moved = true;
            let datagram = BytesMut::from(&buffer[..transmit.size]);
            let mut out = Vec::new();
            match to
                .endpoint
                .handle(now, from.address, None, None, datagram, &mut out)
            {
                Some(DatagramEvent::NewConnection(incoming)) => {
                    let accepted = to.endpoint.accept(incoming, now, &mut out, None, None);
                    to.connection = Some(accepted.map_err(|e| e.cause).unwrap());
                }
                Some(DatagramEvent::ConnectionEvent(_, event)) => {
                    if let Some((_, connection)) = to.connection.as_mut() {
                        connection.handle_event(event, to.endpoint.configs_mut());
                    }
                }
                Some(DatagramEvent::Response(_)) | None => {}
            }
            to.events();
            from.events();
        }
    }

    fn pair(pin_server: bool) -> (Side, Side, Identity, Identity) {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let server = Endpoint::new(
            EndpointConfig::default(),
            Some(tls::server(&server_id).unwrap()),
            true,
            None,
        )
        .unwrap();
        let mut client = Endpoint::new(EndpointConfig::default(), None, true, None).unwrap();
        let pinned = if pin_server {
            server_id.fingerprint()
        } else {
            Identity::generate().unwrap().fingerprint()
        };
        let config = client
            .insert_client_config(tls::client(&client_id, pinned).unwrap())
            .unwrap();
        let server_address: SocketAddr = "127.0.0.1:7001".parse().unwrap();
        let client_address: SocketAddr = "127.0.0.1:7002".parse().unwrap();
        let now = Instant::now();
        let connection = client
            .connect(now, config, server_address, tls::SERVER_NAME, None)
            .unwrap();
        let mut server = Side {
            endpoint: server,
            address: server_address,
            connection: None,
            connected: false,
            lost: None,
        };
        let mut client = Side {
            endpoint: client,
            address: client_address,
            connection: Some(connection),
            connected: false,
            lost: None,
        };
        for _ in 0..16 {
            let a = deliver(now, &mut client, &mut server);
            let b = deliver(now, &mut server, &mut client);
            if !a && !b {
                break;
            }
        }
        (server, client, server_id, client_id)
    }

    /// Two homes connect with each other's raw public keys: the client pins the daemon's by
    /// its SHA-256, the daemon learns the client's, and a stream carries bytes each way.
    #[test]
    fn homes_connect_by_their_raw_keys() {
        let (mut server, mut client, _, client_id) = pair(true);
        assert!(
            client.connected && server.connected,
            "{:?} {:?}",
            client.lost,
            server.lost
        );
        let (_, sconn) = server.connection.as_mut().unwrap();
        let peer = sconn
            .crypto_session()
            .peer_identity()
            .unwrap()
            .downcast::<Vec<CertificateDer<'static>>>()
            .unwrap();
        assert_eq!(peer[0].as_ref(), client_id.spki());
        let (_, cconn) = client.connection.as_mut().unwrap();
        let id = cconn.streams().open(Dir::Bi).unwrap();
        cconn.send_stream(id).write(b"ping").unwrap();
        cconn.send_stream(id).finish().unwrap();
        let now = Instant::now();
        while deliver(now, &mut client, &mut server) || deliver(now, &mut server, &mut client) {}
        let (_, sconn) = server.connection.as_mut().unwrap();
        let accepted = sconn.streams().accept(Dir::Bi).unwrap();
        assert_eq!(accepted, id);
        let mut got = Vec::new();
        let mut recv = sconn.recv_stream(accepted);
        let mut chunks = recv.read(true).unwrap();
        while let Ok(Some(chunk)) = chunks.next(usize::MAX) {
            got.extend_from_slice(&chunk.bytes);
        }
        let _ = chunks.finalize();
        assert_eq!(got, b"ping");
        let _ = StreamEvent::Opened { dir: Dir::Bi };
    }

    /// Only the streams the protocol has open: a client's, both ways. Neither side may open
    /// a stream one way, nor the daemon a stream to its client, whose bytes would go unread.
    #[test]
    fn only_a_clients_two_way_streams_open() {
        let (mut server, mut client, _, _) = pair(true);
        assert!(client.connected && server.connected);
        let (_, sconn) = server.connection.as_mut().unwrap();
        assert!(sconn.streams().open(Dir::Uni).is_none());
        assert!(sconn.streams().open(Dir::Bi).is_none());
        let (_, cconn) = client.connection.as_mut().unwrap();
        assert!(cconn.streams().open(Dir::Uni).is_none());
        assert!(cconn.streams().open(Dir::Bi).is_some());
    }

    /// A daemon whose key is not the one pinned is refused at the handshake.
    #[test]
    fn a_daemon_not_pinned_is_refused() {
        let (server, client, _, _) = pair(false);
        assert!(!client.connected, "connected to a daemon it did not pin");
        assert!(client.lost.is_some() || server.lost.is_some());
    }
}
