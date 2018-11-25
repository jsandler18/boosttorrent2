extern crate bit_vec;
extern crate byteorder;
extern crate bytes;
#[warn(unused_extern_crates)]
extern crate clap;
extern crate crossbeam_channel;
extern crate crypto;
extern crate derive_error;
extern crate futures;
extern crate log;
extern crate maplit;
extern crate rand;
extern crate reqwest;
extern crate serde;
extern crate simple_logger;
extern crate tokio;

use std::{
    fs::File,
    io::Read,
};

use clap::App;
use clap::load_yaml;
use futures::Future;
use futures::stream::Stream;
use log::{
    debug,
    error,
    Level,
    warn,
};
use rand::prelude::*;
use simple_logger::init_with_level;

use boostencode::{FromValue, Value};

mod boostencode;
mod metainfo;
mod tracker2;
mod peer;

#[cfg(not(feature = "experiment"))]
fn main() {
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml).get_matches();

    match matches.occurrences_of("verbose") {
        0 => init_with_level(Level::Error),
        1 => init_with_level(Level::Warn),
        2 => init_with_level(Level::Info),
        3 => init_with_level(Level::Debug),
        4 | _ => init_with_level(Level::Trace),
    }.unwrap();

    if matches.is_present("garbage-mode") {
        warn!("Garbage mode activated");
    }

    if !matches.is_present("torrent-file") {
        error!("No torrent file provided");
        return;
    }

    let string = matches.value_of("torrent-file").unwrap();
    let mut f = File::open(string).expect("file not found");
    let mut contents = Vec::new();
    f.read_to_end(&mut contents).expect("error reading file");
    let val = Value::decode(contents.as_ref()).unwrap();
    debug!("{}", val);

    let metainfo = metainfo::MetaInfo::from_value(&val).unwrap();

    let peer_id = gen_peer_id();


    let stats = tracker2::Stats {
        uploaded: 0,
        downloaded: 0,
        left: metainfo.info.file_info.size() as u64,
    };

    let addr = reqwest::Url::parse(&metainfo.announce).unwrap();
    let tracker_info = tracker2::Tracker::new(addr, metainfo.info_hash, peer_id, 6881);

    let coordinator = tracker_info.send_event(&stats, tracker2::Event::Started)
        .map_err(|_| ())
        .map(|res| {
            println!("Started: {:?}", res);

            // eventually spawn a bunch of tasks and channels here

            ()
        })
        .and_then(|_| {
            let (fp, rx) = file_progress::FileProgress::new();

            tokio::spawn(fp);

            rx.take(1).into_future().map(|_| ()).map_err(|_| ())
        })
        .and_then(move |_| {
            tracker_info.send_event(&stats, tracker2::Event::Stopped).map_err(|_| ())
        })
        .map(|res| {
            println!("Stopped: {:?}", res);
        });

    tokio::run(coordinator);
}

#[cfg(feature = "experiment")]
fn main() {
    let peer_info = peer::PeerInfo {
        addr: std::net::SocketAddr::new("127.0.0.1".parse().unwrap(), 8080),
        peer_id: None,
    };

    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let (peer_tx, mut rx) = crossbeam_channel::unbounded();

    let (peer_receiver, peer_future) = peer_info.connect(*b"ThisIsGoodForBitcoin", gen_peer_id(), peer_tx.clone());

    // obviously only drop once no more peers are expected to connect
    // but for this experiment we have to drop it so we can exit after a time;
    drop(peer_tx);

    rt.spawn(peer_future);

    let peer = peer_receiver.wait().unwrap();

    println!("choking");
    peer.choke();

    println!("am interested");
    peer.interested(true);

    std::thread::spawn(move || {
        use futures::Sink;
        println!("Closing in five seconds");
        std::thread::sleep(std::time::Duration::from_secs(5));
        println!("Closing");
        peer.close();
    });

    loop {
        match rx.try_recv() {
            Ok(message) => println!("Received: {:?}", message),
            Err(crossbeam_channel::TryRecvError::Empty) => {
                // nothing
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
        }


        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!("waiting for runtime");
    rt.shutdown_on_idle().wait().unwrap();

    println!("done");
}

fn gen_peer_id() -> [u8; 20] {
    // Generate peer id in Azures style ("-<2 letter client code><4 digit version number>-<12 random digits>")
    let mut id = "-BO0001-".to_owned();
    for _ in 0..12 {
        id.push_str(&thread_rng().gen_range::<u8>(0, 9).to_string());
    }
    let mut res = [0; 20];
    res.copy_from_slice(id.as_bytes());
    res
}

mod file_progress {
    use futures::Async;
    use futures::Future;
    use futures::sink::Sink;
    use futures::sync::mpsc;
    use rand::Rng;

    pub struct FileProgress {
        pub progress: f64,
        tx: mpsc::UnboundedSender<()>,
    }

    impl FileProgress {
        pub fn new() -> (FileProgress, mpsc::UnboundedReceiver<()>) {
            let (tx, rx) = mpsc::unbounded();

            let fp = FileProgress {
                progress: 0.0,
                tx,
            };

            (fp, rx)
        }
    }

    impl Future for FileProgress {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
            if self.progress >= 100.0f64 {
                self.tx.unbounded_send(()).unwrap();
                let _ = self.tx.close();
                return Ok(Async::Ready(()));
            }

            self.progress += if rand::thread_rng().gen_bool(0.1) { 0.1 } else { 0.0 };
            Ok(Async::NotReady)
        }
    }
}