use std::{thread, fmt, fs, io, time};
use std::io::{Read};
use {rpc, tracker, disk, DISK, RPC, CONFIG, TC, TRACKER};
use amy::{self, Poller, Registrar};
use torrent::{self, Torrent, Peer};
use std::collections::HashMap;
use bencode::BEncode;
use std::sync::{Arc, Mutex};
use throttle::Throttler;

pub struct Control {
    trk_rx: amy::Receiver<tracker::Response>,
    disk_rx: amy::Receiver<disk::Response>,
    ctrl_rx: amy::Receiver<Request>,
    throttler: Throttler,
    reg: Arc<Registrar>,
    poll: Poller,
    tid_cnt: usize,
    tracker_update: time::Instant,
    unchoke_update: time::Instant,
    session_update: time::Instant,
    job_timer: usize,
    torrents: HashMap<usize, Torrent>,
    peers: HashMap<usize, usize>,
    hash_idx: HashMap<[u8; 20], usize>,
}

pub struct Handle {
    pub trk_tx: Mutex<amy::Sender<tracker::Response>>,
    pub disk_tx: Mutex<amy::Sender<disk::Response>>,
    pub ctrl_tx: Mutex<amy::Sender<Request>>,
}

unsafe impl Sync for Handle {}

pub enum Request {
    AddTorrent(BEncode),
    AddPeer(Peer, [u8; 20]),
    RPC(rpc::Request),
    Shutdown,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Request")
    }
}

impl Control {
    pub fn new(poll: Poller,
               trk_rx: amy::Receiver<tracker::Response>,
               disk_rx: amy::Receiver<disk::Response>,
               ctrl_rx: amy::Receiver<Request>) -> Control {
        let torrents = HashMap::new();
        let peers = HashMap::new();
        let hash_idx = HashMap::new();
        let reg = Arc::new(poll.get_registrar().unwrap());
        // Every minute check to update trackers;
        let tracker_update = time::Instant::now();
        let unchoke_update = time::Instant::now();
        let session_update = time::Instant::now();
        let job_timer = reg.set_interval(1000).unwrap();
        // 5 MiB max bucket
        let throttler = Throttler::new(0, 0, 1 * 1024 * 1024, &reg);
        Control { trk_rx, disk_rx, ctrl_rx, poll, torrents, peers,
        hash_idx, reg, tid_cnt: 0, throttler, tracker_update,
        unchoke_update, session_update, job_timer }
    }

    pub fn run(&mut self) {
        if self.deserialize().is_err() {
            println!("Session deserialization failed!");
        }
        loop {
            for event in self.poll.wait(3).unwrap() {
                if self.handle_event(event) {
                    self.serialize();
                    println!("Control shutting down!");
                    return;
                }
            }
        }
    }

    fn serialize(&mut self) {
        for (_, torrent) in self.torrents.iter() {
            torrent.serialize();
        }
    }

    fn deserialize(&mut self) -> io::Result<()> {
        let ref sd = CONFIG.get().session;
        for entry in fs::read_dir(sd)? {
            if self.deserialize_torrent(entry).is_err() {
                println!("Failed to deserialize torrent!");
            }
        }
        Ok(())
    }

    fn deserialize_torrent(&mut self, entry: io::Result<fs::DirEntry>) -> io::Result<()> {
        let dir = entry?;
        let mut f = fs::File::open(dir.path())?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;

        let tid = self.tid_cnt;
        let r = self.reg.clone();
        let throttle = self.throttler.get_throttle();
        if let Ok(t) = Torrent::deserialize(tid, &data, throttle, r) {
            self.hash_idx.insert(t.info.hash, tid);
            self.tid_cnt += 1;
            self.torrents.insert(tid, t);
        } else {
            println!("Failed to deserialize torrent!");
        }
        Ok(())
    }

    fn handle_event(&mut self, not: amy::Notification) -> bool{
        match not.id {
            id if id == self.trk_rx.get_id() => self.handle_trk_ev(),
            id if id == self.disk_rx.get_id() => self.handle_disk_ev(),
            id if id == self.ctrl_rx.get_id() => return self.handle_ctrl_ev(),
            id if id == self.throttler.id() => self.throttler.update(),
            id if id == self.throttler.fid() => self.flush_blocked_peers(),
            id if id == self.job_timer => self.update_jobs(),
            _ => self.handle_peer_ev(not),
        }
        return false;
    }

