# mongo-php-async-driver

A MongoDB driver for PHP with native Fiber support, written in Rust: a drop-in replacement for
the `mongodb` extension (ext-mongodb 2.5.6) whose network waits give control to the
[Revolt](https://revolt.run) event loop instead of blocking: code written for ext-mongodb runs
unchanged, and driver calls made from concurrent PHP Fibers run at the same time.

- **Fibers, with no code changes:** with `revolt/event-loop` installed, a driver call inside a
  `Fiber` suspends only that fiber while it waits for the network, so other fibers and Revolt's
  timers and watchers keep running. Each concurrent operation gets its own pooled connection
  (`maxPoolSize`): 50 to 200 concurrent fibers reach 43–56k operations/s on one core, 3.3–4.3×
  what ext-mongodb does one call at a time. Anything built on Revolt (Amp v3, for one) works.
  Calls in `{main}` wait through the loop too once Revolt is loaded, unless
  `mongodb.main_event_loop=0`. See [Fibers](#fibers).
- **Same API and behaviour as ext-mongodb 2.5.6:** every `MongoDB\BSON\*` and `MongoDB\Driver\*`
  class, constant, `var_dump()` output, serialization format, exception and message (the
  Reflection dump is identical). [PHPLIB](https://github.com/mongodb/mongo-php-library) and code
  written for ext-mongodb run unchanged. Every difference is listed, with its reason and the test
  that pins it, in [DIVERGENCES.md](DIVERGENCES.md); where mirroring ext-mongodb would be unsafe
  (a crash, an overflow, unbounded allocation, a leaked secret), safety wins.
- **Faster than ext-mongodb:** about half the CPU time per operation (`findOne` 12.1 µs against
  27.1 µs, `insertOne` 12.3 against 28.7), 59% less time encoding and decoding BSON, 72% less CPU
  to set up a TLS connection; with concurrent Fibers, 3.3–4.3× the throughput ext-mongodb reaches
  one call at a time. See [Performance](#performance).
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
glibc and musl), macOS (arm64 and x86-64) and Windows (x86-64), for PHP 8.2 to 8.6, NTS and ZTS
(until PHP 8.6 is released: ZTS only on Linux, none on Windows). Elsewhere, PIE builds from source, which needs Rust (`cargo`) and libclang.

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

Compared with ext-mongodb 2.5.6 (a standard PECL build), measured on 2026-11-10 at 6b904a5 on
Linux x86-64: PHP 8.4 CLI pinned to one core, a local MongoDB 8.0 server, release builds,
alternating rounds, medians. Two identical builds of this driver (an A/A control) differed by
0.6% or less on average; single cases differed by up to 3% (BSON) and 5% (concurrent fibers).
CPU is the client's own CPU time per operation, the cleanest measure of the driver; wall time
includes the server's work.

| Operation (standalone server) | CPU, ext-mongodb → this driver | Wall time |
|---|---|---|
| `findOne` | 27.1 → 12.1 µs (−55%) | −18% |
| `insertOne` | 28.7 → 12.3 µs (−57%) | −19% |
| `ping` | 23.0 → 10.0 µs (−56%) | −32% |
| insert of 10 documents | 39.9 → 17.2 µs (−57%) | −21% |
| find, 100 documents | 88.9 → 55.9 µs (−37%) | −14% |
| find, 1000 documents in batches of 100 | 810 → 518 µs (−36%) | −19% |

- **Replica set** (3 members): CPU −37% to −59% on the same operations; wall time −14% to −35% for
  reads and `ping`, equal for writes (both wait about 5 ms for the majority).
- **BSON** encoding and decoding: 59% less time on average (geometric mean over 48 cases); faster in
  46, equal in 2 (`text/fromBSON` −0.3%, `Int64` addition +2.0%). `fromPHP` takes 36% to 96% less
  time, `toPHP` 15% to 96% less, Extended JSON output 54% to 89% less. Parsing JSON
  (`fromJSON`, 1,000 small documents): 381 µs against 674 µs for ext-mongodb with relaxed
  Extended JSON, 641 µs against 973 µs with canonical.
- **Connection setup** (new client and first command): CPU −26% over TCP (162 µs against 220 µs),
  −72% with TLS (375 µs against 1324 µs); wall time −16% and −39%.
- **Fibers** (`findOne`, 20,000 operations): sequential calls in `{main}` run at 15.9k operations/s
  (ext-mongodb: 12.9k). With 50 or 200 concurrent fibers, 43–44k operations/s with
  `maxPoolSize=4` and 54–56k with 32: 3.3–4.3× what ext-mongodb does one call at a time. The single
  client core is the limit.
- **`{main}` with `Revolt\EventLoop` loaded:** every network wait then goes through the event loop.
  This adds 8 to 12 µs of CPU per small operation: `findOne` takes 23.8 µs with Revolt's default
  `StreamSelectDriver`, 20.4 µs with `EvDriver` and 20.8 µs with `UvDriver`, against 11.8 µs
  without the loop and 26.8 µs for ext-mongodb (which never uses the loop). `ping` and
  `insertOne` behave the same (`ping`: 21.2 / 17.8 / 18.1 µs, against 9.8 µs and 22.6 µs).
  With `mongodb.main_event_loop=0`, `{main}` calls take the plain synchronous path of the table
  above (`findOne` 11.9 µs) while fibers keep using the loop. Even through the loop, this driver
  uses less CPU than ext-mongodb: 7% to 16% less with `StreamSelectDriver`, 21% to 27% less with
  `EvDriver`. It is also faster in wall time (`findOne` 70 µs against 77 µs).

**Why it is faster.** Per `findOne`, this driver makes 2 system calls (one send, one receive);
ext-mongodb makes 8 (3 receives, 3 `getpid`, a `poll` and a send). An insert of 10 documents
takes 2 against 17, a find of 1000 documents 22 against 58. In user space, a `findOne` costs
26,600 instructions and 21,000 cycles against 62,800 and 73,300; in the kernel, 42,400 cycles
against 66,000, which is now two thirds of this driver's CPU time. Of ext-mongodb's user-space
instructions per `findOne`, 21,900 run in libbson (9,900 of them stepping from one element to the
next), 6,400 in `malloc`/`free` and 2,200 in 42 case-insensitive string comparisons. Selecting
the server costs it 4,400 instructions, plus 3,200 to copy and free two server descriptions. This
driver spends 5,400 in BSON code, 4,100 in `malloc`/`free`, 300 on the whole server selection and
none on string comparisons.

The scripts are in `tools/bench/` (`rtt.php`, `bson.php`, `fromjson.php`, `handshake.php`,
`fibers.php`, `main.php`, `scan.php`).

## Fibers

```php
use MongoDB\Driver\{Manager, Query};
use Revolt\EventLoop;

$manager = new Manager('mongodb://127.0.0.1');
foreach (['alice', 'bob', 'carol'] as $name) {
    // three queries in flight at once, on three pooled connections
    EventLoop::queue(function () use ($manager, $name) {
        $users = $manager->executeQuery('app.users', new Query(['name' => $name]))->toArray();
        echo $name, ': ', count($users), "\n";
    });
}
EventLoop::run();
```

Automatic, with no option: a driver call made inside a `Fiber` suspends that fiber while it waits
for the network, and other fibers run meanwhile. This needs `revolt/event-loop`
(`composer require revolt/event-loop`); without it, a call inside a fiber throws a
`RuntimeException`. The extension looks the class up by name and has no dependency on it.

- **`{main}`:** once `Revolt\EventLoop` is loaded, calls in `{main}` also wait through the loop,
  so timers, watchers and other fibers keep running. Before that, `{main}` calls block exactly as
  with ext-mongodb. Set `mongodb.main_event_loop=0` (php.ini, or `ini_set()` at run time) to
  keep `{main}` calls blocking even with Revolt loaded: they then take the plain synchronous path
  (see Performance). Calls inside fibers always go through the loop.
- **Concurrency:** concurrent operations of one `Manager` each get their own connections, at most
  `maxPoolSize` (default 100) at once; a fiber finding them all in use waits at most
  `serverSelectionTimeoutMS`. Server scans are shared: one runs at a time.
- **One fiber per object:** a `Cursor` or `Session` used while another fiber's operation holds it
  throws a `LogicException`.
- **No surprise switches:** destructors, the garbage collector and shutdown never suspend.

Under the hood, waits go through one epoll/kqueue instance per thread that a single Revolt
`onReadable` watcher drives; each socket is registered there once, edge-triggered, so the driver
needs no system call to set up a wait for a reply. On Windows each wait watches its sockets
with Revolt directly. The watcher lives on Revolt's current driver: after
`EventLoop::setDriver()`, the next wait moves it to the new one, while waits already pending on
the old driver (fibers suspended there) stay on it and keep only its loop alive. So that a `{main}` wait does not
make Revolt create a new suspension each time, `{main}`'s suspension is kept by a disabled callback
on the driver (one more id in `getIdentifiers()`; it never runs nor keeps the loop alive).
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
