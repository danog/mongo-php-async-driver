//! The initial-dns-seedlist-discovery specification tests (tests/spec/source/
//! initial-dns-seedlist-discovery) against the SRV/TXT resolution and the URI port, as
//! libmongoc's runner (test-mongoc-dns.c) checks them before connecting: the URI error, the
//! seed list of the new topology, `numSeeds`, the options and the parsed options. `hosts`,
//! `numHosts` and `ping` need the specification's replica set on localhost:27017-27019 and are
//! not checked here.
//!
//! The records live in the public zone test.build.10gen.cc, so the test needs DNS: it runs only
//! with MONGO_CORE_DNS_TESTS=1.

use mongo_core::topology::TopologyDescription;
use mongo_core::uri::Uri;
use serde_json::Value;

/// libmongoc behaviour the specification does not expect; each checked against the reference.
const DIVERGENCES: &[&str] = &[
    // libmongoc 2.5.8 does not know srvAllowedHostsSuffix (an unknown option only warns): the
    // parent-domain rule applies unchanged
    "replica-set/srvAllowedHostsSuffix-case-insensitive.json",
    "replica-set/srvAllowedHostsSuffix-period-only.json",
    "replica-set/srvAllowedHostsSuffix-psl-public-suffix-capitalized.json",
    "replica-set/srvAllowedHostsSuffix-psl-public-suffix.json",
    "replica-set/srvAllowedHostsSuffix-tld-only.json",
    "replica-set/srvAllowedHostsSuffix-trailing-dot.json",
    "replica-set/srvAllowedHostsSuffix-with_dot.json",
    "replica-set/srvAllowedHostsSuffix-without_dot_pass.json",
    // the returned (lowercased) host is compared with the SRV host name as written, case-sensitively
    "replica-set/uri-with-uppercase-hostname.json",
];

fn run(path: &std::path::Path) -> Result<(), String> {
    let t: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let want_error = t["error"].as_bool().unwrap_or(false);
    let result = (|| {
        let mut uri = Uri::parse(t["uri"].as_str().unwrap())?;
        mongo_core::srv::initial(&mut uri).map_err(|e| e.message)?;
        uri.finalize()?;
        Ok::<Uri, String>(uri)
    })();
    let uri = match (result, want_error) {
        (Err(_), true) => return Ok(()),
        (Ok(_), true) => return Err("expected an error".into()),
        (Err(e), false) => return Err(format!("unexpected error: {e}")),
        (Ok(u), false) => u,
    };
    let td = TopologyDescription::from_uri(&uri, true);
    let mut seeds: Vec<String> = td.servers.iter().map(|s| s.addr.to_ascii_lowercase()).collect();
    seeds.sort();
    if let Some(want) = t["seeds"].as_array() {
        let mut want: Vec<String> = want.iter().map(|s| s.as_str().unwrap().to_ascii_lowercase()).collect();
        want.sort();
        if want != seeds {
            return Err(format!("seeds {seeds:?}, expected {want:?}"));
        }
    }
    if let Some(n) = t["numSeeds"].as_u64() {
        if seeds.len() as u64 != n {
            return Err(format!("{} seeds, expected {n}", seeds.len()));
        }
    }
    if let Some(o) = t["options"].as_object() {
        for (k, v) in o {
            let got: Value = match k.as_str() {
                "ssl" | "tls" => uri.tls().into(),
                "replicaSet" => uri.options.str("replicaset").into(),
                "authSource" => uri.auth_source_option().into(),
                "srvServiceName" => uri.options.str("srvservicename").into(),
                "loadBalanced" => uri.get_bool("loadbalanced", false).into(),
                "directConnection" => uri.get_bool("directconnection", false).into(),
                "srvMaxHosts" => uri.get_int32("srvmaxhosts", 0).into(),
                k => return Err(format!("unknown option {k}")),
            };
            if &got != v {
                return Err(format!("option {k}: {got}, expected {v}"));
            }
        }
    }
    if let Some(o) = t["parsed_options"].as_object() {
        for (k, v) in o {
            let got: Value = match k.as_str() {
                "user" => uri.username.as_deref().into(),
                "password" => uri.password.as_deref().into(),
                "db" | "defaultDatabase" => uri.database.as_deref().into(),
                "auth_database" => uri.auth_source().into(),
                k => return Err(format!("unknown parsed option {k}")),
            };
            if &got != v {
                return Err(format!("parsed option {k}: {got}, expected {v}"));
            }
        }
    }
    Ok(())
}

#[test]
fn initial_dns_seedlist() {
    if std::env::var_os("MONGO_CORE_DNS_TESTS").is_none() {
        eprintln!("skipped: set MONGO_CORE_DNS_TESTS=1 (needs DNS for test.build.10gen.cc)");
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/spec/source/initial-dns-seedlist-discovery/tests");
    let mut files: Vec<_> = ["replica-set", "sharded", "load-balanced"].iter().flat_map(|d| std::fs::read_dir(root.join(d)).unwrap().map(|e| e.unwrap().path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    let mut failures = Vec::new();
    let mut ran = 0;
    for f in &files {
        let name = format!("{}/{}", f.parent().unwrap().file_name().unwrap().to_string_lossy(), f.file_name().unwrap().to_string_lossy());
        ran += 1;
        match run(f) {
            Ok(()) if DIVERGENCES.contains(&name.as_str()) => failures.push(format!("{name}: listed as a divergence but passes")),
            Ok(()) => {}
            Err(_) if DIVERGENCES.contains(&name.as_str()) => {}
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    eprintln!("{ran} files");
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}
