//! Parallel scans (Client::scan_once over crate::scan, = libmongoc's single-threaded scanner):
//! every server is checked at once, libmongoc's check errors, socketCheckIntervalMS. No server
//! needed: the members are local fake servers.

use mongo_core::apm::Event;
use mongo_core::bson::writer::Writer;
use mongo_core::client::{Client, Settings};
use mongo_core::uri::Uri;
use mongo_core::wire::{MsgBuilder, HEADER_LEN};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// answers every command: hello as a secondary of "rs" listing `hosts`, others ok: 1
    Member,
    /// accepts connections, never answers
    Silent,
    /// answers the first command of a connection, then closes it
    CloseAfterOne,
    /// answers the first command and closes the connection at once
    ReplyAndClose,
}

struct Fake {
    addr: String,
    hosts: Arc<Mutex<Vec<String>>>,
    commands: Arc<AtomicUsize>,
}

fn fake(mode: Mode) -> Fake {
    fake_on("127.0.0.1:0", mode)
}

fn fake_on(bind: &str, mode: Mode) -> Fake {
    let l = TcpListener::bind(bind).unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let hosts = Arc::new(Mutex::new(vec![addr.clone()]));
    let commands = Arc::new(AtomicUsize::new(0));
    let (h, c, me) = (hosts.clone(), commands.clone(), addr.clone());
    thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { return };
            let (h, c, me) = (h.clone(), c.clone(), me.clone());
            thread::spawn(move || serve(s, mode, &me, &h, &c));
        }
    });
    Fake { addr, hosts, commands }
}

fn serve(mut s: TcpStream, mode: Mode, me: &str, hosts: &Mutex<Vec<String>>, commands: &AtomicUsize) {
    loop {
        let mut h = [0u8; HEADER_LEN];
        if s.read_exact(&mut h).is_err() {
            return;
        }
        let len = i32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        let request_id = i32::from_le_bytes(h[4..8].try_into().unwrap());
        if !(HEADER_LEN..=1 << 20).contains(&len) {
            return;
        }
        let mut rest = vec![0u8; len - HEADER_LEN];
        if s.read_exact(&mut rest).is_err() {
            return;
        }
        if mode == Mode::Silent {
            thread::sleep(Duration::from_secs(30));
            return;
        }
        commands.fetch_add(1, Ordering::SeqCst);
        // the body's first key (after flagBits, kind 0, the document's length)
        let name: Vec<u8> = rest[10..].iter().take_while(|b| **b != 0).copied().collect();
        let mut w = Writer::new();
        w.double(b"ok", 1.0);
        if name == b"isMaster" || name == b"hello" {
            w.bool(b"helloOk", true);
            w.bool(b"isWritablePrimary", false);
            w.bool(b"secondary", true);
            w.utf8(b"setName", b"rs");
            let a = w.begin(4, b"hosts");
            for (i, x) in hosts.lock().unwrap().iter().enumerate() {
                w.utf8(i.to_string().as_bytes(), x.as_bytes());
            }
            w.end(a);
            w.utf8(b"me", me.as_bytes());
            w.int32(b"minWireVersion", 0);
            w.int32(b"maxWireVersion", 21);
            w.int32(b"maxBsonObjectSize", 16 * 1024 * 1024);
            w.int32(b"maxMessageSizeBytes", 48_000_000);
            w.int32(b"maxWriteBatchSize", 100_000);
        }
        let mut m = MsgBuilder::new(1, 0);
        m.body(&w.finish());
        let mut reply = m.finish();
        reply[8..12].copy_from_slice(&request_id.to_le_bytes());
        if s.write_all(&reply).is_err() {
            return;
        }
        if mode == Mode::ReplyAndClose {
            return;
        }
        if mode == Mode::CloseAfterOne {
            // after the client read the reply (a close seen with the reply fails the check)
            thread::sleep(Duration::from_millis(200));
            return;
        }
    }
}

fn client(uri: &str) -> Client {
    mongo_core::apm::set_subscriber_count(1);
    let uri = Uri::parse(uri).unwrap();
    let s = Settings::from_uri(&uri);
    Client::new(uri, s).unwrap()
}

/// (event kind, address, error message) of the heartbeat events.
fn heartbeats(c: &mut Client) -> Vec<(&'static str, String, String)> {
    c.take_events()
        .into_iter()
        .filter_map(|t| match t.event {
            Event::HeartbeatStarted { addr } => Some(("started", addr, String::new())),
            Event::HeartbeatSucceeded { addr, .. } => Some(("succeeded", addr, String::new())),
            Event::HeartbeatFailed { addr, error, .. } => Some(("failed", addr, error.message)),
            _ => None,
        })
        .collect()
}

