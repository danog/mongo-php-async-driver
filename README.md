# mongo-php-async-driver

A pure-Rust, drop-in replacement for the PHP `mongodb` extension (ext-mongodb), with one
addition: network waits give control to the [Revolt](https://revolt.run) event loop instead of
blocking. Inside a PHP Fiber they suspend the fiber; in `{main}`, when `Revolt\EventLoop` is
loaded, they suspend `{main}` (the loop runs meanwhile). Without Revolt loaded, a `{main}` call
blocks exactly as ext-mongodb does.

**Status: work in progress, not usable yet.** The extension loads as `mongodb`, reports
ext-mongodb 2.5.6's version, INI settings and phpinfo(), and registers ext-mongodb 2.5.6's whole
API surface (its Reflection dump is identical), but the methods are being ported: those not
ported yet throw an `Error`.

## Goals

- **Same PHP API and behaviour as ext-mongodb 2.5.6:** every `MongoDB\BSON\*` and
  `MongoDB\Driver\*` class, the same constants, `var_dump()`, serialization, exceptions and
  messages, so that [PHPLIB](https://github.com/mongodb/mongo-php-library) and ext-mongodb user
  code run unchanged.
- **No C libraries:** no libbson, libmongoc or libmongocrypt, no OpenSSL, ring or aws-lc, no C
  zstd. The Zend Engine layer is [ext-php-rs](https://github.com/extphprs/ext-php-rs) (its
  bindgen-generated bindings and its small `wrapper.c` are the only C involved). TLS is rustls
  with the [graviola](https://github.com/ctz/graviola) crypto provider (Rust with inline
  assembly, no C). Wire compression is pure Rust too: zlib through flate2 on miniz_oxide,
  zstd through ruzstd, snappy through snap.
- **I/O on the PHP thread, no threads:** synchronous calls make the same blocking socket calls
  as ext-mongodb. Inside a fiber, a wait registers the socket in one poll set (epoll/kqueue via
  mio) whose fd one Revolt `onReadable` watcher drives, and suspends the fiber. On Windows
  (no such handle for IOCP) each wait watches its sockets with Revolt `onReadable` /
  `onWritable` directly.

## Fibers

Automatic, no option: any driver call made inside a `Fiber` suspends that fiber while it waits
for the network, so other fibers run. It needs `revolt/event-loop` (`composer require
revolt/event-loop`; the class is looked up by name, the extension has no dependency on it):
without it, a call inside a fiber throws a `RuntimeException`. Once the class `Revolt\EventLoop`
is loaded, calls in `{main}` wait through the loop as well (`{main}` is suspended and the loop
runs other fibers, timers and watchers until the I/O is ready); before that, `{main}` calls block
as with ext-mongodb. Concurrent operations of one
Manager each use their own connections, at most `maxPoolSize` (default 100) at once; a fiber
finding them all in use waits at most `serverSelectionTimeoutMS`. They connect only to the server
they selected, and server scans are shared: one runs at a time, other fibers wait for it. A `Cursor` or `Session` is used
by one fiber at a time: using one while another fiber's operation holds it throws a
`LogicException`. Object destructors, the GC and
shutdown never suspend. Details and every difference from ext-mongodb: `DIVERGENCES.md`.
Benchmark: `tools/bench/fibers.php`.

## fork()

There is no fork detection (no `getpid()` per operation): a child process keeps using the
connections, cursors and sessions it inherited. Do not use them from both processes; create a new
`Manager` in the child (`DIVERGENCES.md`, "Use after `fork()`").

## Monitoring timestamps

APM and `LogSubscriber` callbacks run when the operation releases the client, not while it runs
(see "APM delivery time" in `DIVERGENCES.md`). Each callback whose signature allows it receives
one extra trailing `int`: the value `hrtime(true)` had when the event or log message was created.
Tracing integrations can use it instead of their own `hrtime(true)` in the callback:

```php
public function commandStarted(CommandStartedEvent $event): void
{
    $startedAt = func_get_args()[1] ?? hrtime(true); // ext-mongodb passes no second argument
}
```

It is passed to methods that declare exactly the interface's parameters, or end in an untyped,
`mixed`, `int` (or union with `int`) by-value variadic; any other signature is called exactly as
ext-mongodb calls it.

## Client-side encryption

`ClientEncryption` and `autoEncryption` (with `bypassAutoEncryption` or `bypassQueryAnalysis`) are a
pure-Rust port of libmongocrypt 1.21: every KMS provider (local, AWS, Azure, GCP, KMIP, on-demand
credentials), CSFLE, and Queryable Encryption (equality, range including Decimal128, string
search). Automatic encryption through query analysis (mongocryptd or crypt_shared, MongoDB
Enterprise) is not included. Differences from ext-mongodb are listed in DIVERGENCES.md.

Trust model (as in ext-mongodb): a data key document names its KMS endpoint, and the configured
credentials for that provider (an AWS request signature, an Azure or GCP access token) are sent to
that endpoint. Whoever can write to the key vault collection can therefore direct those requests to a
host of their choosing; protect the key vault collection as you would the keys themselves.

## Tests

The upstream suites are carried over and run against both this extension and ext-mongodb:

- `tests/phpt`: ext-mongodb's PHPT suite (2.5.6), run with `tools/phpt.sh`, compared with
  `tools/phpt-diff.sh`;
- `tests/phplib`: PHPLIB's tests (2.5.1);
- `tests/spec`: the MongoDB specifications, as a submodule (`git submodule update --init`).

`tools/topo.sh` starts the standalone, replica-set and sharded topologies the suites use.
`tests/UPSTREAM.md` records the upstream versions; `tests/NOT_APPLICABLE.md` lists the tests that
cannot run here or are expected to fail, with the reason.
`DIVERGENCES.md` lists every behaviour that differs from ext-mongodb 2.5.6 (safety wins over
parity), with the test that pins each one.

CI (`.github/workflows/tests.yml`) runs on Linux, macOS and Windows, on every push and daily,
against the official suites at their latest commits rather than the copies above: the default
branches of mongodb/mongo-php-driver (PHPT) and mongodb/mongo-php-library (PHPUnit) are cloned
fresh each run (the commits are listed in the job summary), on standalone, replica-set and
sharded servers. Upstream tests that fail by design are listed in `tools/ci/expected-failures.txt`,
each tied to a `DIVERGENCES.md` row; any other failure fails the run.

## Build

```bash
# PHP: the php binary of the same version (ext-php-rs reads the version from it); bindgen needs libclang
PHP=/usr/bin/php8.4 PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release
# target/release/libmongodb.so: load it with extension=/path/to/libmongodb.so instead of ext-mongodb
```

PHP 8.4 and 8.5, NTS builds, on Linux, macOS and other Unix systems (x86-64, arm64) and on
Windows (x86-64). On Windows there is no php-config: set `PHP` to php.exe (ext-php-rs downloads
the matching PHP development pack), and use a nightly Rust (ext-php-rs needs the vectorcall
calling convention of PHP's functions there, which only nightly Rust has):

```bash
PHP='C:\tools\php\php.exe' cargo +nightly build --release
# target\release\mongodb.dll
```

The previous reads-only client (`danog\Mongo\Client`), which used ext-mongodb's BSON codec, is in
`legacy/zm2` and is being turned into the new engine.

Written by Daniil Gentili ([@danog](https://github.com/danog)).

## License

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE), except the Unicode folding code of Queryable
Encryption string search in `crates/crypt/src/unicode/`, which is ported from libmongocrypt under the
Server Side Public License v1 (see [LICENSE-SSPL](LICENSE-SSPL)). That code is behind the default
`sspl-text-fold` cargo feature: build with `--no-default-features` for an Apache-2.0-only extension
(string search then requires `caseSensitive` and `diacriticSensitive` set to true).
