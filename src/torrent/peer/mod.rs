mod reader;
mod writer;
mod message;

use std::net::SocketAddr;
use std::{io, fmt, mem, time};
use std::net::TcpStream;

use chrono::{DateTime, Utc};

pub use self::message::Message;
use self::reader::Reader;
use self::writer::Writer;
use socket::Socket;
use torrent::{Torrent, Bitfield};
use throttle::Throttle;
use control::cio;
use rpc::{self, resource};
use tracker;
use util;
use {DHT_EXT, CONFIG};

error_chain! {
    errors {
        ProtocolError(r: &'static str) {
            description("Peer did not conform to the bittorrent protocol")
                display("Peer protocol error: {:?}", r)
        }
    }
}

/// Peer connection and associated metadata.
pub struct Peer<T: cio::CIO> {
    id: usize,
    cio: T,
    pieces: Bitfield,
    remote_status: Status,
    local_status: Status,
    queued: u16,
    tid: usize,
    downloaded: u32,
    uploaded: u32,
    downloaded_bytes: u64,
    uploaded_bytes: u64,
    last_flush: DateTime<Utc>,
    addr: SocketAddr,
    t_hash: [u8; 20],
    cid: Option<[u8; 20]>,
    rsv: Option<[u8; 8]>,
}

#[derive(Debug)]
pub struct Status {
    pub choked: bool,
    pub interested: bool,
}

pub struct PeerConn {
    last_action: time::Instant,
    sock: Socket,
    reader: Reader,
    writer: Writer,
}

impl PeerConn {
    pub fn new(sock: Socket) -> PeerConn {
        let writer = Writer::new();
        let reader = Reader::new();
        PeerConn {
            sock,
            writer,
            reader,
            last_action: time::Instant::now(),
        }
    }

    #[cfg(test)]
    pub fn test() -> PeerConn {
        let writer = Writer::new();
        let reader = Reader::new();
        PeerConn {
            last_action: time::Instant::now(),
            sock: Socket::empty(),
            writer,
            reader,
        }
    }

    pub fn sock(&self) -> &Socket {
        &self.sock
    }

    pub fn sock_mut(&mut self) -> &mut Socket {
        &mut self.sock
    }

    pub fn last_action(&self) -> &time::Instant {
        &self.last_action
    }

    /// Creates a new "outgoing" peer, which acts as a client.
    /// Once created, set_torrent should be called.
    pub fn new_outgoing(ip: &SocketAddr) -> io::Result<PeerConn> {
        Ok(PeerConn::new(Socket::new(ip)?))
    }

    /// Creates a peer where we are acting as the server.
    /// Once the handshake is received, set_torrent should be called.
    pub fn new_incoming(sock: TcpStream) -> io::Result<PeerConn> {
        Ok(PeerConn::new(Socket::from_stream(sock)?))
    }

    pub fn writable(&mut self) -> io::Result<()> {
        self.last_action = time::Instant::now();
        self.writer.writable(&mut self.sock)
    }

    pub fn readable(&mut self) -> io::Result<Option<Message>> {
        self.last_action = time::Instant::now();
        self.reader.readable(&mut self.sock)
    }

    pub fn write_message(&mut self, msg: Message) -> io::Result<()> {
        self.writer.write_message(msg, &mut self.sock)
    }

    pub fn set_throttle(&mut self, throt: Throttle) {
        self.sock.throttle = Some(throt);
    }
}

impl Status {
    fn new() -> Status {
        Status {
            choked: true,
            interested: false,
        }
    }
}

#[cfg(test)]
impl Peer<cio::test::TCIO> {
    pub fn test(
        id: usize,
        uploaded: u32,
        downloaded: u32,
        queued: u16,
        pieces: Bitfield,
    ) -> Peer<cio::test::TCIO> {
        Peer {
            id,
            remote_status: Status::new(),
            local_status: Status::new(),
            uploaded,
            downloaded,
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            addr: "127.0.0.1:0".parse().unwrap(),
            cio: cio::test::TCIO::new(),
            queued,
            pieces,
            tid: 0,
            t_hash: [0u8; 20],
            rsv: None,
            cid: None,
            last_flush: Utc::now(),
        }
    }

    pub fn test_from_pieces(id: usize, pieces: Bitfield) -> Peer<cio::test::TCIO> {
        Peer::test(id, 0, 0, 0, pieces)
    }