    fn handle_trk_ev(&mut self) {
        loop {
            match self.trk_rx.try_recv() {
                Ok((id, resp)) => {
                    {
                        let torrent = self.torrents.get_mut(&id).unwrap();
                        torrent.set_tracker_response(&resp);
                    }
                    if let Ok(r) = resp {
                        for ip in r.peers.iter() {
                            if let Ok(peer) = Peer::new_outgoing(ip) {
                                self.add_peer(id, peer);
                            }
                        }
                    }
                }
                Err(_) => { break; }
            }
        }
    }

    fn update_jobs(&mut self) {
        if self.tracker_update.elapsed() > time::Duration::from_secs(60) {
            self.update_trackers();
            self.tracker_update = time::Instant::now();
        }

        if self.unchoke_update.elapsed() > time::Duration::from_secs(1) {
            self.unchoke_peers();
            self.unchoke_update = time::Instant::now();
        }

        if self.session_update.elapsed() > time::Duration::from_secs(1) {
            self.serialize();
            self.session_update = time::Instant::now();
        }
    }

    fn update_trackers(&mut self) {
        for (_, torrent) in self.torrents.iter_mut() {
            torrent.update_tracker();
        }
    }

    fn unchoke_peers(&mut self) {
        for (_, torrent) in self.torrents.iter_mut() {
            torrent.update_unchoked();
        }
    }

    fn handle_disk_ev(&mut self) {
        loop {
            match self.disk_rx.try_recv() {
                Ok(resp) => {
                    let pid = resp.context.id;
                    let tid = self.peers[&pid];
                    let ref mut torrent = self.torrents.get_mut(&tid).unwrap();
                    torrent.block_available(pid, resp).unwrap();
                }
                Err(_) => { break; }
            }
        }
    }

    fn handle_ctrl_ev(&mut self) -> bool {
        loop {
            match self.ctrl_rx.try_recv() {
                Ok(Request::AddTorrent(b)) => {
                    if let Ok(i) = torrent::Info::from_bencode(b) {
                        self.add_torrent(i);
                    }
                }
                Ok(Request::AddPeer(p, hash)) => {
                    let tid = *self.hash_idx.get(&hash).unwrap();
                    self.add_peer(tid, p);
                }
                Ok(Request::RPC(r)) => {
                    self.handle_rpc(r);
                }
                Ok(Request::Shutdown) => {
                    return true;
                }
                Err(_) => { break; }
            }
        }
        return false;
    }

    fn handle_peer_ev(&mut self, not: amy::Notification) {
        let pid = not.id;
        if not.event.readable() {
            let res = {
                let torrent = self.torrents.get_mut(&self.peers[&pid]).unwrap();
                torrent.peer_readable(pid)
            };
            if res.is_err() {
                self.remove_peer(pid);
                return;
            }
        }
        if not.event.writable() {
            let res = {
                let torrent = self.torrents.get_mut(&self.peers[&pid]).unwrap();
                torrent.peer_writable(pid)
            };
            if res.is_err() {
                self.remove_peer(pid);
            }
        }
    }

    fn flush_blocked_peers(&mut self) {
        for pid in self.throttler.flush_dl() {
            let res = {
                let torrent = self.torrents.get_mut(&self.peers[&pid]).unwrap();
                torrent.peer_readable(pid)
            };
            if res.is_err() {
                self.remove_peer(pid);
            }
        }
        for pid in self.throttler.flush_ul() {
            let res = {
                let torrent = self.torrents.get_mut(&self.peers[&pid]).unwrap();
                torrent.peer_writable(pid)
            };
            if res.is_err() {
                self.remove_peer(pid);
            }
        }
    }

    fn add_torrent(&mut self, info: torrent::Info) {
        if self.hash_idx.contains_key(&info.hash) {
            return;
        }
        let tid = self.tid_cnt;
        let r = self.reg.clone();
        let throttle = self.throttler.get_throttle();
        let t = Torrent::new(tid, info, throttle, r);
        self.hash_idx.insert(t.info.hash, tid);
        self.tid_cnt += 1;
        self.torrents.insert(tid, t);
    }

