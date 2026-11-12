# mongo-php-async-driver

A drop-in replacement for the `mongodb` PHP extension (ext-mongodb 2.5.6), written in pure Rust,
whose network waits are async: a driver call inside a `Fiber` suspends only that fiber, through
the [Revolt](https://revolt.run) event loop built into the extension. Code written for
ext-mongodb, [PHPLIB](https://github.com/mongodb/mongo-php-library) included, runs unchanged.

- **Same API and behaviour as ext-mongodb 2.5.6:** every `MongoDB\BSON\*` and `MongoDB\Driver\*`
  class, constant, `var_dump()` output, serialization format, exception and message. Every
  difference is listed, with its reason and the test that pins it, in
  [DIVERGENCES.md](DIVERGENCES.md); where mirroring ext-mongodb would be unsafe, safety wins.
- **Async, with no code changes:** concurrent driver calls in fibers run at the same time, each
  on its own pooled connection; Amp v3 and anything else built on Revolt work unchanged.
- **Faster:** about half the CPU time of ext-mongodb per operation, and 3.3–4.3× its throughput
  with concurrent fibers (see [Performance](#performance)).
- **No C libraries:** no libbson, libmongoc, libmongocrypt, OpenSSL, zlib or zstd (TLS is rustls
  with the [graviola](https://github.com/ctz/graviola) crypto provider). No threads: all I/O runs
  on the PHP thread.

## Install

With [PIE](https://github.com/php/pie):

```bash
pie install danog/mongo-php-async-driver
```

It installs as the `mongodb` extension, so uninstall the official one first
(`pie uninstall mongodb/mongodb-extension`, or remove ext-mongodb's package or its
`extension=mongodb` line). Releases carry pre-built binaries; elsewhere PIE builds from source,
which needs Rust (`cargo`) and libclang.

## Compatibility

- **PHP** 8.2 to 8.6, NTS and ZTS (FrankenPHP, the Windows "TS" builds, ext-parallel), non-debug,
  64-bit. Pre-built binaries: Linux (x86-64 and arm64, glibc and musl), macOS (arm64 and x86-64)
  and Windows (x86-64); until PHP 8.6 is released, 8.6 has ZTS builds only on Linux and none on
  Windows.
- **MongoDB** 4.2 and later (as ext-mongodb 2.5.6): standalone, replica sets, sharded clusters.
- **Client-side encryption** in pure Rust: `ClientEncryption`, every KMS provider, CSFLE and
  Queryable Encryption, and `autoEncryption` with `bypassAutoEncryption` or `bypassQueryAnalysis`.
  Not included: automatic encryption through query analysis (mongocryptd or crypt_shared, from
  MongoDB Enterprise). As with ext-mongodb, whoever can write to the key vault collection can send
  the KMS credentials to a host of their choosing: protect it as you would the keys themselves.

## Async

```php
use MongoDB\Driver\{Manager, Query};
use function Amp\async;
use function Amp\Future\await;

$manager = new Manager('mongodb://127.0.0.1');

$futures = [];
foreach (['alice', 'bob', 'carol'] as $name) {
    // three queries in flight at once, on three pooled connections
    $futures[$name] = async(fn () => $manager->executeQuery('app.users', new Query(['name' => $name]))->toArray());
}
$users = await($futures); // ['alice' => [...], 'bob' => [...], 'carol' => [...]]
```

The example uses [Amp](https://amphp.org) (`composer require amphp/amp`); any other library built
on Revolt, or Revolt on its own, works the same way. Async is automatic: a driver call inside a
fiber waits through the event loop, and other fibers run meanwhile.

The Revolt event loop is built in ([revolt-rs](https://github.com/danog/revolt-rs), Revolt
implemented natively in Rust): the extension registers the `Revolt\EventLoop` classes itself, so
revolt/event-loop need not be installed (if it is, it stays unused). This package's
`composer.json` declares `"provide": {"ext-revolt": "*", "revolt/event-loop": "1.0.9"}`, so
projects that require danog/mongo-php-async-driver through Composer do not get
revolt/event-loop installed.

## Performance

Client CPU time per operation against ext-mongodb 2.5.6 (Linux x86-64, PHP 8.4, local MongoDB 8.0):

| Operation | ext-mongodb | this driver |
|---|---|---|
| `findOne` | 27.1 µs | 12.1 µs (−55%) |
| `insertOne` | 28.7 µs | 12.3 µs (−57%) |
| find, 1000 documents | 810 µs | 518 µs (−36%) |
| BSON encoding and decoding (48 cases) | | −59% on average |
| `findOne` in 50–200 concurrent fibers | 12.9k ops/s (one call at a time) | 43–56k ops/s |

[docs/PERFORMANCE.md](docs/PERFORMANCE.md) has the method, the full results and the event-loop
measurements.

## Build from source

```bash
PHP=/usr/bin/php8.4 PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release
# load target/release/libmongodb.so (.dylib on macOS) with extension=... instead of ext-mongodb
```

bindgen needs libclang. On Windows, set `PHP` to php.exe (ext-php-rs downloads the matching
development pack) and build with nightly Rust (`cargo +nightly build --release`, for the
vectorcall calling convention): the result is `target\release\mongodb.dll`.

## Tests

`tests/phpt` (ext-mongodb's PHPT suite, `tools/phpt.sh`), `tests/phplib` (PHPLIB's suite,
`tools/phplib.sh`), `tests/local` (this driver's own, mostly for DIVERGENCES.md) and
`cargo test`; `tools/topo.sh` starts the servers. CI (`.github/workflows/tests.yml`) runs the
official suites at their latest commits on Linux, macOS and Windows against standalone,
replica-set and sharded servers; the upstream tests that fail by design are listed in
`tools/ci/expected-failures.txt`.

## License

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE). The exception is the Unicode folding
code of Queryable Encryption string search (`crates/crypt/src/unicode/`), ported from
libmongocrypt under the Server Side Public License v1 ([LICENSE-SSPL](LICENSE-SSPL)), behind the
default `sspl-text-fold` cargo feature: build with `--no-default-features` for an
Apache-2.0-only extension (string search then requires `caseSensitive` and `diacriticSensitive`
set to true). revolt-rs, compiled in, is under the MIT License (notice in [NOTICE](NOTICE)).

Written by Daniil Gentili ([@danog](https://github.com/danog)).