    pub fn test_from_stats(id: usize, ul: u32, dl: u32) -> Peer<cio::test::TCIO> {
        Peer::test(id, ul, dl, 0, Bitfield::new(4))
    }

    pub fn test_with_tcio(mut cio: cio::test::TCIO) -> Peer<cio::test::TCIO> {
        use control::cio::CIO;

        let conn = PeerConn::test();
        let id = cio.add_peer(conn).unwrap();
        let mut peer = Peer::test(id, 0, 0, 0, Bitfield::new(4));
        peer.cio = cio;
        peer
    }
}

impl<T: cio::CIO> Peer<T> {
    pub fn new(
        mut conn: PeerConn,
        t: &mut Torrent<T>,
        cid: Option<[u8; 20]>,
        rsv: Option<[u8; 8]>,
    ) -> cio::Result<Peer<T>> {
        let addr = conn.sock().addr();
        conn.set_throttle(t.get_throttle(0));
        let id = t.cio.add_peer(conn)?;
        let mut p = Peer {
            id,
            addr,
            remote_status: Status::new(),
            local_status: Status::new(),
            uploaded: 0,
            downloaded: 0,
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            cio: t.cio.new_handle(),
            queued: 0,
            pieces: Bitfield::new(t.info.hashes.len() as u64),
            tid: t.id,
            t_hash: t.info.hash,
            rsv,
            cid,
            last_flush: Utc::now(),
        };
        p.send_message(Message::handshake(&t.info));
        p.send_message(Message::Bitfield(t.pieces.clone()));
        p.send_rpc_info();
        Ok(p)
    }

