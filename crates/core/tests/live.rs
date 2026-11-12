//! Live-server tests of the connection layer. They run when MONGO_CORE_TEST_SERVER=host:port
//! names a standalone mongod (tools/topo.sh start standalone), and pass trivially otherwise.

use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::handshake::{self, HelloOpts, Metadata};
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

#[test]
fn nonblocking_with_poll() {
    use mongo_core::{net, os};
    let Some((h, p)) = server() else { return };
    let (mut c, _) = handshake::connect_and_hello(&h, p, Some(Duration::from_secs(5)), &md(), &HelloOpts::default()).unwrap();
    let wait = |c: &net::Conn, events: i16| {
        let mut fds = [os::pollfd(c.raw_fd(), events)];
        assert_eq!(net::poll_fds(&mut fds, Some(std::time::Instant::now() + Duration::from_secs(5))).unwrap(), 1);
    };
    c.set_nonblocking().unwrap();
    let mut w = Writer::new();
    w.int32(b"ping", 1);
    w.utf8(b"$db", b"admin");
    let mut m = MsgBuilder::new(handshake::next_request_id(), 0);
    m.body(&w.finish());
    let msg = m.finish();
    let mut off = 0;
    while !c.try_send(&msg, &mut off).unwrap() {
        wait(&c, os::POLLOUT);
    }
    c.begin_recv();
    while !c.try_recv().unwrap() {
        wait(&c, os::POLLIN);
    }
    let rep = wire::parse_reply(c.message()).unwrap();
    mongo_core::cmd::check_ok(rep.body).unwrap();
}

#[test]
fn client_select_and_command() {
    use mongo_core::client::{Client, Settings};
    use mongo_core::topology::OpType;
    use mongo_core::uri::Uri;
    // an unreachable server: serverSelectionTryOnce error with the scanner's error
    let uri = Uri::parse("mongodb://127.0.0.1:28099/?connectTimeoutMS=200").unwrap();
    let s = Settings::from_uri(&uri);
    let mut c = Client::new(uri, s).unwrap();
    let e = c.select_server_id(OpType::Read, None, &[]).unwrap_err();
    assert_eq!(e.message, "No suitable servers found (`serverSelectionTryOnce` set): [connection refused calling hello on '127.0.0.1:28099']. Topology type: Single");
    // in cooldown right after: no rescan
    let e = c.select_server_id(OpType::Read, None, &[]).unwrap_err();
    assert!(e.message.starts_with("No servers yet eligible for rescan"), "{}", e.message);
    let Some((h, p)) = server() else { return };
    let uri = Uri::parse(&format!("mongodb://{h}:{p}/")).unwrap();
    let s = Settings::from_uri(&uri);
    let mut c = Client::new(uri, s).unwrap();
    let (id, _) = c.select_server_id(OpType::Write, None, &[]).unwrap();
    assert_eq!(c.td().server(id).unwrap().ty, ServerType::Standalone);
    let mut w = Writer::with_capacity(32);
    w.int32(b"ping", 1);
    w.utf8(b"$db", b"admin");
    let r = c.run_command(id, "ping", "admin", &w.finish()).unwrap();
    assert!(matches!(get(&r, b"ok"), Some(Value::Double(f)) if f == 1.0));
}

/// No fork detection (as libmongoc): after fork() the child uses the connection it inherited.
/// Using it from both processes at once is the application's problem; here the parent waits
/// for the child, then uses the same connection again.
#[cfg(unix)]
#[test]
fn client_fork() {
    use mongo_core::client::{Client, Settings};
    use mongo_core::topology::OpType;
    use mongo_core::uri::Uri;
    let Some((h, p)) = server() else { return };
    let uri = Uri::parse(&format!("mongodb://{h}:{p}/")).unwrap();
    let s = Settings::from_uri(&uri);
    let mut c = Client::new(uri, s).unwrap();
    let ping = || {
        let mut w = Writer::with_capacity(32);
        w.int32(b"ping", 1);
        w.utf8(b"$db", b"admin");
        w.finish()
    };
    let (id, _) = c.select_server_id(OpType::Read, None, &[]).unwrap();
    c.run_command(id, "ping", "admin", &ping()).unwrap();
    let parent_fd = c.conn_fd(id).unwrap();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // child: the inherited connection is used
        let ok = (|| {
            for _ in 0..20 {
                c.run_command(id, "ping", "admin", &ping()).ok()?;
            }
            (c.conn_fd(id)? == parent_fd).then_some(())
        })();
        unsafe { libc::_exit(if ok.is_some() { 0 } else { 1 }) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "child failed: {status}");
    for _ in 0..20 {
        c.run_command(id, "ping", "admin", &ping()).unwrap();
    }
    assert_eq!(c.conn_fd(id), Some(parent_fd));
}
