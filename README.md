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
- **No C at all:** no libbson, libmongoc or libmongocrypt, no C dependencies, no `cc` builds. The
  Zend Engine bindings are generated with bindgen and checked in (`src/zend/sys/`); `build.rs`
  only asks `php-config` which ones to use.
- **I/O on the PHP thread,** through one poll set (epoll/kqueue via mio) that the extension's own
  Revolt watcher drives; connections are released as soon as the server has answered, and
  waiters queue in FIFO order, so lazily-resumed fibers cannot deadlock the pool.

## Tests

The upstream suites are carried over and run against both this extension and ext-mongodb:

- `tests/phpt`: ext-mongodb's PHPT suite (2.5.6), run with `tools/phpt.sh`, compared with
  `tools/phpt-diff.sh`;
- `tests/phplib`: PHPLIB's tests (2.5.0);
- `tests/spec`: the MongoDB specifications, as a submodule (`git submodule update --init`).

`tools/topo.sh` starts the standalone, replica-set and sharded topologies the suites use.
`tests/UPSTREAM.md` records the upstream versions; `tests/NOT_APPLICABLE.md` lists the tests that
cannot run here or are expected to fail, with the reason.

## Build

```bash
PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release
# target/release/libmongodb.so: load it with extension=/path/to/libmongodb.so instead of ext-mongodb
```

PHP 8.4 and 8.5, NTS builds, on Linux and other Unix systems.

The previous reads-only client (`danog\Mongo\Client`), which used ext-mongodb's BSON codec, is in
`legacy/zm2` and is being turned into the new engine.

Written by Daniil Gentili ([@danog](https://github.com/danog)).

## License

Apache-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE).
