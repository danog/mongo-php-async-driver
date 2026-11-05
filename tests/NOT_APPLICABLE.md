# Tests that do not apply, cannot run here, or are expected to fail

Nothing is dropped silently: every carried-over test either runs, or is listed here with the
reason. Counts are kept up to date in the README once each phase lands.

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
| `ocsp-success.phpt`, `ocsp-failure.phpt` | an OCSP responder and certificates from drivers-evergreen-tools |
| `clientEncryption/*` using AWS, Azure, GCP or KMIP KMS providers | KMS credentials / a KMIP server; the local KMS provider runs |
| `connect/*-auth-*`, `standalone-x509-*`, `standalone-ssl-*` | a mongod with auth / TLS / X.509 set up (to be added to tools/topo.sh in phase 5) |
| `frankenphp/` | not tests: a FrankenPHP worker-mode restart script (ZTS); this extension is NTS-only |

### Observe libmongoc internals (to be emulated, or documented as differences)

| Tests | Why |
|---|---|
| `logging/*` and others reading `mongodb.debug` output (30 files mention it) | they match libmongoc's trace/log lines. Messages the PHP API exposes (log levels, domains, `Monitoring\LogSubscriber`) are emulated in phase 6; raw libmongoc trace lines are not reproduced, and the tests that match them will be listed here one by one |
| phpinfo() rows `libmongoc SSL library`, `libmongoc crypto library`, `libmongocrypt crypto library` | report `rustls` / `RustCrypto` instead of `OpenSSL` / `libcrypto` (no test checks them) |

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

## Rust driver and specification tests (crates/core, cargo test)

Ported (as each phase lands): BSON corpus, URI options, connection string, auth (connection
string part), server selection, max staleness, SDAM (single-threaded semantics), initial DNS
seedlist (needs the public test DNS records), retryable reads/writes (through PHPLIB's unified
runner rather than in Rust), SCRAM, compression and wire framing unit tests.

Not applicable:

| Tests | Why |
|---|---|
| mongo-rust-driver tests of its public async API, tokio runtime, `Client`/`Collection` builders, sync wrapper | they test the Rust crate's API, which this extension does not have |
| SDAM monitoring with streaming hello / RTT monitor threads, CMAP background thread (minPoolSize maintenance, pool paused/ready events) | ext-mongodb uses libmongoc's single-threaded client (no monitoring threads, no CMAP pool); this extension follows it |
| CSOT (client-side operations timeout) spec tests | not supported by ext-mongodb 2.5.6 either |
