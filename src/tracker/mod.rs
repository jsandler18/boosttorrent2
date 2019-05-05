use crate::boostencode::{DecodeError, FromValue, Value};
use log::trace;
use reqwest::{
    r#async::Client,
    StatusCode,
};
use std::fmt;
use std::net::{
    IpAddr,
    SocketAddr,
};
use tokio::await;
use tokio::prelude::*;

#[cfg(test)]
mod test;


pub struct Tracker {
    // The 20 byte unique identifier for this instance of the client
    peer_id: [u8; 20],
    // The uri of the tracker
    tracker_uri: String,
    // The SHA1 hash of the value of the info key in the torrent file
    info_hash: [u8; 20],
    // The port we will be listening on for peer connections
    port: u16,
    // A string the client should send on subsequent announcements
    tracker_id: Option<String>,
    // The shared state of the client
    // A future of the must recent tracker request
    request: Box<dyn Future<Item=TrackerResponse, Error=TrackerError> + Send>,
}

#[derive(Debug, PartialEq)]
pub struct PeerInfo {
    // the unique identifier for this peer
    pub peer_id: Option<[u8; 20]>,
    // The ip/port of this peer
    pub address: SocketAddr,
}

#[derive(Debug, PartialEq)]
pub struct TrackerSuccessResponse {
    // The number of seconds the client should wait before sending a regular request to the tracker
    pub interval: u32,
    // If present, clients must not re-announce more frequently than this
    pub min_interval: Option<u32>,
    // A string the client should send on subsequent announcements
    pub tracker_id: Option<String>,
    // Number of seeders
    pub complete: u32,
    // Number of leechers,
    pub incomplete: u32,
    // A list of peers that we could connect to
    pub peers: Vec<PeerInfo>,
}

#[derive(Debug, PartialEq)]
pub enum TrackerResponse {
    Failure(String),
    Warning(String, TrackerSuccessResponse),
    Success(TrackerSuccessResponse),
}


#[derive(Debug, derive_error::Error)]
pub enum TrackerError {
    /// Could not connect to the tracker
    ConnectionError(reqwest::Error),
    /// The tracker returned a non 200 status code
    #[error(non_std, no_from)]
    ResponseError(u16),
    /// The response body could not be bdecoded
    DecodeError(DecodeError),
    /// The contents of the response are not correct
    InvalidResponse,
}

pub enum Event {
    Started,
    Stopped,
    Completed,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", match self {
            Event::Started => "started",
            Event::Stopped => "stopped",
            Event::Completed => "completed"
        })
    }
}

impl FromValue for PeerInfo {
    type Error = String;

    fn from_value(val: &Value) -> Result<Self, Self::Error> {
        let map = val.dict().ok_or("Not a dictionary".to_string())?;

        let mut peer_id: [u8; 20] = [0; 20];
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

        Ok(PeerInfo {
            peer_id,
            address: SocketAddr::new(ip, port),
        })
    }
}

impl FromValue for TrackerResponse {
    type Error = String;

    fn from_value(val: &Value) -> Result<Self, Self::Error> {
        let map = val.dict().ok_or("Not a dictionary".to_string())?;

        if let Some(msg) = map.get("failure reason".as_bytes()) {
            return Err(msg.bstring_utf8().unwrap_or("unknown failure reason".to_string()));
        };

        let warning_msg = map.get("warning message".as_bytes()).and_then(Value::bstring_utf8);


        let interval = map.get("interval".as_bytes()).and_then(Value::integer)
            .map(|i| *i as u32)
            .ok_or("Missing key: interval".to_string())?;

        let min_interval = map.get("min interval".as_bytes()).and_then(Value::integer)
            .map(|i| *i as u32);

        let tracker_id = map.get("tracker id".as_bytes()).and_then(Value::bstring_utf8);

        let complete = map.get("complete".as_bytes()).and_then(Value::integer)
            .map(|i| *i as u32)
            .ok_or("Missing key: complete".to_string())?;

        let incomplete = map.get("incomplete".as_bytes()).and_then(Value::integer)
            .map(|i| *i as u32)
            .ok_or("Missing key: incomplete".to_string())?;

        let peers = match map.get("peers".as_bytes()).ok_or("Missing key: peers".to_string())? {
            // Dictionary model
            Value::List(peers) => peers.iter()
                .map(PeerInfo::from_value)
                .collect::<Result<Vec<_>, _>>()?,
            // Binary model
            Value::BString(peers) => peers.chunks(6)
                .map(|peer_slice| {
                    // port is in big endian.  multiply instead of bitshift so you can't mess up endianness
                    let port = (peer_slice[0] as u16 * 256) + peer_slice[1] as u16;
                    let mut ip_bytes: [u8; 4] = [0; 4];
                    ip_bytes.copy_from_slice(&peer_slice[..4]);
                    let ip: IpAddr = ip_bytes.into();
                    PeerInfo {
                        peer_id: None,
                        address: (ip, port).into(),
                    }
                })
                .collect(),
            _ => return Err("peers is not in the correct form".to_owned())
        };

        let res = TrackerSuccessResponse {
            interval,
            min_interval,
            tracker_id,
            complete,
            incomplete,
            peers,
        };

        match warning_msg {
            Some(msg) => Ok(TrackerResponse::Warning(msg, res)),
            None => Ok(TrackerResponse::Success(res))
        }
    }
}

pub async fn announce(event: Option<Event>,
                      uploaded: u64,
                      downloaded: u64,
                      left: u64,
                      tracker_id: Option<String>,
                      tracker_url: String,
                      peer_id: ::std::sync::Arc<[u8; 20]>,
                      info_hash: ::std::sync::Arc<[u8; 20]>,
                      listen_port: u16) -> Result<TrackerResponse, TrackerError> {
    let mut req_builder = Client::new().get(&tracker_url)
        .query(&[
            ("port", listen_port.to_string()),
            ("uploaded", uploaded.to_string()),
            ("downloaded", downloaded.to_string()),
            ("left", left.to_string())])
        .query(&[
            ("info_hash", &*info_hash),
            ("peer_id", &*peer_id)]);
    if let Some(e) = event {
        req_builder = req_builder.query(&[("event", e.to_string())]);
    }
    if let Some(id) = tracker_id {
        req_builder = req_builder.query(&[("trackerid", id.to_string())]);
    }
    let response = await!(req_builder.send())?;
    if response.status() != StatusCode::OK {
        return Err(TrackerError::ResponseError(response.status().as_u16()));
    }
    let body = await!(response.into_body().map(|chunk| Vec::from(&*chunk)).concat2())?;
    let bdecoded = Value::decode(&body)?;
    TrackerResponse::from_value(&bdecoded).map_err(|_| TrackerError::InvalidResponse)
}