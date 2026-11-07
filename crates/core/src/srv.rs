//! `mongodb+srv://` seed lists (Initial DNS Seedlist Discovery) and SRV polling for sharded
//! clusters, as libmongoc does them: mongoc_topology_new (SRV + TXT lookups, the TXT options,
//! the seed list), mongoc_topology_rescan_srv (single-threaded: at most once per rescan
//! interval, at the start of a scan, only while the topology is Sharded or Unknown) and
//! _mongoc_client_get_rr / _mongoc_get_rr_search (libresolv's answers to records).
//!
//! DNS is only queried when a client is created for an SRV URI and when one of its scans is
//! due: never per operation.

use crate::dns::{self, LookupError};
use crate::error::{code, domain, Error};
use crate::uri::{Host, Uri};

/// MONGOC_TOPOLOGY_MIN_RESCAN_SRV_INTERVAL_MS
pub const MIN_RESCAN_INTERVAL_MS: i64 = 60_000;

/// mongoc_rr_data_t
#[derive(Default, Debug)]
pub struct RrData {
    pub hosts: Vec<Host>,
    pub min_ttl: u32,
    pub txt_record_opts: Option<String>,
}

/// The SRV polling state of a client (srv_polling_last_scan_ms, srv_polling_rescan_interval_ms).
#[derive(Clone, Copy, Debug)]
pub struct Polling {
    pub last_scan_ms: i64,
    pub interval_ms: i64,
}

fn err(msg: String) -> Error {
    Error::new(domain::STREAM, code::STREAM_NAME_RESOLUTION, msg)
}

fn now_ms() -> i64 {
    crate::sdam::monotonic_usec() / 1000
}

/// MONGOC_EXPERIMENTAL_SRV_PREFER_TCP
fn prefer_tcp() -> bool {
    std::env::var_os("MONGOC_EXPERIMENTAL_SRV_PREFER_TCP").is_some()
}

/// The "_<srvServiceName>._tcp.<host>" name of the URI's SRV records.
pub fn prefixed_hostname(uri: &Uri) -> String {
    format!("_{}._tcp.{}", uri.options.str("srvservicename").unwrap_or("mongodb"), uri.srv.as_deref().unwrap_or_default())
}

