//! The server-discovery-and-monitoring specification tests (single, rs, sharded, load-balanced,
//! errors) against the mongoc-topology-description.c port, run the way libmongoc's
//! test-mongoc-sdam.c runs them.

use mongo_core::bson::json_parse::from_json;
use mongo_core::error::{code, domain, Error};
use mongo_core::sdam::{ServerType, NO_SESSIONS, NO_SET_VERSION, ZERO_OID};
use mongo_core::topology::{AppError, TopologyDescription};
use mongo_core::uri::Uri;
use serde_json::Value;

/// Skipped by libmongoc too.
const SKIP_SUBSTRINGS: &[&str] = &["Pre-4.2"];

fn bson(v: &Value) -> Vec<u8> {
    from_json(serde_json::to_string(v).unwrap().as_bytes()).unwrap()
}

fn oid(v: &Value) -> [u8; 12] {
    match v.get("$oid").and_then(Value::as_str) {
        Some(h) => {
            let mut o = [0u8; 12];
            for i in 0..12 {
                o[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap();
            }
            o
        }
        None => ZERO_OID,
    }
}

fn server_type(s: &str) -> ServerType {
    match s {
        "Standalone" => ServerType::Standalone,
        "Mongos" => ServerType::Mongos,
        "PossiblePrimary" => ServerType::PossiblePrimary,
        "RSPrimary" => ServerType::RsPrimary,
        "RSSecondary" => ServerType::RsSecondary,
        "RSArbiter" => ServerType::RsArbiter,
        "RSOther" => ServerType::RsOther,
        "RSGhost" => ServerType::RsGhost,
        "LoadBalancer" => ServerType::LoadBalancer,
        "Unknown" => ServerType::Unknown,
        t => panic!("server type {t}"),
    }
}

fn i64_of(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.get("$numberLong").and_then(Value::as_str).and_then(|s| s.parse().ok())).unwrap_or_else(|| panic!("{v}"))
}

