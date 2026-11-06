# mongo-php-async-driver

A pure-Rust, drop-in replacement for the PHP `mongodb` extension (ext-mongodb), with one
addition: inside a PHP Fiber, with a [Revolt](https://revolt.run) event loop available, I/O
suspends the fiber instead of blocking. In `{main}` it blocks, as ext-mongodb does.

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
  assembly, no C).
- **I/O on the PHP thread,** through one poll set (epoll/kqueue via mio) that the extension's own
  Revolt watcher drives; connections are released as soon as the server has answered, and
  waiters queue in FIFO order, so lazily-resumed fibers cannot deadlock the pool.

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

## Build

```bash
# PHP: the php binary of the same version (ext-php-rs reads the version from it); bindgen needs libclang
PHP=/usr/bin/php8.4 PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release
# target/release/libmongodb.so: load it with extension=/path/to/libmongodb.so instead of ext-mongodb
```

PHP 8.4 and 8.5, NTS builds, on Linux and other Unix systems.

The previous reads-only client (`danog\Mongo\Client`), which used ext-mongodb's BSON codec, is in
`legacy/zm2` and is being turned into the new engine.

Written by Daniil Gentili ([@danog](https://github.com/danog)).

## License

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE).
