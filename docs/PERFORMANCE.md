# Performance

The measurements behind the summary in the [README](../README.md#performance), with their method.

## Against ext-mongodb

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
- **Async** (`findOne` in Fibers, 20,000 operations): sequential calls in `{main}` run at 15.9k operations/s
  (ext-mongodb: 12.9k). With 50 or 200 concurrent fibers, 43–44k operations/s with
  `maxPoolSize=4` and 54–56k with 32: 3.3–4.3× what ext-mongodb does one call at a time. The single
  client core is the limit.
- **`{main}` through the event loop:** once the loop is in use and has something else to do (a
  timer, a watcher, a fiber waiting), every network wait of a `{main}` call goes through it (see
  [Event loops](#event-loops) below). With the built-in loop this adds 0.5–0.6 µs of CPU per small
  operation on macOS and 0.8–1.2 µs on Linux: one `kevent()` / `epoll_wait()` on the loop's own
  poller, where the driver's sockets are registered, before the receive. On an idle loop, or before the
  loop is used, `{main}` calls take the plain synchronous path of the table above (fibers keep
  using the loop).

### Why it is faster

Per `findOne`, this driver makes 2 system calls (one send, one receive);
ext-mongodb makes 8 (3 receives, 3 `getpid`, a `poll` and a send). An insert of 10 documents
takes 2 against 17, a find of 1000 documents 22 against 58. In user space, a `findOne` costs
26,600 instructions and 21,000 cycles against 62,800 and 73,300; in the kernel, 42,400 cycles
against 66,000, which is now two thirds of this driver's CPU time. Of ext-mongodb's user-space
instructions per `findOne`, 21,900 run in libbson (9,900 of them stepping from one element to the
next), 6,400 in `malloc`/`free` and 2,200 in 42 case-insensitive string comparisons. Selecting
the server costs it 4,400 instructions, plus 3,200 to copy and free two server descriptions. This
driver spends 5,400 in BSON code, 4,100 in `malloc`/`free`, 300 on the whole server selection and
none on string comparisons.

## Event loops

The built-in loop at revolt-rs 2b6f678 (after its optimisation pass) against the previous build
(bbc4049, revolt-rs ed38299) and against the revolt/event-loop PHP library (an older build,
338845e, with `mongodb.native_event_loop=0`, which later builds no longer have), measured on
2026-11-12 at 354d184 with `tools/bench/revolt.sh`: PHP 8.4 CLI, release builds, one process per
measurement (5,000 `{main}` calls, or 20,000 calls shared by the fibers, after a warm-up), the
configurations alternating with their order rotated every round, 9 rounds (macOS `findOne`: 15),
medians of the client's CPU time (user + system) per operation.

- **Linux:** AMD Ryzen 9 5950X, kernel 6.8, the PHP process pinned to one core, MongoDB 8.0.15
  standalone on the same machine.
- **macOS:** 27 arm64 (Apple M3 Pro, which cannot pin a process to a core), MongoDB 8.0 in Docker.
  Wall time is dominated by Docker's round trip (about 0.2 ms). The machine was in desktop use:
  `findOne` in `{main}` and in one fiber ran at one of two speeds per process (up to ±30%), so
  those medians compare poorly.

`{main}`, CPU per operation (µs), Linux / macOS:

| Configuration | `ping` | `findOne` | `insertOne` |
|---|---|---|---|
| Revolt not used (`EventLoop` untouched): the synchronous path | 9.6 / 18.8 | 11.8 / 24.0 | 11.6 / 19.4 |
| loop in use, idle: the synchronous path | **9.5 / 18.6** | **12.3 / 22.9** | **11.7 / 19.6** |
| loop in use with other work (an unreferenced timer): through the loop | **10.4 / 19.3** | **13.0 / 24.7** | **12.6 / 20.0** |
| previous build, loop in use with other work | 10.8 / 19.2 | 13.5 / 26.4 | 13.0 / 20.1 |
| PHP library (`StreamSelectDriver`), through the loop | 21.2 / 25.8 | 23.9 / 32.2 | 23.6 / 26.4 |

Fibers (`EventLoop::queue()`, then `EventLoop::run()`; default `maxPoolSize`), CPU per operation
(µs): built-in loop / previous build / PHP library, Linux:

| Fibers | `ping` | `findOne` | `insertOne` |
|---|---|---|---|
| 1 | 11.6 / 12.2 / 23.7 | 14.1 / 14.7 / 27.1 | 13.7 / 14.2 / 26.5 |
| 10 | 8.9 / 9.5 / 10.6 | 12.7 / 13.7 / 14.0 | 13.8 / 12.5 / 13.3 |
| 50 | 9.5 / 9.3 / 11.4 | 11.5 / 12.0 / 14.7 | 11.8 / 11.9 / 14.7 |
| 200 | 10.4 / 10.5 / 15.0 | 13.5 / 13.9 / 18.9 | 13.4 / 13.8 / 18.6 |

macOS:

| Fibers | `ping` | `findOne` | `insertOne` |
|---|---|---|---|
| 1 | 20.1 / 20.0 / 27.1 | 32.8 / 35.4 / 41.8 | 20.7 / 20.7 / 27.5 |
| 10 | 18.9 / 18.9 / 21.0 | 20.5 / 20.6 / 23.0 | 20.3 / 20.5 / 22.9 |
| 50 | 14.1 / 14.2 / 15.3 | 14.9 / 15.5 / 16.6 | 15.0 / 15.4 / 16.4 |
| 200 | 14.0 / 14.3 / 18.0 | 15.2 / 15.4 / 19.0 | 15.3 / 15.1 / 18.9 |

- **The driver's sockets live in the loop's poller:** registered edge-triggered once (at their
  first wait, per request), handed to the driver in one batch per poll. A `{main}` `ping` through
  a loop with other work makes 3 system calls on Linux (send, `epoll_wait`, receive; the previous
  version 4: its own epoll instance was drained after the loop's poll); with 50 or 200 fibers,
  fewer than 0.2 `epoll_wait` per operation.
- **An idle loop costs nothing:** while the loop has nothing else to do (no enabled callback,
  referenced or not, nothing queued, no other wait of the driver), a `{main}` call blocks in its
  own send and receive, as without a loop: 2 system calls per `ping`, the same CPU time within
  noise. So does a call before the loop is used.
- **Through a loop with work,** `{main}` calls cost 0.8–1.2 µs of CPU more than the synchronous
  path on Linux (the previous build: 1.1–1.6 µs) and 0.5–0.6 µs on macOS (the same as before);
  the PHP library costs 12 µs and 7 µs more.
- **Fibers:** 4–5% less CPU than the previous build in one fiber on Linux; at 50 and 200 fibers
  between 4% less and 2% more (on Linux each process with 10 fibers runs at one of two speeds
  whatever the build, so single medians move by up to ±25%); on macOS the same within noise.
  Against the PHP library: 17–31% less CPU on Linux and 8–22% on macOS from 50 fibers on.
- `EventLoop::setDriver()` is refused, so these are always the numbers of the built-in loop.

How the driver uses the loop: each socket is registered once, edge-triggered, in the poller of
the loop's `NativeDriver` (no Revolt callback per socket), so the driver needs no system call to
set up a wait for a reply; the earliest deadline is one unreferenced `delay()`. The waits, that
timer, `getSuspension()`, `suspend()` and `resume()` are direct calls into revolt-rs, without PHP
calls. [DIVERGENCES.md](../DIVERGENCES.md) ("Driver calls inside a `Fiber`") has the rules for `{main}`.

The scripts are in `tools/bench/` (`rtt.php`, `bson.php`, `fromjson.php`, `handshake.php`,
`fibers.php`, `main.php`, `scan.php`, `revolt.php` with `revolt.sh`).
