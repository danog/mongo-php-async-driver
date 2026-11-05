# Async MongoDB PHP extension

A small MongoDB wire client for PHP, written in Rust, that runs reads either blocking or by suspending the current fiber through the [Revolt](https://revolt.run) event loop.

It runs on the PHP thread itself, with no threads and no tokio. BSON is encoded and decoded by ext-mongodb's own encoder and decoder, so results are identical to an ext-mongodb `Cursor` with an array type map.

An earlier design based on the official Rust `mongodb` crate and tokio cost about 65 µs of CPU per operation in thread hand-offs and crate overhead, which made it slower than ext-mongodb for single queries; this one avoids both.

## Status

- **Reads only.** Every command is checked against an allowlist before anything is sent: `find`, `aggregate` (without `$out`/`$merge`), `count`, `distinct`, and `getMore` for their cursors. Writes stay on ext-mongodb.
- **Not supported**, and rejected when the URI is parsed: auth, TLS, `mongodb+srv://`, `maxStalenessSeconds`, load balancers. There are no sessions, no `lsid` and no `$clusterTime`.
- **Linux only.** It uses epoll and timerfd.
- **Requirements:** PHP 8.4 or 8.5, and ext-mongodb 2.1.x or 2.5.x loaded before it. Other ext-mongodb versions are refused at the first call, because the decoder state layout has to be checked for each version.

## Features

- **Discovery:** replica-set discovery with read preference modes, tag sets and the `localThresholdMS` latency window.
- **Scans:** topology scans at first use, then every `heartbeatFrequencyMS` (default 60 s), and after a server error at most every 500 ms.
- **Reads:** cursors are drained with `getMore`, and reads are retried once.
- **Compression:** OP_COMPRESSED with zstd (statically linked) and zlib, when the URI's `compressors` lists them and the server agrees. snappy is skipped.
- **Connection pools,** per server and per process: they stay warm across FPM requests, are fork-safe, and keep at most `maxPoolSize` connections (default 100).
  - Asynchronous operations are driven by the event loop rather than by their fiber: it sends, receives and runs `getMore` until the cursor is exhausted, and then hands the connection to the next queued operation. A connection is free as soon as the server has answered, even if the fiber that asked has not run yet. Schedulers that resume fibers lazily therefore cannot deadlock on the pool.
  - When the pool is exhausted, asynchronous operations queue in FIFO order, and blocking callers poll the event set until a connection frees.
- **Extra safety options:**
  - URI option `strictSecondary=true`: `secondaryPreferred`, `nearest` and `primaryPreferred` select secondaries only, and fail rather than fall back to the primary.
  - Environment variable `MONGO_ASYNC_REQUIRE_TAG=key:value`: only secondaries with that tag are ever selected, whatever the mode, tag sets or topology type.

## Performance

Measured on one Linux host against a local replica set, PHP 8.4, ext-mongodb 2.1.4.

| | this extension | ext-mongodb |
|---|---|---|
| `findOne` latency | 76 µs | 106 µs |
| CPU per `findOne` | 32 µs | 54 µs |
| Mixed reads, 50 concurrent fibers | 5.9–6.0k ops/s | 3.6k ops/s (sequential) |

## Build

```bash
cargo build --release
# target/release/libmongo_php_async_driver.so
```

Building needs the PHP development headers and a Rust toolchain. With several PHP versions installed, select the target one explicitly, for example `PHP=/usr/bin/php8.4 PHP_CONFIG=/usr/bin/php-config8.4 cargo build --release`.

## Usage

```ini
extension=mongodb
extension=/path/to/libmongo_php_async_driver.so
```

```php
<?php

use danog\Mongo\Client;
use Revolt\EventLoop;

// Once per request: the extension's event set, watched by Revolt.
$stream = Client::init();
stream_set_blocking($stream, false);
$watcher = EventLoop::onReadable($stream, static fn () => Client::wakeup());

// Clients are cached per process by URI (and pool size), so this is cheap to call per request.
$client = new Client('mongodb://db1,db2,db3/?replicaSet=rs0&readPreference=secondaryPreferred&compressors=zstd', null);

// Inside a fiber this suspends the fiber; in {main} it blocks. Pass the last argument to force either.
$docs = $client->find('mydb', ['find' => 'items', 'filter' => ['active' => true], 'limit' => 10], null, null);
$reply = $client->command('mydb', ['count' => 'items', 'query' => ['active' => true]], null, null);

// While no operation is pending, unreference the watcher so EventLoop::run() can return.
if (Client::pending() === 0) {
    EventLoop::unreference($watcher);
}
```

### API

- **`new Client(string $uri, ?int $maxPoolSize)`:** returns the cached topology for that URI.
- **`find(string $db, array|object|string $cmd, ?string $readPreference, ?bool $async): array`:** runs a cursor command (`find`, `aggregate`) and drains it into a list of PHP arrays. `$cmd` may also be a raw BSON string.
- **`command(string $db, array|object|string $cmd, ?string $readPreference, ?bool $async): array`:** runs a command and returns its reply as a PHP array. Server errors throw `MongoDB\Driver\Exception\CommandException`.
- **`Client::init()`:** returns the stream to watch with `EventLoop::onReadable()`, which should call `Client::wakeup()`.
- **`Client::pending(): int`:** the number of in-flight asynchronous operations.
- **`Client::stats(bool $reset): array`:** pool counters: checkouts, wait time, connections created and closed, queueing, rejected commands.
- **`Client::wireStats(bool $reset): array`:** wire counters: `cmd:<name>`, `rejected:<name>`, `checkout@<addr>`, `created@<addr>`, `queued@<addr>`, `zip:<compressor>`, `unzip:<compressor>`.

`$async = null` means asynchronous inside a fiber and blocking in `{main}`.

Written by Daniil Gentili ([@danog](https://github.com/danog)).

## License

AGPL-3.0, see [LICENSE](LICENSE).
