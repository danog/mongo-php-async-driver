//! Live-server tests of the connection layer. They run when MONGO_CORE_TEST_SERVER=host:port
//! names a standalone mongod (tools/topo.sh start standalone), and pass trivially otherwise.

use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::handshake::{self, HelloOpts, Metadata};
use mongo_core::net::{Interest, Reactor};
use mongo_core::sdam::ServerType;
use mongo_core::wire::{self, MsgBuilder};
use std::time::Duration;

fn server() -> Option<(String, u16)> {
    let s = std::env::var("MONGO_CORE_TEST_SERVER").ok()?;
    let (h, p) = s.rsplit_once(':')?;
    Some((h.to_owned(), p.parse().ok()?))
}

fn get<'a>(doc: &'a [u8], key: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    if it.find(key) { Some(it.value()) } else { None }
}

fn md() -> Metadata {
    Metadata { appname: Some("core-live".into()), driver_name: "ext-mongodb:PHP".into(), driver_version: "2.5.6".into(), platform: "PHP test".into() }
}

#[test]
fn handshake_and_crud() {
    let Some((h, p)) = server() else { return };
    let (mut c, sd) = handshake::connect_and_hello(&h, p, Some(Duration::from_secs(5)), &md(), &HelloOpts::default()).unwrap();
    assert_eq!(sd.ty, ServerType::Standalone, "{sd:?}");
    assert!(sd.max_wire_version >= 8);
    assert!(sd.hello_ok);
    let t = Some(Duration::from_secs(5));

    let mut w = Writer::new();
    w.int32(b"ping", 1);
    w.utf8(b"$db", b"admin");
    let r = handshake::run_command(&mut c, &w.finish(), t).unwrap();
    mongo_core::cmd::check_ok(&r).unwrap();

    let mut w = Writer::new();
    w.utf8(b"drop", b"live");
    w.utf8(b"$db", b"core_test");
    let _ = handshake::run_command(&mut c, &w.finish(), t).unwrap();

    // insert with a document sequence
    let docs: Vec<Vec<u8>> = (0..3).map(|i| {
        let mut d = Writer::new();
        d.int32(b"_id", i);
        d.finish()
    }).collect();
    let mut w = Writer::new();
    w.utf8(b"insert", b"live");
    w.utf8(b"$db", b"core_test");
    let mut m = MsgBuilder::new(handshake::next_request_id(), 0);
    m.body(&w.finish());
    m.sequence("documents", docs.iter().map(|d| &d[..]));
    let reply = c.roundtrip(&m.finish(), t).unwrap();
    let r = wire::parse_reply(reply).unwrap();
    mongo_core::cmd::check_ok(r.body).unwrap();
    assert!(matches!(get(r.body, b"n"), Some(Value::Int32(3))));

    // a command error
    let mut w = Writer::new();
    w.int32(b"noSuchCommand", 1);
    w.utf8(b"$db", b"admin");
    let r = handshake::run_command(&mut c, &w.finish(), t).unwrap();
    let e = mongo_core::cmd::check_ok(&r).unwrap_err();
    assert_eq!(e.code, 59);
    assert!(e.server);
}

#[cfg(unix)]
#[test]
fn nonblocking_with_mio() {
    let Some((h, p)) = server() else { return };
    let (mut c, _) = handshake::connect_and_hello(&h, p, Some(Duration::from_secs(5)), &md(), &HelloOpts::default()).unwrap();
    let mut r = mongo_core::net::MioReactor::new().unwrap();
    c.set_nonblocking().unwrap();
    let mut w = Writer::new();
    w.int32(b"ping", 1);
    w.utf8(b"$db", b"admin");
    let mut m = MsgBuilder::new(handshake::next_request_id(), 0);
    m.body(&w.finish());
    let msg = m.finish();
    let mut off = 0;
    let mut ready = Vec::new();
    while !c.try_send(&msg, &mut off).unwrap() {
        r.arm(&mut c, 1, Interest::Write).unwrap();
        r.poll(Some(Duration::from_secs(5)), &mut ready).unwrap();
    }
    c.begin_recv();
    loop {
        if c.try_recv().unwrap() {
            break;
        }
        r.arm(&mut c, 7, Interest::Read).unwrap();
        ready.clear();
        r.poll(Some(Duration::from_secs(5)), &mut ready).unwrap();
        assert_eq!(ready, vec![7]);
    }
    let rep = wire::parse_reply(c.message()).unwrap();
    mongo_core::cmd::check_ok(rep.body).unwrap();
    r.disarm(&mut c).unwrap();
}
