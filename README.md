# mongo-php-async-driver

A pure-Rust, drop-in replacement for the PHP `mongodb` extension (ext-mongodb 2.5.6), with one
addition: network waits give control to the [Revolt](https://revolt.run) event loop instead of
blocking, so driver calls inside PHP Fibers run concurrently.

- **Same API and behaviour as ext-mongodb 2.5.6:** every `MongoDB\BSON\*` and `MongoDB\Driver\*`
  class, constant, `var_dump()` output, serialization format, exception and message (the
  Reflection dump is identical). [PHPLIB](https://github.com/mongodb/mongo-php-library) and code
  written for ext-mongodb run unchanged. Every difference is listed, with its reason and the test
  that pins it, in [DIVERGENCES.md](DIVERGENCES.md); where mirroring ext-mongodb would be unsafe
  (a crash, an overflow, unbounded allocation, a leaked secret), safety wins.
- **No C libraries:** no libbson, libmongoc or libmongocrypt, no OpenSSL, zlib or zstd. TLS is
  rustls with the [graviola](https://github.com/ctz/graviola) crypto provider (Rust and assembly);
  compression is flate2/miniz_oxide, ruzstd and snap. The only C is the small `wrapper.c` of
  [ext-php-rs](https://github.com/extphprs/ext-php-rs), the Zend Engine bindings.
- **No threads:** all I/O happens on the PHP thread. Thread-safe (ZTS) PHP works too: each PHP
  thread has its own clients and event-loop state.

## Install

With [PIE](https://github.com/php/pie):

```bash
pie install danog/mongo-php-async-driver
```

It installs as the `mongodb` extension, so it conflicts with the official `mongodb/mongodb-extension`:
uninstall that first (`pie uninstall mongodb/mongodb-extension`, or remove ext-mongodb's package
or `extension=mongodb` line). Releases carry pre-built binaries for Linux (x86-64 and arm64,
glibc and musl), macOS (arm64 and x86-64) and Windows (x86-64), for PHP 8.2 to 8.6, NTS and ZTS.
Elsewhere, PIE builds from source, which needs Rust (`cargo`) and libclang.

## Build from source

PHP 8.2 to 8.6, NTS or ZTS (thread-safe: FrankenPHP, the Windows "TS" builds, ext-parallel),
non-debug, 64-bit: Linux and macOS (x86-64, arm64) and Windows (x86-64). bindgen needs libclang.

```bash
PHP=/usr/bin/php8.4 PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release
# load target/release/libmongodb.so (.dylib on macOS) with extension=... instead of ext-mongodb
```

On Windows, set `PHP` to php.exe (there is no php-config; ext-php-rs downloads the matching PHP
development pack) and use nightly Rust: ext-php-rs needs the vectorcall calling convention,
which only nightly has.

```bash
PHP='C:\tools\php\php.exe' cargo +nightly build --release
# target\release\mongodb.dll
```

## Performance

Compared with ext-mongodb 2.5.6 (a standard PECL build) on Linux x86-64: PHP 8.4 CLI pinned to
one core, a local MongoDB 8.0 server, release builds, alternating rounds (medians; an A/A run of
two identical builds differed by about 1%). CPU is the client's own CPU time per operation, the
cleanest measure of the driver; wall time includes the server's work.

| Operation (standalone server) | CPU, ext-mongodb → this driver | Wall time |
|---|---|---|
| `findOne` | 26.8 → 12.6 µs (−53%) | −17% |
| `insertOne` | 28.1 → 12.9 µs (−54%) | −18% |
| `ping` | 22.9 → 10.2 µs (−55%) | −32% |
| insert of 10 documents | 39.6 → 19.0 µs (−52%) | −18% |
| find, 100 documents | 88.9 → 60.2 µs (−32%) | −12% |
| find, 1000 documents in batches of 100 | 812 → 569 µs (−30%) | −16% |

Most of the CPU difference is system calls: a `ping` makes 2 (one send, one receive), against
about 9 for ext-mongodb. On a replica set the CPU savings are similar (−31% to −57%).

- **BSON** encoding and decoding: 36% less time on average (geometric mean over 48 cases, faster
  in 45); decoding large JSON documents (`big/fromJSON`) is 9% slower.
- **Connection setup** (new client and first command): −26% CPU over TCP, −72% with TLS
  (374 µs against 1336 µs).
- **Fibers** (`findOne`, 20,000 operations): sequential calls run at 15.6k operations/s
  (ext-mongodb: 13.1k); with 50–200 concurrent fibers, 40–50k operations/s, about 3.8× what
  ext-mongodb can do one call at a time. This was limited by the single client core.
- **Known weak spot:** in `{main}` with `Revolt\EventLoop` loaded, every network wait goes
  through the event loop, which adds about 17 µs of CPU per small operation; there this driver uses
  7–22% more CPU than ext-mongodb on small operations (still less on larger ones, and still
  faster in wall time). Without Revolt loaded, `{main}` calls are the plain synchronous path of
  the first table.

The scripts are in `tools/bench/` (`rtt.php`, `bson.php`, `handshake.php`, `fibers.php`,
`scan.php`).

## Fibers

Automatic, with no option: a driver call made inside a `Fiber` suspends that fiber while it waits
for the network, and other fibers run meanwhile. This needs `revolt/event-loop`
(`composer require revolt/event-loop`); without it, a call inside a fiber throws a
`RuntimeException`. The extension looks the class up by name and has no dependency on it.

- **`{main}`:** once `Revolt\EventLoop` is loaded, calls in `{main}` also wait through the loop,
  so timers, watchers and other fibers keep running. Before that, `{main}` calls block exactly as
  with ext-mongodb.
- **Concurrency:** concurrent operations of one `Manager` each get their own connections, at most
  `maxPoolSize` (default 100) at once; a fiber finding them all in use waits at most
  `serverSelectionTimeoutMS`. Server scans are shared: one runs at a time.
- **One fiber per object:** a `Cursor` or `Session` used while another fiber's operation holds it
  throws a `LogicException`.
- **No surprise switches:** destructors, the garbage collector and shutdown never suspend.

Under the hood, waits go through one epoll/kqueue poll set (via mio) that a single Revolt
`onReadable` watcher drives; on Windows each wait watches its sockets with Revolt directly.
Benchmark: `tools/bench/fibers.php`.

## fork()

There is no per-operation fork detection: a child process keeps using the connections and
cursors it inherited, so do not use them from both processes; create a new `Manager` in the
child. As with ext-mongodb, a live cursor or session destroyed in a child (for example when the
child exits) leaves the parent's cursor and transaction alone, and the child never reuses the
parent's pooled server sessions.

## Monitoring timestamps

APM and `LogSubscriber` callbacks run when the operation releases the client, not while it runs.
So that tracing tools can still time commands, each callback whose signature allows it gets one
extra trailing `int`: the value `hrtime(true)` had when the event or message was created.

```php
public function commandStarted(CommandStartedEvent $event): void
{
    $startedAt = func_get_args()[1] ?? hrtime(true); // The original C ext-mongodb passes no second argument
}
```

It is passed only to methods that declare exactly the interface's parameters, or that end in an
untyped, `mixed` or `int`-accepting by-value variadic. Every other signature is called exactly as
ext-mongodb calls it.

## Client-side encryption

`ClientEncryption` and `autoEncryption` (with `bypassAutoEncryption` or `bypassQueryAnalysis`)
are a pure-Rust port of libmongocrypt 1.21:
- every KMS provider (local, AWS, Azure, GCP, KMIP) and on-demand credentials;
- CSFLE;
- Queryable Encryption: equality, range (including Decimal128) and string search.

Not included: automatic encryption through query analysis (mongocryptd or crypt_shared, from
MongoDB Enterprise).

**Trust model (as in ext-mongodb):** a data key document names its KMS endpoint, and the
provider's credentials (an AWS request signature, an Azure or GCP access token) are sent to that
endpoint. Whoever can write to the key vault collection can direct those requests to a host of
their choosing, so protect the key vault collection as you would the keys themselves.

## Tests

- `tests/phpt`: ext-mongodb's PHPT suite (2.5.6). Run it with `tools/phpt.sh` and compare it with
  ext-mongodb's results using `tools/phpt-diff.sh`.
- `tests/phplib`: PHPLIB's test suite (2.5.1).
- `tests/local`: this driver's own tests, mostly for the entries of DIVERGENCES.md.
- `tests/spec`: the MongoDB specifications, as a submodule (`git submodule update --init`).
- `cargo test`: the Rust crates' unit and integration tests.

`tools/topo.sh` starts the standalone, replica-set and sharded servers the suites use.
`tests/UPSTREAM.md` records the upstream versions. `tests/NOT_APPLICABLE.md` lists the tests that
cannot run here, with the reason.

**CI** (`.github/workflows/tests.yml`) runs on Linux, macOS and Windows, on every push and daily.
It tests against the official suites at their latest commits rather than the copies above: the
default branches of mongodb/mongo-php-driver (PHPT) and mongodb/mongo-php-library (PHPUnit) are
cloned fresh each run (the commits are listed in the job summary), against standalone,
replica-set and sharded servers. Upstream tests that fail by design are listed in
`tools/ci/expected-failures.txt` with the reason (a DIVERGENCES.md entry, or a failure
ext-mongodb has too). Any other failure fails the run.

## License

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE). The exception is the Unicode folding
code used by Queryable Encryption string search (`crates/crypt/src/unicode/`), which is ported
from libmongocrypt under the Server Side Public License v1 (see [LICENSE-SSPL](LICENSE-SSPL)). It
sits behind the default `sspl-text-fold` cargo feature: build with `--no-default-features` for an
Apache-2.0-only extension (string search then requires `caseSensitive` and `diacriticSensitive`
set to true).

Written by Daniil Gentili ([@danog](https://github.com/danog)).