    /// Returns whether or not the peer has received a handshake
    pub fn ready(&self) -> bool {
        self.cid.is_some()
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn pieces(&self) -> &Bitfield {
        &self.pieces
    }

    #[cfg(test)]
    pub fn pieces_mut(&mut self) -> &mut Bitfield {
        &mut self.pieces
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn flush(&mut self) -> (u32, u32) {
        (
            mem::replace(&mut self.uploaded, 0),
            mem::replace(&mut self.downloaded, 0),
        )
    }

    pub fn remote_status(&self) -> &Status {
        &self.remote_status
    }

    pub fn get_tx_rates(&mut self) -> (u64, u64) {
        let dur = Utc::now()
            .signed_duration_since(self.last_flush)
            .num_milliseconds() as u64;
        let ub = mem::replace(&mut self.uploaded_bytes, 0);
        let db = mem::replace(&mut self.downloaded_bytes, 0);
        let ul = (1000 * ub) / dur;
        let dl = (1000 * db) / dur;
        self.last_flush = Utc::now();
        (ul, dl)
    }

    pub fn can_queue_req(&mut self) -> bool {
        !self.remote_status.choked && self.queued < 5
    }

    pub fn handle_msg(&mut self, msg: &mut Message) -> Result<()> {
        match *msg {
            Message::Handshake { rsv, id, .. } => {
                if (rsv[DHT_EXT.0] & DHT_EXT.1) != 0 {
                    self.send_message(Message::Port(CONFIG.dht.port));
                }
                self.rsv = Some(rsv);
                self.cid = Some(id);
                self.send_rpc_info();
            }
            Message::Piece { length, .. } |
            Message::SharedPiece { length, .. } => {
                self.downloaded_bytes += length as u64;
                self.downloaded += 1;
                self.queued -= 1;
            }
            Message::Request { .. } => {
                if self.local_status.choked {
                    return Err(
                        ErrorKind::ProtocolError("Peer requested while choked!").into(),
                    );
                }
            }
            Message::Choke => {
                self.remote_status.choked = true;
            }
            Message::Unchoke => {
                self.remote_status.choked = false;
            }
            Message::Interested => {
                self.remote_status.interested = true;
            }
            Message::Uninterested => {
                self.remote_status.interested = false;
            }
            Message::Have(idx) => {
                if idx >= self.pieces.len() as u32 {
                    return Err(
                        ErrorKind::ProtocolError("Invalid piece provided in HAVE!").into(),
                    );
                }
                self.pieces.set_bit(idx as u64);
            }
            Message::Bitfield(ref mut pieces) => {
                // Set the correct length, then swap the pieces
                pieces.cap(self.pieces.len());
                mem::swap(pieces, &mut self.pieces);
            }
            Message::KeepAlive => {
                self.send_message(Message::KeepAlive);
            }
            Message::Cancel { index, begin, .. } => {
                self.cio.get_peer(self.id, |conn| {
                    conn.writer.write_queue.retain(|m| {
                        if let Message::Piece { index: i, begin: b, .. } = *m {
                            return !(i == index && b == begin);
                        }
                        return true;
                    });
                });
            }
            Message::Port(p) => {
                let mut s = self.addr();
                s.set_port(p);
                self.cio.msg_trk(tracker::Request::AddNode(s));
            }
        }
        Ok(())
    }

    pub fn request_piece(&mut self, idx: u32, offset: u32, len: u32) {
        let m = Message::request(idx, offset, len);
        self.queued += 1;
        self.send_message(m);
    }

    pub fn choke(&mut self) {
        if !self.local_status.choked {
            self.local_status.choked = true;
            self.send_message(Message::Choke);
        }
    }

    pub fn unchoke(&mut self) {
        if self.local_status.choked {
            self.local_status.choked = false;
            self.send_message(Message::Unchoke);
        }
    }

    pub fn interested(&mut self) {
        if !self.local_status.interested {
            self.local_status.interested = true;
            self.send_message(Message::Interested);
        }
    }

    pub fn uninterested(&mut self) {
        if self.local_status.interested {
            self.local_status.interested = false;
            self.send_message(Message::Uninterested);
        }
    }

    pub fn send_message(&mut self, msg: Message) {
        match &msg {
            &Message::SharedPiece { length, .. } |
            &Message::Piece { length, .. } => {
                self.uploaded += 1;
                self.uploaded_bytes += length as u64;
            }
            _ => {}
        }
        self.cio.msg_peer(self.id, msg);
    }

    fn send_rpc_info(&mut self) {
        if let Some(cid) = self.cid {
            let id = util::peer_rpc_id(&self.t_hash, self.id as u64);
            self.cio.msg_rpc(rpc::CtlMessage::Extant(vec![
                resource::Resource::Peer(resource::Peer {
                    id,
                    torrent_id: util::hash_to_id(&self.t_hash[..]),
                    client_id: cid,
                    ip: self.addr.to_string(),
                    rate_up: 0,
                    rate_down: 0,
                    availability: 0.,
                }),
            ]));
        }
    }

    pub fn send_rpc_removal(&mut self) {
        if self.ready() {
            self.cio.msg_rpc(rpc::CtlMessage::Removed(
                vec![util::peer_rpc_id(&self.t_hash, self.id as u64)],
            ));
        }
    }
}

impl<T: cio::CIO> Drop for Peer<T> {
    fn drop(&mut self) {
        self.send_rpc_removal();
    }
}

impl<T: cio::CIO> fmt::Debug for Peer<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Peer {{ id: {}, tid: {}, local_status: {:?}, remote_status: {:?} }}",
            self.id,
            self.tid,
            self.local_status,
            self.remote_status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Peer;
    use control::cio::{CIO, test};
    use torrent::Message;

    #[test]
    fn test_cancel() {
        let mut tcio = test::TCIO::new();
        let mut peer = Peer::test_with_tcio(tcio.new_handle());
        let p1 = Message::Piece {
            index: 0,
            begin: 0,
            data: Box::new([0u8; 16384]),
            length: 16384,
        };
        let p2 = Message::Piece {
            index: 1,
            begin: 1,
            data: Box::new([0u8; 16384]),
            length: 16384,
        };
        let p3 = Message::Piece {
            index: 2,
            begin: 2,
            data: Box::new([0u8; 16384]),
            length: 16384,
        };
        peer.send_message(Message::KeepAlive);
        peer.send_message(p1.clone());
        peer.send_message(p2.clone());
        peer.send_message(p3.clone());

        let mut c = Message::Cancel {
            index: 1,
            begin: 1,
            length: 16384,
        };
        peer.handle_msg(&mut c).unwrap();
        let wq = tcio.get_peer(peer.id, |p| p.writer.write_queue.clone())
            .unwrap();
        assert_eq!(wq.len(), 2);
        assert_eq!(wq[0], p1);
        assert_eq!(wq[1], p3);
    }
}
