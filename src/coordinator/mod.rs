use crate::metainfo::MetaInfo;
use crate::tracker::{
    announce,
    Event,
    TrackerResponse,
    TrackerSuccessResponse,
};
use std::str::FromStr;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::await;
use tokio::prelude::*;
use log::{
    error,
    warn
};



pub async fn init_coordinator(meta: MetaInfo, peer_id: [u8; 20]) {
    let address = SocketAddr::from_str("0.0.0.0:6888").unwrap();
    let download_size = meta.info.file_info.size() as u64;
    let info_hash = Arc::new(meta.info_hash);
    let peer_id = Arc::new(peer_id);
    let tracker_response = await!(announce(Some(Event::Started), 0, 0, download_size,
                                    None, meta.announce.clone(), peer_id.clone(), info_hash.clone(), 6888));
    let tracker_response = match tracker_response {
        Err(e) => {
            error!("Error communicating with tracker: {}", e);
            return
        },
        Ok(TrackerResponse::Failure(s)) => {
            error!("Tracker returned an error response: {}",s );
            return
        },
        Ok(TrackerResponse::Warning(s, resp)) => {
            warn!("Tracker returned a warning: {}", s);
            resp
        },
        Ok(TrackerResponse::Success(resp)) => resp
    };
    // TODO get some initial peers
    let peers = Vec::new();

    await!(event_loop(meta, peers, tracker_response, peer_id, info_hash));
}

async fn event_loop(meta: MetaInfo,
              initial_peers: Vec<()>,
              initial_tracker_response: TrackerSuccessResponse,
              peer_id: Arc<[u8;20]>,
              info_hash: Arc<[u8;20]>) {

}