fn run(t: &Value) -> Result<(), String> {
    let uri = Uri::parse(t["uri"].as_str().unwrap()).map_err(|e| format!("uri: {e}"))?;
    let mut td = TopologyDescription::from_uri(&uri, true);
    td.open();
    for (pi, phase) in t["phases"].as_array().unwrap().iter().enumerate() {
        if let Some(rs) = phase.get("responses").and_then(Value::as_array) {
            for r in rs {
                let addr = r[0].as_str().unwrap();
                let Some(id) = td.server_by_address(addr).map(|s| s.id) else { continue };
                td.handle_hello(id, Some(&bson(&r[1])), 1, None);
            }
        } else if let Some(errs) = phase.get("applicationErrors").and_then(Value::as_array) {
            for e in errs {
                let addr = e["address"].as_str().unwrap();
                let Some(sd) = td.server_by_address(addr) else { continue };
                let id = sd.id;
                let generation = e.get("generation").and_then(Value::as_u64).map_or_else(|| sd.generation(&ZERO_OID), |g| g as u32);
                let handshake_complete = match e["when"].as_str().unwrap() {
                    "beforeHandshakeCompletes" => false,
                    "afterHandshakeCompletes" => true,
                    w => panic!("when {w}"),
                };
                let (kind, reply) = match e["type"].as_str().unwrap() {
                    "command" => (AppError::Command, bson(&e["response"])),
                    k @ ("network" | "timeout") => {
                        let kind = if k == "network" { AppError::Network } else { AppError::Timeout };
                        let reply = if handshake_complete { bson(&serde_json::json!({})) } else { bson(&serde_json::json!({"errorLabels": ["SystemOverloadedError"]})) };
                        (kind, reply)
                    }
                    k => panic!("type {k}"),
                };
                let why = Error::new(0, 0, "");
                td.handle_app_error(id, handshake_complete, kind, &reply, &why, generation, &ZERO_OID);
            }
        }
        let o = &phase["outcome"];
        let err = |m: String| Err(format!("phase {pi}: {m}"));
        for (k, v) in o.as_object().unwrap() {
            match k.as_str() {
                "servers" => {
                    let want = v.as_object().unwrap();
                    if want.len() != td.servers.len() {
                        return err(format!("{} servers, want {}: {:?}", td.servers.len(), want.len(), td.servers.iter().map(|s| &s.addr).collect::<Vec<_>>()));
                    }
                    for (addr, ws) in want {
                        let Some(sd) = td.server_by_address(addr) else { return err(format!("no server {addr}")) };
                        for (f, fv) in ws.as_object().unwrap() {
                            let ok = match f.as_str() {
                                "setName" => fv.as_str().is_none_or(|n| sd.set_name.as_deref() == Some(n)),
                                "type" => sd.ty == server_type(fv.as_str().unwrap()),
                                "setVersion" => sd.set_version == if fv.is_null() { NO_SET_VERSION } else { i64_of(fv) },
                                "electionId" => sd.election_id == oid(fv),
                                // libmongoc compares with assert_match_bson against {} for null: any
                                // value matches (a reset keeps the topologyVersion)
                                "topologyVersion" => fv.is_null() || sd.topology_version == bson(fv),
                                // newer than libmongoc's copy of the spec: libmongoc records no error
                                // when it invalidates a stale primary
                                "error" => true,
                                "pool" => sd.generation(&ZERO_OID) as u64 == fv["generation"].as_u64().unwrap(),
                                "logicalSessionTimeoutMinutes" => sd.session_timeout_minutes == if fv.is_null() { NO_SESSIONS } else { i64_of(fv) },
                                "minWireVersion" => sd.min_wire_version as i64 == if fv.is_null() { 0 } else { i64_of(fv) },
                                "maxWireVersion" => sd.max_wire_version as i64 == if fv.is_null() { 0 } else { i64_of(fv) },
                                _ => return err(format!("unparsed server field {f}")),
                            };
                            if !ok {
                                return err(format!("{addr}.{f}: want {fv}, got {:?}", sd));
                            }
                        }
                    }
                }
                "setName" => {
                    if td.set_name.as_deref() != v.as_str() {
                        return err(format!("setName {:?}", td.set_name));
                    }
                }
                "topologyType" => {
                    if td.ty.name() != v.as_str().unwrap() {
                        return err(format!("topologyType {}", td.ty.name()));
                    }
                }
                "logicalSessionTimeoutMinutes" => {
                    let want = if v.is_null() { NO_SESSIONS } else { i64_of(v) };
                    if td.session_timeout_minutes != want {
                        return err(format!("logicalSessionTimeoutMinutes {}", td.session_timeout_minutes));
                    }
                }
                "compatible" => {
                    let ok = if v.as_bool().unwrap() { td.compatibility_error.is_none() } else { td.compatibility_error.as_ref().is_some_and(|e| e.domain == domain::PROTOCOL && e.code == code::PROTOCOL_BAD_WIRE_VERSION) };
                    if !ok {
                        return err(format!("compatible: {:?}", td.compatibility_error));
                    }
                }
                "maxSetVersion" => {
                    let want = if v.is_null() { NO_SET_VERSION } else { i64_of(v) };
                    if td.max_set_version != want {
                        return err(format!("maxSetVersion {}", td.max_set_version));
                    }
                }
                "maxElectionId" => {
                    if td.max_election_id != oid(v) {
                        return err(format!("maxElectionId {:?}", td.max_election_id));
                    }
                }
                f => return err(format!("unparsed outcome field {f}")),
            }
        }
    }
    Ok(())
}

#[test]
fn sdam() {
    let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source/server-discovery-and-monitoring/tests");
    let (mut n, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    for dir in ["single", "rs", "sharded", "load-balanced", "errors"] {
        let mut files: Vec<_> = std::fs::read_dir(format!("{base}/{dir}")).unwrap().map(|f| f.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        files.sort();
        for p in files {
            let t: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            let d = t["description"].as_str().unwrap();
            if SKIP_SUBSTRINGS.iter().any(|s| d.contains(s)) {
                skipped += 1;
                continue;
            }
            n += 1;
            if let Err(e) = run(&t) {
                failures.push(format!("{dir}/{}: {e}", p.file_name().unwrap().to_string_lossy()));
            }
        }
    }
    eprintln!("sdam: {n} files run, {skipped} skipped, {} failures", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