/// _mongoc_get_rr_dnsapi (Windows): SRV records (hosts) or the TXT record (options) of
/// `hostname`, through the system resolver (DnsQuery_UTF8, bypassing its cache), with
/// libmongoc's messages. A failed TXT lookup is not an error (only a second TXT record is).
#[cfg(windows)]
pub fn get_rr(_cfg: &dns::Config, hostname: &str, srv: bool, rr: &mut RrData) -> Result<(), Error> {
    use std::ffi::{CStr, CString};
    use windows_sys::Win32::NetworkManagement::Dns as d;
    let (ty, name) = if srv { (d::DNS_TYPE_SRV, "SRV") } else { (d::DNS_TYPE_TEXT, "TXT") };
    let fail = |m: String| if srv { Err(err(m)) } else { Ok(()) };
    // a C string: whatever follows a NUL is not part of it
    let c = CString::new(hostname.split('\0').next().unwrap_or_default()).unwrap_or_default();
    let mut options = d::DNS_QUERY_BYPASS_CACHE;
    if prefer_tcp() {
        options |= d::DNS_QUERY_USE_TCP_ONLY;
    }
    /// The record list, freed when dropped.
    struct List(*mut d::DNS_RECORDA);
    impl Drop for List {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the list DnsQuery_UTF8 returned, freed once
                unsafe { d::DnsFree(self.0.cast(), d::DnsFreeRecordList) };
            }
        }
    }
    let mut list = List(std::ptr::null_mut());
    // SAFETY: a NUL-terminated name; the result list is owned by `list`
    let res = unsafe { d::DnsQuery_UTF8(c.as_ptr().cast(), ty, options, std::ptr::null_mut(), &mut list.0, std::ptr::null_mut()) };
    if res != 0 {
        return fail(format!("Failed to look up {name} record \"{hostname}\": {}", crate::os::error_text(res)));
    }
    if list.0.is_null() {
        return fail(format!("No {name} records for \"{hostname}\""));
    }
    let mut matching = 0;
    let mut seen = 0;
    let mut p = list.0 as *const d::DNS_RECORDA;
    while !p.is_null() {
        // SAFETY: a record of the list, valid until it is freed
        let r = unsafe { &*p };
        seen += 1;
        // as many records as a resolver answer may give (dns::parse_answers)
        if seen > dns::MAX_RECORDS && srv {
            return Err(err(format!("Too many records in {name} answer for \"{hostname}\": at most {} are accepted", dns::MAX_RECORDS)));
        }
        // records of other types may come back
        if r.wType == ty {
            if matching > 0 && !srv {
                return Err(err(format!("Multiple TXT records for \"{hostname}\"")));
            }
            if matching == 0 || r.dwTtl < rr.min_ttl {
                rr.min_ttl = r.dwTtl;
            }
            if srv {
                // SAFETY: an SRV record's data; the target is a NUL-terminated UTF-8 string
                let s = unsafe { r.Data.SRV };
                let target = if s.pNameTarget.is_null() { String::new() } else { unsafe { CStr::from_ptr(s.pNameTarget.cast()) }.to_string_lossy().into_owned() };
                let host = crate::uri::host_from_hostport(&target, s.wPort).map_err(err)?;
                upsert(&mut rr.hosts, host);
            } else {
                // SAFETY: a TXT record's data: dwStringCount NUL-terminated strings
                let t = std::ptr::addr_of!(r.Data.TXT);
                let count = unsafe { (*t).dwStringCount } as usize;
                let strings = unsafe { std::ptr::addr_of!((*t).pStringArray) }.cast::<*const u8>();
                let mut txt = Vec::with_capacity(usize::from(r.wDataLength));
                for i in 0..count {
                    let sp = unsafe { *strings.add(i) };
                    if !sp.is_null() {
                        txt.extend_from_slice(unsafe { CStr::from_ptr(sp.cast()) }.to_bytes());
                    }
                }
                rr.txt_record_opts = Some(String::from_utf8_lossy(&txt).into_owned());
            }
            matching += 1;
        }
        p = r.pNext;
    }
    if matching == 0 {
        return fail(format!("No matching {name} records for \"{hostname}\""));
    }
    Ok(())
}

/// _mongoc_get_rr_search: SRV records (hosts) or the TXT record (options) of `hostname`.
/// A failed TXT lookup is not an error (only a malformed or second TXT record is).
#[cfg(unix)]
pub fn get_rr(cfg: &dns::Config, hostname: &str, srv: bool, rr: &mut RrData) -> Result<(), Error> {
    let (ty, name) = if srv { (dns::T_SRV, "SRV") } else { (dns::T_TXT, "TXT") };
    let m = match dns::search(cfg, hostname, ty, prefer_tcp()) {
        Ok(m) => m,
        Err(e) if srv => return Err(err(format!("Failed to look up {name} record \"{hostname}\": {}", e.message()))),
        Err(_) => return Ok(()),
    };
    let records = match dns::parse_answers(&m) {
        Ok(r) => r,
        Err("too many records") if srv => return Err(err(format!("Too many records in {name} answer for \"{hostname}\": at most {} are accepted", dns::MAX_RECORDS))),
        Err(_) if srv => return Err(err(format!("Invalid {name} answer for \"{hostname}\""))),
        Err(_) => return Ok(()),
    };
    if records.is_empty() {
        return if srv { Err(err(format!("No {name} records for \"{hostname}\""))) } else { Ok(()) };
    }
    let mut matching = 0;
    for (i, r) in records.iter().enumerate() {
        // CDRIVER-3628: records of other types may come back
        if r.rtype != ty {
            continue;
        }
        if matching > 0 && !srv {
            return Err(err(format!("Multiple TXT records for \"{hostname}\"")));
        }
        matching += 1;
        if i == 0 || r.ttl < rr.min_ttl {
            rr.min_ttl = r.ttl;
        }
        let data = &m[r.rdata.0..r.rdata.1];
        if srv {
            srv_record(hostname, &m, r.rdata, rr)?;
        } else {
            txt_record(hostname, data, rr)?;
        }
    }
    if matching == 0 {
        return if srv { Err(err(format!("No matching {name} records for \"{hostname}\""))) } else { Ok(()) };
    }
    Ok(())
}

