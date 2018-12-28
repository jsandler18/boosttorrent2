use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;

use byteorder::{ByteOrder, NetworkEndian};
use futures::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::sync::oneshot::{self, Receiver};
use tokio::codec::Framed;
use tokio::net::TcpStream;
use tokio::prelude::{Future, Sink, Stream};

use crate::boostencode::{FromValue, Value};
use crate::peer::channel_sink::ChannelSink;
use crate::peer::protocol::Message;

mod handshake;
pub mod protocol;
mod channel_sink;

// asymmetrical because started cares about the channels but stopped doesn't
pub enum PeerLifecycleEvent {
    Started(Peer),
    Stopped(PeerId),
}

#[derive(Debug)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub peer_id: Option<PeerId>,
}

pub type PeerId = [u8; 20];

pub struct Peer {
    peer_id: PeerId,
    tx: RefCell<UnboundedSender<Message>>,
}

impl PartialEq for Peer {
    fn eq(&self, other: &Peer) -> bool {
        self.peer_id == other.peer_id
    }
}

impl Eq for Peer {}

impl Hash for Peer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.peer_id.hash(state);
    }
}

impl Peer {
    pub fn new(peer_id: PeerId, tx: UnboundedSender<Message>) -> Peer {
        Peer { peer_id, tx: RefCell::new(tx) }
    }

    pub fn choke(&self) {
        self.tx.borrow().unbounded_send(Message::Choke)
            .map_err(|e| println!("Peer {:?} choke error {}", self.peer_id, e))
            .unwrap();
    }

    pub fn interested(&self, is_interested: bool) {
        self.tx.borrow().unbounded_send(if is_interested { Message::Interested } else { Message::NotInterested })
            .map_err(|e| println!("Peer {:?} interested error {}", self.peer_id, e))
            .unwrap();
    }

    pub fn close(&self) {
        self.tx.borrow_mut().close();
    }
}

impl FromValue for PeerInfo {
    type Error = String;

    fn from_value(val: &Value) -> Result<Self, Self::Error> where Self: Sized {
        let map = val.dict().ok_or("Not a dictionary".to_string())?;

        let mut peer_id = [0u8; 20];
        map.get("peer id".as_bytes()).and_then(Value::bstring)
            .map(|bytes| peer_id.copy_from_slice(bytes))
            .ok_or("Missing key: peer id".to_string())?;
        let peer_id = Some(peer_id);

        let ip = map.get("ip".as_bytes()).and_then(Value::bstring_utf8)
            .map(|s| s.parse())
            .ok_or("Missing key: ip".to_string())?
            .map_err(|_| "Invalid ip addr".to_string())?;

        let port = map.get("port".as_bytes()).and_then(Value::integer)
            .map(|i| *i as u16)
            .ok_or("Missing key: port".to_string())?;

        Ok(PeerInfo { addr: SocketAddr::new(ip, port), peer_id })
    }
}

impl PeerInfo {
    pub fn from_compact(bytes: &[u8]) -> Vec<PeerInfo> {
        bytes.chunks(6).map(|chunk| {
            let port = NetworkEndian::read_u16(&chunk[4..]);
            let mut ip = [0; 4];
            ip.copy_from_slice(&chunk[..4]);

            PeerInfo {
                addr: (ip, port).into(),
                peer_id: None,
            }
        }).collect()
    }

    pub fn connect(&self, info_hash: [u8; 20], our_peer_id: [u8; 20], output_sender: crossbeam_channel::Sender<Message>) -> (Receiver<Peer>, impl Future<Item=(), Error=()>) {
        // build a future that handshakes a peer
        // then wraps the socket in a peer protocol codec and writes to a channel

        // should find out how to close all these things at some point

        let (input_sender, input_receiver) = mpsc::unbounded();
        let (peer_sender, peer_receiver) = oneshot::channel();

        let peer = TcpStream::connect(&self.addr)
            .and_then(move |socket| {
                let (write, read) = Framed::new(socket, handshake::HandshakeCodec::new()).split();

                write.send((info_hash, our_peer_id).into())
                    .map(|write| (write, read))
            })
            .and_then(|(write, read)| {
                read.take(1).into_future()
                    .map(|(handshake, read)| (handshake, read.into_inner(), write))
                    .map_err(|(e, _)| e)
            })
            .and_then(move |(maybe_handshake, read, write)| match maybe_handshake {
                Some(ref handshake) if handshake.info_hash == info_hash => futures::future::ok((handshake.peer_id, read.reunite(write).unwrap().into_inner())),
                _ => {
                    println!("invalid handshake");
                    futures::future::err(std::io::Error::new(std::io::ErrorKind::Other, "invalid handshake"))
                }
            })
            .map_err(|e| println!("error: {}", e))
            .and_then(|(their_peer_id, socket)| {
                peer_sender.send(Peer::new(their_peer_id, input_sender));
                let (socket_output, socket_input) = Framed::new(socket, protocol::MessageCodec::new()).split();
                println!("valid handshake, reframing");
                let output = input_receiver.forward(socket_output.sink_map_err(|e| println!("socket output error: {}", e)));
                let input = socket_input
                    .map_err(|e| println!("socket receive error: {}", e))
                    .forward(ChannelSink::new(output_sender).sink_map_err(|e| println!("output send error: {}", e)));

                // oof that error type
                output.select2(input).map_err(|_| println!("peer io error"))
            });

        (peer_receiver, peer.map(|_| ()))
    }
}

pub fn handle_stream(socket: TcpStream, info_hash: [u8; 20], our_peer_id: PeerId, output_sender: crossbeam_channel::Sender<Message>, peer_lifecycle_sender: crossbeam_channel::Sender<PeerLifecycleEvent>) -> impl Future<Item=(), Error=()> {
    let (input_sender, input_receiver) = mpsc::unbounded();

    let (write, read) = Framed::new(socket, handshake::HandshakeCodec::new()).split();

    // handshake
    write.send((info_hash, our_peer_id).into())
        .and_then(|write| {
            read.take(1).into_future()
                .map(|(handshake, read)| (handshake, read.into_inner(), write))
                .map_err(|(e, _)| e)
        })
        .and_then(move |(maybe_handshake, read, write)| match maybe_handshake {
            Some(ref handshake) if handshake.info_hash == info_hash => futures::future::ok((handshake.peer_id, read.reunite(write).unwrap().into_inner())),
            _ => {
                eprintln!("invalid handshake");
                futures::future::err(std::io::Error::new(std::io::ErrorKind::Other, "invalid handshake"))
            }
        })
        .map_err(|e| eprintln!("error while handshaking: {}", e))
        .and_then(move |(their_peer_id, socket)| {
            let _ignore = peer_lifecycle_sender.send(PeerLifecycleEvent::Started(Peer::new(their_peer_id, input_sender)));

            let (socket_output, socket_input) = Framed::new(socket, protocol::MessageCodec::new()).split();
            let output = input_receiver.forward(socket_output.sink_map_err(|e| eprintln!("socket output error: {}", e)));
            let input = socket_input
                .map_err(|e| eprintln!("socket receive error: {}", e))
                .forward(ChannelSink::new(output_sender).sink_map_err(|e| eprintln!("output send error: {}", e)));

            let peer_lifecycle_sender_clone = peer_lifecycle_sender.clone();
            output.select2(input)
                .map_err(|_| eprintln!("peer io error"))
                .map(|_| ())
                .then(move |r| {
                    let _ignore = peer_lifecycle_sender_clone.send(PeerLifecycleEvent::Stopped(their_peer_id));
                    r
                })
        })
}