#[test]
fn servers_are_checked_at_once() {
    let (s1, s2, good) = (fake(Mode::Silent), fake(Mode::Silent), fake(Mode::Member));
    *good.hosts.lock().unwrap() = vec![s1.addr.clone(), s2.addr.clone(), good.addr.clone()];
    let mut c = client(&format!("mongodb://{},{},{}/?replicaSet=rs&connectTimeoutMS=600", s1.addr, s2.addr, good.addr));
    c.td().open();
    let t0 = Instant::now();
    c.scan_once(true);
    let took = t0.elapsed();
    // sequential checks would take two timeouts (1.2 s); at once: one (0.6 s after the send)
    assert!(took >= Duration::from_millis(590) && took < Duration::from_millis(1000), "scan took {took:?}");
    let hb = heartbeats(&mut c);
    // every check starts first; the member answers first, the silent ones time out in order
    let started: Vec<_> = hb.iter().take(3).map(|e| (e.0, e.1.clone())).collect();
    assert_eq!(started, vec![("started", s1.addr.clone()), ("started", s2.addr.clone()), ("started", good.addr.clone())]);
    assert_eq!((hb[3].0, &hb[3].1), ("succeeded", &good.addr));
    for (e, a) in hb[4..].iter().zip([&s1.addr, &s2.addr]) {
        assert_eq!((e.0, &e.1, e.2.clone()), ("failed", a, format!("socket timeout calling hello on '{a}'")));
    }
    assert_eq!(hb.len(), 6);
}

#[test]
fn refused_and_unresolvable_hosts() {
    // a port nobody listens on
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let mut c = client(&format!("mongodb://{closed},nonexistent.invalid:27017/?connectTimeoutMS=2000"));
    c.td().open();
    c.scan_once(true);
    let hb = heartbeats(&mut c);
    // the name resolution fails during the setup, before the refused connect is seen
    assert_eq!(hb[0], ("started", closed.clone(), String::new()));
    assert_eq!(hb[1], ("started", "nonexistent.invalid:27017".to_owned(), String::new()));
    assert_eq!(hb[2], ("failed", "nonexistent.invalid:27017".to_owned(), "Failed to resolve 'nonexistent.invalid'".to_owned()));
    assert_eq!(hb[3], ("failed", closed.clone(), format!("connection refused calling hello on '{closed}'")));
    assert_eq!(hb.len(), 4);
}

#[test]
fn every_address_of_a_host_is_tried() {
    // "localhost" resolves to ::1 and 127.0.0.1 on most hosts; only the second address listens:
    // the refused first one hands over to it at once (no happy-eyeballs delay)
    let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(&("localhost", 1)).unwrap().collect();
    if addrs.len() < 2 || addrs[0].is_ipv6() == addrs[1].is_ipv6() {
        eprintln!("skipped: localhost resolves to {addrs:?}");
        return;
    }
    let m = fake_on(&std::net::SocketAddr::new(addrs[1].ip(), 0).to_string(), Mode::Member);
    let port = m.addr.rsplit_once(':').unwrap().1.to_owned();
    *m.hosts.lock().unwrap() = vec![format!("localhost:{port}")];
    let mut c = client(&format!("mongodb://localhost:{port}/?connectTimeoutMS=2000"));
    c.td().open();
    let t0 = Instant::now();
    c.scan_once(true);
    assert!(t0.elapsed() < Duration::from_millis(200), "took {:?}", t0.elapsed());
    let hb = heartbeats(&mut c);
    assert_eq!(hb.iter().map(|e| e.0).collect::<Vec<_>>(), vec!["started", "succeeded"]);
}

#[test]
fn idle_connections_are_checked_before_use() {
    // socketCheckIntervalMS: a ping after the interval
    let m = fake(Mode::Member);
    let mut c = client(&format!("mongodb://{}/?socketCheckIntervalMS=300", m.addr));
    let (id, _) = c.select_server_id(mongo_core::topology::OpType::Read, None, &[]).unwrap();
    let before = m.commands.load(Ordering::SeqCst);
    assert!(c.check_interval(id), "fresh connection");
    assert_eq!(m.commands.load(Ordering::SeqCst), before, "no ping before the interval");
    thread::sleep(Duration::from_millis(350));
    assert!(c.check_interval(id), "ping answered");
    assert_eq!(m.commands.load(Ordering::SeqCst), before + 1, "one ping");
    // the peer closed an idle connection: seen without a round trip after one second
    let m = fake(Mode::CloseAfterOne);
    let mut c = client(&format!("mongodb://{}/", m.addr));
    let (id, _) = c.select_server_id(mongo_core::topology::OpType::Read, None, &[]).unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert!(!c.check_interval(id), "closed connection detected");
    assert_eq!(c.td().server(id).unwrap().ty, mongo_core::sdam::ServerType::Unknown);
    assert_eq!(c.td().server(id).unwrap().error.as_ref().unwrap().message, "connection closed");
    // no connection: false, the caller selects again (libmongoc)
    assert!(!c.check_interval(id));
}

#[test]
fn reply_with_close_is_read() {
    // the reply is read before the close is acted on (macOS polls the pair as POLLIN|POLLHUP)
    let m = fake(Mode::ReplyAndClose);
    let mut c = client(&format!("mongodb://{}/?connectTimeoutMS=2000", m.addr));
    c.td().open();
    c.scan_once(true);
    let hb = heartbeats(&mut c);
    assert_eq!(hb.iter().map(|e| e.0).collect::<Vec<_>>(), vec!["started", "succeeded"], "{hb:?}");
}