#[cfg_attr(not(unix), allow(dead_code))]
/// srv_callback: priority, weight, port, target (the record must hold all of them; libmongoc
/// reads the port without checking the record's length).
fn srv_record(hostname: &str, m: &[u8], (start, end): (usize, usize), rr: &mut RrData) -> Result<(), Error> {
    let invalid = || err(format!("Invalid record in SRV answer for \"{hostname}\": \"An unknown error occurred.\""));
    if end - start < 7 {
        return Err(invalid());
    }
    let port = u16::from_be_bytes([m[start + 4], m[start + 5]]);
    // the target must end inside the record
    let target = dns::expand_name(&m[..end], start + 6).map_err(|_| invalid())?;
    let host = crate::uri::host_from_hostport(&target, port).map_err(err)?;
    upsert(&mut rr.hosts, host);
    Ok(())
}

#[cfg_attr(not(unix), allow(dead_code))]
/// txt_callback: the record's strings concatenated (at most 255 bytes in all, as libmongoc).
fn txt_record(hostname: &str, data: &[u8], rr: &mut RrData) -> Result<(), Error> {
    let total = data.len();
    if !(1..=255).contains(&total) {
        return Err(err(format!("Invalid TXT record size {total} for \"{hostname}\"")));
    }
    let mut txt = Vec::with_capacity(total);
    let mut pos = 0;
    while pos < total {
        let len = data[pos] as usize;
        pos += 1;
        if total - pos < len {
            return Err(err(format!("Invalid TXT string size {len} at {pos} in {total}-byte TXT record for \"{hostname}\"")));
        }
        txt.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    rr.txt_record_opts = Some(String::from_utf8_lossy(&txt).into_owned());
    Ok(())
}

/// _mongoc_host_list_upsert: a host already listed (same host_and_port, any case) is replaced.
fn upsert(list: &mut Vec<Host>, h: Host) {
    match list.iter().position(|x| x.host_and_port.eq_ignore_ascii_case(&h.host_and_port)) {
        Some(i) => list[i] = h,
        None => list.push(h),
    }
}

/// mongoc_uri_validate_srv_result: `host` must be a descendant of the service's parent domain
/// (the SRV host name without its first label).
pub fn validate(uri: &Uri, host: &str) -> Result<(), Error> {
    let srv_hostname = uri.srv.as_deref().unwrap_or_default();
    let bad = || err(format!("Invalid host \"{host}\" returned for service \"{srv_hostname}\": host must be subdomain of service name"));
    if host.len() <= 1 || host.starts_with('.') {
        return Err(bad());
    }
    let srv_host = &srv_hostname[srv_hostname.find('.').unwrap_or(srv_hostname.len())..];
    if host.len() < srv_host.len() || !host.ends_with(srv_host) {
        return Err(bad());
    }
    Ok(())
}

/// mongoc_topology_new for an SRV URI: the seed list and the TXT options, applied to `uri`;
/// the polling state. Errors are what the client creation fails with.
pub fn initial(uri: &mut Uri) -> Result<Option<Polling>, Error> {
    if uri.srv.is_none() {
        return Ok(None);
    }
    // mongoc_topology_new logs a CosmosDB/DocumentDB host before the lookups
    crate::client::detect_nongenuine_hosts(uri);
    let cfg = dns::Config::system();
    let mut rr = RrData::default();
    get_rr(&cfg, &prefixed_hostname(uri), true, &mut rr)?;
    let srv_hostname = uri.srv.clone().unwrap_or_default();
    get_rr(&cfg, &srv_hostname, false, &mut rr)?;
    if let Some(opts) = &rr.txt_record_opts {
        uri.apply_query_string(opts, true).map_err(|m| Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, m))?;
    }
    // mongoc_uri_init_with_srv_host_list
    for h in rr.hosts {
        validate(uri, &h.host)?;
        upsert(&mut uri.hosts, h);
    }
    Ok(Some(Polling { last_scan_ms: now_ms(), interval_ms: (i64::from(rr.min_ttl) * 1000).max(MIN_RESCAN_INTERVAL_MS) }))
}

