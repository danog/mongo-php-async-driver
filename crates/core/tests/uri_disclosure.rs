//! libmongoc 2.5.9's URI diagnostics tests (CVE-2026-129939, test-mongoc-connection-uri.c and
//! test-mongoc-ns-validation.c): parse errors and warnings never quote connection-string input,
//! not even a known option or mechanism in the user's spelling; options are named by their
//! position in the raw query (or TXT record).

use mongo_core::uri::Uri;
use serde_json::Value;

/// check_uri_disclosure: neither the error of a rejected URI nor the warnings logged while
/// parsing (accepted or not) contain `text` (case-insensitively).
fn check(uri: &str, text: &str) {
    let text = text.to_lowercase();
    let warnings = match Uri::parse_with_warnings(uri) {
        Err((e, warnings)) => {
            assert!(!e.to_lowercase().contains(&text), "{uri}: the error discloses {text}: {e}");
            warnings
        }
        Ok(u) => u.warnings,
    };
    for w in &warnings {
        assert!(!w.to_lowercase().contains(&text), "{uri}: a warning discloses {text}: {w}");
    }
}

#[test]
fn credential_disclosure_corpus() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/credential_disclosure/credential-disclosure.json");
    let j: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let tests = j["tests"].as_array().unwrap();
    assert!(!tests.is_empty());
    for t in tests {
        let uri = t["uri"].as_str().unwrap();
        check(uri, "SENTINELpassword1234");
        check(uri, "%53%45%4E%54%49%4E%45%4C%70%61%73%73%77%6F%72%64%31%32%33%34");
    }
}

#[test]
fn diagnostic_input_disclosure() {
    for (uri, input) in [
        ("badScheme://localhost/", "badScheme"),
        ("mongodb://badHost:notaport/", "badHost"),
        ("mongodb://localhost/badDatabase.name", "badDatabase"),
        ("mongodb://localhost/?connectTimeoutMS=654321oops", "654321oops"),
        ("mongodb://localhost/?madeUpOption=value", "madeUpOption"),
        ("mongodb://localhost/?madeUpOption=value&madeUpOption=other", "madeUpOption"),
        ("mongodb://localhost/?madeUpOption", "madeUpOption"),
        ("mongodb://localhost/?readPreferenceTags=brokenTags", "brokenTags"),
        ("mongodb://localhost/?compressors=madeUpCompressor", "madeUpCompressor"),
        ("mongodb://localhost/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:madeUpEnvironment", "madeUpEnvironment"),
        ("mongodb://localhost/?authMechanism=MONGODB-OIDC&authMechanismProperties=madeUpProperty:value", "madeUpProperty"),
        ("mongodb://localhost/?sslCertificateAuthorityFile=firstPath&tlsCAFile=secondPath", "firstPath"),
        ("mongodb://localhost/?sslCertificateAuthorityFile=firstPath&tlsCAFile=secondPath", "secondPath"),
        ("mongodb://localhost/?connectTimeoutMS=654321&madeUpOption=value", "654321"),
        ("mongodb://localhost/?authMechanism=madeUpMechanism", "madeUpMechanism"),
        ("mongodb://localhost/?authMechanism=MONGODB-X509&authSource=madeUpSource", "madeUpSource"),
        // test-mongoc-ns-validation.c
        ("mongodb://localhost/?authSource=SENTINELpassword1234.bad", "SENTINELpassword1234"),
        ("mongodb://localhost/?authSource=SENTINELpassword1234%2Ebad", "SENTINELpassword1234"),
    ] {
        check(uri, input);
    }
    for uri in ["mongodb://localhost/?authSource=SENTINELpassword1234.bad", "mongodb://localhost/?authSource=SENTINELpassword1234%2Ebad"] {
        let e = Uri::parse(uri).unwrap_err();
        assert_eq!(e, "Invalid authSource: database name invalid: contains \".\"");
    }
}

