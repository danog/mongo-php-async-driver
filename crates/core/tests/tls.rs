//! TLS without a server: a client asked for TLS never sends anything in cleartext, and a
//! failed handshake fails the server check.

use mongo_core::client::{Client, Settings};
use mongo_core::topology::OpType;
use mongo_core::uri::Uri;
use std::io::Read;
use std::net::TcpListener;

/// A fake server that records the first bytes of every connection, then closes it.
fn recorder() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming().take(4) {
            let Ok(mut s) = s else { return };
            s.set_read_timeout(Some(std::time::Duration::from_millis(500))).unwrap();
            let mut b = vec![0u8; 64];
            let n = s.read(&mut b).unwrap_or(0);
            b.truncate(n);
            let _ = tx.send(b);
        }
    });
    (port, rx)
}

#[test]
fn tls_is_never_cleartext() {
    for q in ["tls=true", "ssl=true", "tlsCAFile=/nonexistent/ca.pem", "tlsInsecure=true", "tlsAllowInvalidCertificates=true"] {
        let (port, rx) = recorder();
        let uri = Uri::parse(&format!("mongodb://127.0.0.1:{port}/?{q}&serverSelectionTimeoutMS=500&connectTimeoutMS=500")).unwrap();
        let mut c = Client::new(uri.clone(), Settings::from_uri(&uri)).unwrap();
        let e = c.select_server_id(OpType::Read, None, &[]).unwrap_err();
        assert!(e.message.contains("TLS handshake failed"), "{q}: {}", e.message);
        // a missing CA file fails before connecting; otherwise the first byte is a TLS
        // handshake record (0x16), never an OP_MSG header
        if let Ok(first) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
            assert!(first.is_empty() || first[0] == 0x16, "{q}: cleartext sent: {first:?}");
        }
    }
}

#[test]
fn tls_off_is_plain() {
    let (port, rx) = recorder();
    let uri = Uri::parse(&format!("mongodb://127.0.0.1:{port}/?tls=false&serverSelectionTimeoutMS=500&connectTimeoutMS=500")).unwrap();
    let mut c = Client::new(uri.clone(), Settings::from_uri(&uri)).unwrap();
    let _ = c.select_server_id(OpType::Read, None, &[]);
    let first = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    // positive control: the plain hello's OP_MSG opcode (2013) at offset 12
    assert_eq!(&first[12..16], &2013i32.to_le_bytes());
}