/// mongoc_topology_rescan_srv (the caller checks the topology type): the hosts to reconcile
/// the topology with, or None (nothing to change; the reason is logged as libmongoc logs it).
pub fn rescan(uri: &Uri, p: &mut Polling, heartbeat_ms: i64) -> Option<Vec<String>> {
    let now = now_ms();
    if now < p.last_scan_ms + p.interval_ms {
        return None;
    }
    let mut rr = RrData::default();
    let r = get_rr(&dns::Config::system(), &prefixed_hostname(uri), true, &mut rr);
    p.last_scan_ms = now_ms();
    if let Err(e) = r {
        p.interval_ms = heartbeat_ms;
        crate::log::error(crate::log::MONGOC, || format!("SRV polling error: {}", e.message));
        return None;
    }
    p.interval_ms = (i64::from(rr.min_ttl) * 1000).max(MIN_RESCAN_INTERVAL_MS);
    let mut valid: Vec<Host> = Vec::new();
    for h in rr.hosts {
        if validate(uri, &h.host).is_ok() {
            upsert(&mut valid, h);
        } else {
            crate::log::error(crate::log::MONGOC, || format!("Invalid host returned by SRV: {}", h.host_and_port));
        }
    }
    if valid.is_empty() {
        crate::log::error(crate::log::MONGOC, || "SRV response did not contain any valid hosts".to_owned());
        p.interval_ms = heartbeat_ms;
        return None;
    }
    Some(valid.into_iter().map(|h| h.host_and_port).collect())
}

/// LookupError is re-exported for the tests.
pub type Lookup = LookupError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records() {
        let mut rr = RrData::default();
        assert!(txt_record("h", b"", &mut rr).is_err());
        assert_eq!(txt_record("h", b"\x05abc", &mut rr).unwrap_err().message, "Invalid TXT string size 5 at 1 in 4-byte TXT record for \"h\"");
        txt_record("h", b"\x0dauthSource=db\x0e&replicaSet=rs", &mut rr).unwrap();
        assert_eq!(rr.txt_record_opts.as_deref(), Some("authSource=db&replicaSet=rs"));
        let uri = Uri::parse("mongodb+srv://test1.test.build.10gen.cc/").unwrap();
        assert!(validate(&uri, "localhost.test.build.10gen.cc").is_ok());
        assert!(validate(&uri, "localhost.build.10gen.cc").is_err());
        assert!(validate(&uri, "evil.test.build.10gen.cc.example.com").is_err());
        assert!(validate(&uri, ".test.build.10gen.cc").is_err());
        // a record too short for a port and a target
        let m = [0u8; 18];
        assert!(srv_record("h", &m, (12, 18), &mut rr).is_err());
    }

    /// The system resolver on Windows (the initial DNS seedlist spec's public test records).
    #[cfg(windows)]
    #[test]
    fn windows_dnsapi() {
        let cfg = dns::Config::default();
        let mut rr = RrData::default();
        get_rr(&cfg, "_mongodb._tcp.test1.test.build.10gen.cc", true, &mut rr).unwrap();
        let mut hosts: Vec<_> = rr.hosts.iter().map(|h| h.host_and_port.clone()).collect();
        hosts.sort();
        assert_eq!(hosts, ["localhost.test.build.10gen.cc:27017", "localhost.test.build.10gen.cc:27018"]);
        let mut rr = RrData::default();
        get_rr(&cfg, "test5.test.build.10gen.cc", false, &mut rr).unwrap();
        assert_eq!(rr.txt_record_opts.as_deref(), Some("replicaSet=repl0&authSource=thisDB"));
        // libmongoc's message, with the system's text of the DNS status
        let e = get_rr(&cfg, "_mongodb._tcp.nonexistent.test.build.10gen.cc", true, &mut RrData::default()).unwrap_err();
        assert!(e.message.starts_with("Failed to look up SRV record \"_mongodb._tcp.nonexistent.test.build.10gen.cc\": (0x0000"), "{}", e.message);
        // a failed TXT lookup is no error
        get_rr(&cfg, "nonexistent.test.build.10gen.cc", false, &mut RrData::default()).unwrap();
    }
}
