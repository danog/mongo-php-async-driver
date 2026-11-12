# Tests that do not apply, cannot run here, or are expected to fail

Nothing is dropped silently: every carried-over test either runs, or is listed here with the
reason. The current counts are in the summary of each CI run (`tools/ci/junit-summary.py`).

Reference numbers for `tests/phpt` against a standalone mongod 8.0 (PHP 8.4), with
`tools/phpt.sh`:

| build | PASS | FAIL | SKIP | XFAIL |
|---|---|---|---|---|
| ext-mongodb 2.5.6 (reference, built from source) | 2072 | 4 | 180 | 11 |
| ext-mongodb 2.1.4 (distribution package; the suite is 2.5.6's) | 1891 | 185 | 180 | 11 |
| this extension, API surface only (no method ported) | 55 | 1652 | 550 | 10 |

The 4 reference failures depend on the server environment (fail points, a writeConcernError
without message, a missing key in ClientEncryption::deleteKey()); they are tracked, not exempted.

## tests/phpt (mongo-php-driver)

### Need infrastructure the test host lacks (skipped, not counted as failures)

| Tests | Needs |
|---|---|
| `atlas.phpt` | Atlas clusters and credentials (`TESTS=tests/atlas.phpt`) |
| `frankenphp/` | Docker: not a PHPT but a script (FrankenPHP reboots its PHP threads in-process, PHPC-2797); its Dockerfile builds the C extension, so the CI job `frankenphp` builds this one into the same image with `tools/ci/frankenphp.Dockerfile` and runs `restart-test.sh` (the commands are in that file) |
| `ocsp-success.phpt`, `ocsp-failure.phpt` | an OCSP responder and certificates from drivers-evergreen-tools (run only with `TESTS=tests/ocsp-*.phpt`); the same checks are covered by `cargo test --test ocsp` (rustls test server with stapled responses, local HTTP responder) |
| `clientEncryption/*` using AWS, Azure, GCP or KMIP KMS providers | KMS credentials / a KMIP server; the local KMS provider runs |
| `standalone-x509-*`, `standalone-plain-*`, `standalone-ssl-*` | a mongod with TLS (and X.509 for the x509 ones): `tools/tls-certs.sh $SSL_DIR` + `SSL_DIR=... tools/topo.sh start tls`, then `MONGODB_URI=mongodb://localhost:28016/?ssl=true`; for X.509 and PLAIN `SSL_DIR=... tools/topo.sh start x509` and `MONGODB_URI=mongodb://127.0.0.1:28014/?tls=true&authMechanism=MONGODB-X509&tlsCAFile=$SSL_DIR/ca.pem&tlsCertificateKeyFile=$SSL_DIR/client.pem` (`standalone-x509-auth-001` passes, `-error-0001` and both `standalone-plain-*` are XFAIL upstream: PLAIN users need LDAP, so `tests/local/manager-x509-plain-001.phpt` checks the PLAIN exchange up to the server's answer; `standalone-x509-extract_username-001` skips: it requires the TLS library to be OpenSSL, Secure Transport or Secure Channel by name, and this build reports rustls; its check is the "x509, no user name" case of `tests/local/manager-x509-plain-001.phpt`). `standalone-ssl-no_verify-003` and `-verify_cert-003` still skip there (their `skip_if_not_live()` connects without the test CA); `-verify_cert-error-003` passes (the reference fails it: it reports "connection closed"). `tests/local/tls-verify-001.phpt` covers the TLS matrix. The SCRAM tests (`connect/standalone-auth-*`, `tests/local/manager-scram-001.phpt`) run with `tools/topo.sh auth` and `MONGODB_URI=mongodb://root:toor@127.0.0.1:28015/?authSource=admin` |

### Observe libmongoc internals (to be emulated, or documented as differences)

| Tests | Why |
|---|---|
| libmongoc TRACE lines in `mongodb.debug` output (function entry/exit, hex dumps of wire messages) | not reproduced: they name libmongoc's C functions, and the dumps hold credentials (see `DIVERGENCES.md`). The level/domain/message lines of the ported code paths are emulated, and every upstream test reading `mongodb.debug` or using `LogSubscriber` matches them with `%A`; none needs a TRACE line |
| phpinfo() rows `libmongoc SSL library`, `libmongoc crypto library`, `libmongocrypt crypto library` | report `rustls` / `RustCrypto` instead of `OpenSSL` / `libcrypto` (no test checks them) |
| `manager/manager-ctor-ssl-003` | skips: it requires the libmongoc SSL library to be OpenSSL by name, and this build reports rustls. Its body (a `ca_dir` driver option implicitly enables TLS, so a ping to a non-TLS server fails with `ConnectionException`) passes here, checked by hand on the standalone and auth topologies |

### Expected to fail until their phase lands

| Phase | Directories |
|---|---|
| 2 (BSON) | `bson/`, `bson-corpus/`, `bson-binary-vector/` |
| 3 (driver core) | `manager/`, `server/`, `serverDescription/`, `topologyDescription/`, `query/`, `command/`, `cursor/`, `bulk/`, `bulkwritecommand/`, `writeConcern/`, `writeConcernError/`, `writeError/`, `writeResult/`, `readConcern/`, `readPreference/`, `exception/`, `functional/`, `standalone/`, `replicaset/`, `connect/` (non-auth) |
| 4 (sessions, retries) | `session/`, `causal-consistency/`, `retryable-reads/`, `retryable-writes/`, `serverApi/` |
| 5 (auth, TLS, SRV) | `connect/` (auth, TLS) |
| 6 (monitoring) | `apm/`, `logging/` |
| 7 (encryption) | `clientEncryption/` |

## tests/phplib (mongo-php-library)

| Tests | Status |
|---|---|
| Unit tests (no server) | phase 2-3 |
| Functional tests, `UnifiedSpecTests` (CRUD, transactions, sessions, retryable, change streams, GridFS, command monitoring, versioned API, run-command, index/collection management) | phases 3-6 |
| `UnifiedSpecTests` for load balancers | need a load-balanced topology (phase 6; not on ct yet) |
| `SpecTests/ClientSideEncryptionSpecTest`, CSFLE prose tests | phase 7; KMS-provider cases need credentials (AWS, Azure, GCP, KMIP) and are skipped on ct |
| Atlas search index tests | need Atlas: skipped |
| Kerberos, MONGODB-AWS, MONGODB-OIDC auth tests | need a KDC / AWS / an identity provider: not runnable on ct |

### PHPLIB 2.5.1 results (tools/phplib.sh; ours vs the reference on the same topologies)

Pending features, not bugs to hide (they fail on ours until the feature lands):

| Tests | Reason |
|---|---|
| `ClientTest::testCreateClientEncryption*` (3), `ClientTest::testConstructorAutoEncryptionOpts`, `ClientTest::testConstructorEmptyKmsProvider`, CSFLE spec/prose tests | client-side encryption is not built: this extension behaves as ext-mongodb built without libmongocrypt (see `DIVERGENCES.md`) |

Consequences of a listed divergence:

| Tests | Reason |
|---|---|
| `RetryableWrites\Prose3_ReturnOriginalErrorTest` (replica set; reported as risky: it asserts nothing when no exception is thrown) | its subscriber configures the second fail point from `commandSucceeded` of the first attempt; this extension delivers APM events after the operation ("APM delivery time" in `DIVERGENCES.md`), so the retry runs before the fail point exists and succeeds |

Environment-dependent on both builds (sharded cluster of tools/topo.sh: config server and two
single-node shards, two mongos, in Docker): change-stream tests (`UnifiedSpecTest::testChangeStreams`,
`Operation\WatchFunctionalTest`) time out waiting for events ("iterateUntilDocumentOrError timed
out", "Expected cursor to return an element but none was found after 10 attempts"). The reference
fails 48 of them, ours 50 (plus the 5 CSFLE errors); the sets differ from run to run on both
builds, with the same messages. Counts at the time of the 2.5.1 baseline:

| Topology | Reference 2.5.6 | This driver |
|---|---|---|
| standalone | 6973 tests, 0 errors, 0 failures, 1461 skipped, 604 incomplete | 5 errors (CSFLE) |
| replica set | 6973 tests, 0 errors, 0 failures, 890 skipped, 604 incomplete | 5 errors (CSFLE), 1 risky (Prose3, above) |
| sharded | 6973 tests, 48 failures (change streams), 909 skipped, 604 incomplete | 5 errors (CSFLE), 50 failures (change streams) |

## Rust driver and specification tests (crates/core, cargo test)

Ported (as each phase lands): BSON corpus, URI options, connection string, auth (connection
string part), server selection, max staleness, SDAM (single-threaded semantics), initial DNS
seedlist (needs the public test DNS records: `MONGO_CORE_DNS_TESTS=1`; seeds, options and errors are checked, not `hosts`/`numHosts`/`ping`, which need the specification's replica set on localhost:27017-27019, outside the ports these suites use), retryable reads/writes (through PHPLIB's unified
runner rather than in Rust), SCRAM, compression and wire framing unit tests.

Not applicable:

| Tests | Why |
|---|---|
| mongo-rust-driver tests of its public async API, tokio runtime, `Client`/`Collection` builders, sync wrapper | they test the Rust crate's API, which this extension does not have |
| SDAM monitoring with streaming hello / RTT monitor threads, CMAP background thread (minPoolSize maintenance, pool paused/ready events) | ext-mongodb uses libmongoc's single-threaded client (no monitoring threads, no CMAP pool); this extension follows it |
| CSOT (client-side operations timeout) spec tests | not supported by ext-mongodb 2.5.6 either |

## Local tests (tests/local) and deliberate divergences

Every behaviour that differs from ext-mongodb 2.5.6 (for safety, permanently for other reasons,
or temporarily until a feature lands) is listed in `DIVERGENCES.md` at the repository root, with
the local PHPT or cargo test that pins it.
