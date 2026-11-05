//! A hostile replica set whose members keep inventing new members: one scan must stay bounded
//! (Client::scan_once, MAX_DISCOVERED_PER_SCAN; see DIVERGENCES.md). No server needed: the
//! members are local fake servers.

use mongo_core::bson::writer::Writer;
use mongo_core::client::{Client, Settings, MAX_DISCOVERED_PER_SCAN};
use mongo_core::uri::Uri;
use mongo_core::wire::{MsgBuilder, HEADER_LEN};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Every hello answered by any fake member.
static HELLOS: AtomicUsize = AtomicUsize::new(0);
/// Fake members started (bounded so that a broken bound fails instead of exhausting the host).
static MEMBERS: AtomicUsize = AtomicUsize::new(0);
const MEMBER_LIMIT: usize = 1000;

/// Starts a member: a secondary of "rs" (no primary, so its hosts are trusted) that lists itself
/// and a member it starts on demand, a new one for every hello.
fn member() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let me = l.local_addr().unwrap().to_string();
    MEMBERS.fetch_add(1, Ordering::SeqCst);
    let addr = Arc::new(me.clone());
    thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { return };
            let addr = addr.clone();
            thread::spawn(move || serve(s, &addr));
        }
    });
    me
}

fn serve(mut s: TcpStream, me: &str) {
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
        HELLOS.fetch_add(1, Ordering::SeqCst);
        let next = if MEMBERS.load(Ordering::SeqCst) < MEMBER_LIMIT { member() } else { "127.0.0.1:1".to_owned() };
        let mut w = Writer::new();
        w.double(b"ok", 1.0);
        w.bool(b"helloOk", true);
        w.bool(b"isWritablePrimary", false);
        w.bool(b"secondary", true);
        w.utf8(b"setName", b"rs");
        let a = w.begin(4, b"hosts");
        w.utf8(b"0", me.as_bytes());
        w.utf8(b"1", next.as_bytes());
        w.end(a);
        w.utf8(b"me", me.as_bytes());
        w.int32(b"minWireVersion", 0);
        w.int32(b"maxWireVersion", 21);
        w.int32(b"maxBsonObjectSize", 16 * 1024 * 1024);
        w.int32(b"maxMessageSizeBytes", 48_000_000);
        w.int32(b"maxWriteBatchSize", 100_000);
        let mut m = MsgBuilder::new(1, 0);
        m.body(&w.finish());
        let mut reply = m.finish();
        reply[8..12].copy_from_slice(&request_id.to_le_bytes());
        if s.write_all(&reply).is_err() {
            return;
        }
    }
}

#[test]
fn scan_with_endless_discovery_is_bounded() {
    let seed = member();
    let uri = Uri::parse(&format!("mongodb://{seed}/?replicaSet=rs&connectTimeoutMS=2000")).unwrap();
    let s = Settings::from_uri(&uri);
    let mut c = Client::new(uri, s).unwrap();
    c.scan_once(false);
    // the seed plus at most MAX_DISCOVERED_PER_SCAN discovered members, each checked once
    let n = HELLOS.load(Ordering::SeqCst);
    assert_eq!(n, 1 + MAX_DISCOVERED_PER_SCAN, "hellos in one scan");
    // the next scan goes on from there, again bounded
    c.scan_once(false);
    let n2 = HELLOS.load(Ordering::SeqCst) - n;
    assert!(n2 <= c.td.servers.len(), "second scan: {n2} hellos for {} servers", c.td.servers.len());
}