#[test]
fn diagnostic_typed_values() {
    for (uri, message, input) in [
        ("mongodb://localhost/?authMechanism=sCrAm-ShA-1", "'SCRAM-SHA-1'", "sCrAm-ShA-1"),
        ("mongodb://localhost/?authMechanism=sCrAm-ShA-256", "'SCRAM-SHA-256'", "sCrAm-ShA-256"),
        ("mongodb://user@localhost/?authMechanism=pLaIn", "'PLAIN'", "pLaIn"),
        ("mongodb://localhost/?authMechanism=gSsApI", "'GSSAPI'", "gSsApI"),
        ("mongodb://user:@localhost/?authMechanism=mOnGoDb-X509", "'MONGODB-X509'", "mOnGoDb-X509"),
        ("mongodb://user@localhost/?authMechanism=mOnGoDb-AwS", "'MONGODB-AWS'", "mOnGoDb-AwS"),
        ("mongodb://user:@localhost/?authMechanism=mOnGoDb-OiDc", "'MONGODB-OIDC'", "mOnGoDb-OiDc"),
        ("mongodb://localhost/?heartbeatFrequencyMS=%34%32", "of 42", "%34%32"),
        ("mongodb://localhost/?zlibCompressionLevel=%31%30", "of 10", "%31%30"),
        ("mongodb+srv://a.example.com/?srvMaxHosts=-%34%32", "value -42", "-%34%32"),
        ("mongodb://localhost/?wTimeoutMS=-%32%31%34%37%34%38%33%36%34%39", "-2147483649", "-%32%31%34%37%34%38%33%36%34%39"),
        ("mongodb://localhost/?w=-00042", "[w=-42]", "-00042"),
        ("mongodb://localhost/?w=0000&journal=true", "[w=0]", "0000"),
    ] {
        let e = Uri::parse(uri).unwrap_err();
        assert!(e.contains(message), "{uri}: {e}");
        // the exact spelling, casing and numeric encoding included, is never copied
        assert!(!e.contains(input), "{uri}: {e}");
    }
    for (uri, message) in [("mongodb://localhost/?maxStalenessSeconds=-42", "\"-42\""), ("mongodb://localhost/?maxAdaptiveRetries=-42", "of -42")] {
        let u = Uri::parse(uri).unwrap();
        assert!(u.warnings.iter().any(|w| w.contains(message)), "{uri}: {:?}", u.warnings);
    }
    check("mongodb://localhost/?heartbeatFrequencyMS=42oops", "42oops");
    check("mongodb://localhost/?unknownNumericOption=654321", "654321");
    check("mongodb://localhost/?authMechanism=MONGODB-X509&authSource=654321", "654321");
    check("mongodb://localhost/?tlsCertificateKeyFilePassword=654321&tlsCertificateKeyFilePassword=123456", "654321");
    check("mongodb://localhost/?w=42oops&journal=true", "42oops");
    check("mongodb://localhost:654321/", "654321");
    // the mechanism is kept as given; only diagnostics use the canonical name
    let u = Uri::parse("mongodb://user:pass@localhost/?authMechanism=sCrAm-ShA-256").unwrap();
    assert_eq!(u.auth_mechanism(), Some("sCrAm-ShA-256"));
}

#[test]
fn diagnostic_positions() {
    for (uri, warning) in [
        ("mongodb://localhost/?unknown=value", "query position 0"),
        ("mongodb://user:short@localhost/?appname=x&unknown=value", "query position 10"),
        ("mongodb://user:muchlongerpassword@localhost/?appname=x&unknown=value", "query position 10"),
        ("mongodb://localhost/?appname=\u{e9}&unknown=value", "query position 10"),
        ("mongodb://localhost/?appname=\u{20ac}\u{1f600}&unknown=value", "query position 11"),
        ("mongodb://localhost/?appname=e\u{301}&unknown=value", "query position 11"),
        ("mongodb://localhost/?appname=%C3%A9&unknown=value", "query position 15"),
    ] {
        let u = Uri::parse(uri).unwrap();
        assert!(u.warnings.iter().any(|w| w.contains(warning)), "{uri}: {:?}", u.warnings);
    }
    // libmongoc logs this one as the warning of mongoc_uri_new
    let e = Uri::parse("mongodb://localhost/?SSL=true&tls=false").unwrap_err();
    assert!(e.contains("canonical name 'tls'"), "{e}");
    let e = Uri::parse("mongodb://localhost/?appname=x&missingEquals").unwrap_err();
    assert!(e.contains("query position 10"), "{e}");

    // TXT records never override the URI's options, and are not quoted
    let mut u = Uri::parse("mongodb://localhost/?replicaSet=original").unwrap();
    u.apply_query_string("replicaSet=SENTINELpassword1234", true).unwrap();
    assert!(u.warnings.iter().any(|w| w.contains("Ignoring URI option \"replicaset\"")), "{:?}", u.warnings);
    assert_eq!(u.options.str("replicaset"), Some("original"));
    u.set_auth_source("original");
    u.apply_query_string("authSource=SENTINELpassword1234", true).unwrap();
    u.apply_query_string("authSource=SENTINELpassword1234", true).unwrap();
    assert_eq!(u.auth_source(), "original");
    assert!(u.warnings.iter().all(|w| !w.contains("SENTINEL")), "{:?}", u.warnings);
    for (options, message) in [
        ("SENTINELpassword1234=value", "TXT record position 0 prohibited"),
        ("SENTINELpassword1234", "TXT record position 0 contains no \"=\""),
        ("replicaSet=SENTINELpassword1234%zz", "TXT record position 0 is invalid"),
        ("authSource=\u{e9}&replicaSet=SENTINELpassword1234%zz", "TXT record position 13 is invalid"),
    ] {
        let e = u.apply_query_string(options, true).unwrap_err();
        assert!(!e.to_lowercase().contains("sentinelpassword1234"), "{options}: {e}");
        assert!(e.contains(message), "{options}: {e}");
    }
}

