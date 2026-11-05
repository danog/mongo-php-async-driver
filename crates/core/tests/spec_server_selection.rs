//! The server-selection (server_selection, rtt) and max-staleness specification tests against
//! the mongoc-topology-description.c port, run the way libmongoc's json-test.c runs them
//! (libmongoc checks in_latency_window only). in_window is not run: it tests the
//! operationCount-based "power of two choices", which libmongoc does not implement.

use mongo_core::bson::json_parse::from_json;
use mongo_core::sdam::{ServerDescription, ServerType, RTT_UNSET};
use mongo_core::topology::{OpType, ReadPrefs, TopologyDescription, TopologyType, HEARTBEAT_FREQUENCY_MS_SINGLE_THREADED, LOCAL_THRESHOLD_MS};
use mongo_core::uri::ReadMode;
use serde_json::Value;

fn topology_type(s: &str) -> TopologyType {
    match s {
        "Unknown" => TopologyType::Unknown,
        "Sharded" => TopologyType::Sharded,
        "ReplicaSetNoPrimary" => TopologyType::RsNoPrimary,
        "ReplicaSetWithPrimary" => TopologyType::RsWithPrimary,
        "Single" => TopologyType::Single,
        "LoadBalanced" => TopologyType::LoadBalanced,
        t => panic!("topology type {t}"),
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

fn read_mode(s: &str) -> ReadMode {
    match s {
        "Primary" => ReadMode::Primary,
        "Secondary" => ReadMode::Secondary,
        "PrimaryPreferred" => ReadMode::PrimaryPreferred,
        "SecondaryPreferred" => ReadMode::SecondaryPreferred,
        "Nearest" => ReadMode::Nearest,
        m => panic!("read mode {m}"),
    }
}

fn i64_of(v: &Value) -> i64 {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).or_else(|| v.get("$numberLong").and_then(Value::as_str).and_then(|s| s.parse().ok())).unwrap_or_else(|| panic!("{v}"))
}

fn run(t: &Value) -> Result<(), String> {
    let hb = t.get("heartbeatFrequencyMS").map_or(HEARTBEAT_FREQUENCY_MS_SINGLE_THREADED, i64_of);
    let tdj = &t["topology_description"];
    let mut td = TopologyDescription::new(topology_type(tdj["type"].as_str().unwrap()), hb);
    for (i, s) in tdj["servers"].as_array().unwrap().iter().enumerate() {
        let mut sd = ServerDescription::new(i as u32, s["address"].as_str().unwrap());
        sd.ty = server_type(s["type"].as_str().unwrap());
        match s.get("avg_rtt_ms") {
            Some(v) => sd.rtt_ms = i64_of(v),
            None if sd.ty != ServerType::Unknown => return Err("no avg_rtt_ms".into()),
            None => {}
        }
        if let Some(v) = s.get("maxWireVersion") {
            sd.max_wire_version = i64_of(v) as i32;
        }
        if let Some(v) = s.get("lastUpdateTime") {
            sd.last_update_time_usec = i64_of(v) * 1000;
        }
        if let Some(v) = s.get("lastWrite") {
            sd.last_write_date_ms = i64_of(&v["lastWriteDate"]);
        }
        if let Some(v) = s.get("tags") {
            sd.tags = from_json(serde_json::to_string(v).unwrap().as_bytes()).unwrap();
        }
        td.servers.push(sd);
    }
    let mut rp = ReadPrefs::default();
    let rpj = &t["read_preference"];
    if let Some(m) = rpj.get("mode") {
        rp.mode = read_mode(m.as_str().unwrap());
    }
    if let Some(sets) = rpj.get("tag_sets").and_then(Value::as_array) {
        // as libmongoc's runner: tags are set unless the first set is empty
        if sets.first().and_then(Value::as_object).is_some_and(|f| !f.is_empty()) {
            rp.tags = sets.iter().map(|s| s.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned())).collect()).collect();
        }
    }
    if let Some(v) = rpj.get("maxStalenessSeconds") {
        rp.max_staleness_seconds = i64_of(v);
    }
    let op = match t.get("operation").and_then(Value::as_str) {
        None | Some("read") => OpType::Read,
        Some("write") => OpType::Write,
        Some(o) => panic!("operation {o}"),
    };
    let ds: Vec<u32> = t.get("deprioritized_servers").and_then(Value::as_array).map_or_else(Vec::new, |a| a.iter().filter_map(|s| td.server_by_address(s["address"].as_str().unwrap()).map(|x| x.id)).collect());
    let expect_error = t.get("error").and_then(Value::as_bool) == Some(true);
    let ok = rp.is_valid() && td.compatible(Some(&rp)).is_ok();
    if expect_error {
        return if ok { Err("no error".into()) } else { Ok(()) };
    }
    if !ok {
        return Err(format!("error: valid {} / {:?}", rp.is_valid(), td.compatible(Some(&rp))));
    }
    let (set, _) = td.suitable_servers(op, Some(&rp), &ds, LOCAL_THRESHOLD_MS);
    let mut got: Vec<&str> = set.iter().map(|&i| td.servers[i].addr.as_str()).collect();
    let mut want: Vec<&str> = t["in_latency_window"].as_array().unwrap().iter().map(|s| s["address"].as_str().unwrap()).collect();
    got.sort();
    want.sort();
    if got != want {
        return Err(format!("in_latency_window {got:?} != {want:?}"));
    }
    Ok(())
}

fn json_files(dir: &str, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            json_files(p.to_str().unwrap(), out);
        } else if p.extension().is_some_and(|e| e == "json") {
            out.push(p);
        }
    }
}

#[test]
fn server_selection_and_max_staleness() {
    let base = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source");
    let mut files = Vec::new();
    json_files(&format!("{base}/server-selection/tests/server_selection"), &mut files);
    json_files(&format!("{base}/max-staleness/tests"), &mut files);
    files.sort();
    let mut failures = Vec::new();
    for p in &files {
        let t: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        if let Err(e) = run(&t) {
            failures.push(format!("{}: {e}", p.strip_prefix(base).unwrap().display()));
        }
    }
    eprintln!("server selection + max staleness: {} files, {} failures", files.len(), failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rtt() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source/server-selection/tests/rtt");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|e| e != "json") {
            continue;
        }
        n += 1;
        let t: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let mut sd = ServerDescription::new(1, "localhost:27017");
        sd.rtt_ms = if t["avg_rtt_ms"] == "NULL" { RTT_UNSET } else { i64_of(&t["avg_rtt_ms"]) };
        sd.update_rtt(i64_of(&t["new_rtt_ms"]));
        assert_eq!(sd.rtt_ms, i64_of(&t["new_avg_rtt"]), "{}", p.display());
    }
    eprintln!("rtt: {n} cases");
}