    fn handle_rpc(&mut self, req: rpc::Request) {
        match req {
            rpc::Request::ListTorrents => {
                let mut resp = Vec::new();
                for (id, _) in self.torrents.iter() {
                    resp.push(*id);
                }
                RPC.tx.send(rpc::Response::Torrents(resp)).unwrap();
            }
            rpc::Request::TorrentInfo(i) => {
                if let Some(torrent) = self.torrents.get(&i) {
                    RPC.tx.send(rpc::Response::TorrentInfo(torrent.rpc_info())).unwrap();
                } else {
                    RPC.tx.send(rpc::Response::Err("Torrent ID not found!".to_owned())).unwrap();
                }
            }
            rpc::Request::AddTorrent(data) => {
                match torrent::Info::from_bencode(data) {
                    Ok(i) => {
                        self.add_torrent(i);
                        RPC.tx.send(rpc::Response::Ack).unwrap();
                    }
                    Err(e) => {
                        RPC.tx.send(rpc::Response::Err(e.to_owned())).unwrap();
                    }
                }
            }
            rpc::Request::PauseTorrent(id) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    t.pause();
                    RPC.tx.send(rpc::Response::Ack).unwrap();
                } else {
                    RPC.tx.send(rpc::Response::Err("Torrent not found!".to_owned())).unwrap();
                }
            }
            rpc::Request::ResumeTorrent(id) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    t.resume();
                    RPC.tx.send(rpc::Response::Ack).unwrap();
                } else {
                    RPC.tx.send(rpc::Response::Err("Torrent not found!".to_owned())).unwrap();
                }
            }
            rpc::Request::RemoveTorrent(id) => {
                if let Some(_) = self.torrents.remove(&id) {
                    RPC.tx.send(rpc::Response::Ack).unwrap();
                } else {
                    RPC.tx.send(rpc::Response::Err("Torrent not found!".to_owned())).unwrap();
                }
            }
            rpc::Request::ThrottleUpload(amnt) => {
                self.throttler.set_ul_rate(amnt);
                RPC.tx.send(rpc::Response::Ack).unwrap();
            }
            rpc::Request::ThrottleDownload(amnt) => {
                self.throttler.set_dl_rate(amnt);
                RPC.tx.send(rpc::Response::Ack).unwrap();
            }
            rpc::Request::Shutdown => { unimplemented!(); }
        }
    }

    fn add_peer(&mut self, id: usize, peer: Peer) {
        let torrent = self.torrents.get_mut(&id).unwrap();
        if let Some(pid) = torrent.add_peer(peer) {
            self.peers.insert(pid, id);
        }
    }

    fn remove_peer(&mut self, id: usize) {
        println!("Removing peer {:?}", id);
        let tid = self.peers.remove(&id).expect("Removed pid should be in peer map!");
        let torrent = self.torrents.get_mut(&tid).expect("Torrent should be present in map");
        torrent.remove_peer(id);
    }
}

pub fn start() -> Handle {
    let poll = amy::Poller::new().unwrap();
    let mut reg = poll.get_registrar().unwrap();
    let (trk_tx, trk_rx) = reg.channel().unwrap();
    let (disk_tx, disk_rx) = reg.channel().unwrap();
    let (ctrl_tx, ctrl_rx) = reg.channel().unwrap();
    thread::spawn(move || {
        {
            Control::new(poll, trk_rx, disk_rx, ctrl_rx).run();
            use std::sync::atomic;
            TC.fetch_sub(1, atomic::Ordering::SeqCst);
        }
        DISK.tx.send(disk::Request::shutdown()).unwrap();
        RPC.rtx.send(rpc::Request::Shutdown).unwrap();
        TRACKER.tx.send(tracker::Request::Shutdown).unwrap();
    });
    Handle { trk_tx: Mutex::new(trk_tx), disk_tx: Mutex::new(disk_tx), ctrl_tx: Mutex::new(ctrl_tx) }
}