/// The messages as libmongoc 2.5.9 words them.
#[test]
fn messages() {
    for (uri, message) in [
        ("mongodb://h/?appname=x&unknown=v", None),
        ("foo://h/", Some("Invalid URI scheme. Expected one of \"mongodb://\" or \"mongodb+srv://\"")),
        ("mongodb://h:x/", Some("Invalid host specifier: Invalid host specifier: Invalid port string: Port string is not a valid integer")),
        ("mongodb://::1]/", Some("Invalid host specifier: Invalid IPv6 literal address: Must start with a bracket '['")),
        ("mongodb://[::1]:x/", Some("Invalid host specifier: Invalid IPv6 literal address: Invalid port: Port string is not a valid integer")),
        ("mongodb://h%zz/", Some("Invalid host specifier: Invalid %-sequence: expected two hexadecimal digits")),
        ("mongodb://h/db%2", Some("Invalid database specifier: Truncated %-sequence: expected two hexadecimal digits")),
        ("mongodb://h/d.b", Some("Invalid database specifier: Contains disallowed characters")),
        ("mongodb+srv://a.b/", Some("Invalid SRV service name: expected a hostname with at least three components")),
        ("mongodb+srv://a%2/", Some("Invalid SRV service name: Truncated %-sequence: expected two hexadecimal digits")),
        ("mongodb://h/?replicaSet=", Some("Value for URI option \"replicaset\" cannot be empty string")),
        ("mongodb://h/?readPreferenceTags=a", Some("Unsupported value for \"readpreferencetags\"")),
        ("mongodb://h/?connectTimeoutMS=x", Some("Unsupported value for \"connecttimeoutms\"")),
        ("mongodb://h/?TLS=maybe", Some("Unsupported value for \"tls\"")),
        ("mongodb://h/?readPreference=x", Some("Error while assigning URI read preference: Unsupported readPreference value")),
        ("mongodb://h/?wTimeoutMS=-1", Some("Unsupported wtimeoutMS value -1: must be non-negative")),
        ("mongodb://h/?zlibCompressionLevel=10", Some("Invalid \"zlibcompressionlevel\" of 10: must be between -1 and 9")),
        ("mongodb://h/?heartbeatFrequencyMS=1", Some("Invalid \"heartbeatfrequencyms\" of 1: must be at least 500")),
        ("mongodb://u@h/?authMechanism=x", Some("Unsupported value for authMechanism: must be one of ['MONGODB-OIDC', 'SCRAM-SHA-1', 'SCRAM-SHA-256', 'PLAIN', 'MONGODB-X509', 'GSSAPI', 'MONGODB-AWS']")),
        ("mongodb://h/?authMechanism=mongodb-x509&authSource=x", Some("'MONGODB-X509' authentication mechanism requires \"$external\" authSource")),
        ("mongodb://u@h/?authMechanism=gssapi&authMechanismProperties=X:y", Some("Unsupported 'GSSAPI' authentication mechanism property")),
        ("mongodb://h/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:x", Some("'MONGODB-OIDC' authentication has unrecognized ENVIRONMENT property")),
        ("mongodb://u@h/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:test", Some("'MONGODB-OIDC' authentication environment does not accept a username")),
        ("mongodb://h/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:test,TOKEN_RESOURCE:r", Some("'MONGODB-OIDC' authentication environment does not accept TOKEN_RESOURCE")),
        ("mongodb://h/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:gcp", Some("'MONGODB-OIDC' authentication environment requires TOKEN_RESOURCE")),
    ] {
        match (Uri::parse(uri), message) {
            (Ok(_), None) => {}
            (Err(e), Some(m)) => assert_eq!(e, m, "{uri}"),
            (r, m) => panic!("{uri}: {:?} (expected {m:?})", r.map(|u| u.warnings)),
        }
    }
    let warnings = |uri: &str| Uri::parse(uri).unwrap().warnings;
    assert_eq!(warnings("mongodb://h/?appname=a&appName=b"), ["Overwriting previously provided value for 'appname'"]);
    assert_eq!(warnings("mongodb://h/?x=a&X=b"), ["Unsupported URI option at query position 0", "Overwriting previously provided URI option at query position 4", "Unsupported URI option at query position 4"]);
    assert_eq!(warnings("mongodb://h/?gssapiServiceName=s"), ["Unsupported URI option at query position 0", "gssapiservicename is deprecated, use authmechanismproperties with SERVICE_NAME instead"]);
    assert_eq!(warnings("mongodb://h/?compressors=zli"), ["Unsupported compressor in URI option \"compressors\""]);
    assert_eq!(warnings("mongodb://h/?ssl=yes"), ["Deprecated boolean value for \"tls\", please update to \"tls=true\""]);
    assert_eq!(warnings("mongodb://h/?tls="), ["Empty value provided for \"tls\""]);
    assert_eq!(warnings("mongodb://h/?maxAdaptiveRetries=x"), ["Unsupported value for \"maxadaptiveretries\": expected an integer"]);
    assert_eq!(warnings("mongodb://h/?enableOverloadRetargeting=x"), ["Unsupported value for \"enableoverloadretargeting\": expected a boolean"]);
}
