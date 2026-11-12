--TEST--
Fibers: concurrent operations, {main} running the loop, pool limit, destroyed fiber, cursor guard
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber").
- 8 fibers each running a query whose server-side $where sleeps 300 ms finish in about 300 ms;
- a {main} call while fibers wait in the driver runs the event loop (they progress meanwhile);
- maxPoolSize=1: a second fiber waits for the cluster, at most serverSelectionTimeoutMS;
- a fiber left unreferenced while it waits lives until its wait ends; {main} goes on meanwhile;
- a cursor being advanced (getMore) by one fiber is refused to another (LogicException).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$ns = 'mdrop_local.fiber_concurrency';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_concurrency']));
$bw = new BulkWrite;
for ($i = 0; $i < 6; $i++) $bw->insert(['k' => $i]);
$m->executeBulkWrite($ns, $bw);
$slow = fn (int $ms, array $o = ['limit' => 1]) => new Query(['$where' => "sleep($ms) || true"], $o);

$t = hrtime(true);
$done = 0;
for ($i = 0; $i < 8; $i++) EventLoop::queue(function () use ($m, $ns, $slow, &$done) { $m->executeQuery($ns, $slow(300))->toArray(); $done++; });
EventLoop::run();
$ms = (hrtime(true) - $t) / 1e6;
echo "8 fibers: done=$done concurrent=", var_export($ms < 1500, true), "\n";

$done = 0;
for ($i = 0; $i < 3; $i++) EventLoop::queue(function () use ($m, $ns, $slow, &$done) { $m->executeQuery($ns, $slow(200))->toArray(); $done++; });
$s = EventLoop::getSuspension();
EventLoop::defer(fn () => $s->resume());
$s->suspend();
// {main}'s find outlasts the fibers' by 800 ms or more: under parallel test load, mongod's
// execution admission queue (read tickets) was seen holding a fiber's find for up to 400 ms
$m->executeQuery($ns, $slow(1000))->toArray();
echo "fibers done during the {main} call: $done\n";
EventLoop::run();

$m1 = new Manager($uri . $sep . 'maxPoolSize=1&serverSelectionTimeoutMS=300');
$m1->executeCommand('admin', new Command(['ping' => 1]));
for ($i = 0; $i < 2; $i++) EventLoop::queue(function () use ($m1, $ns, $slow, $i) {
    try { $m1->executeQuery($ns, $slow(1000))->toArray(); echo "pool $i: done\n"; }
    catch (MongoDB\Driver\Exception\Exception $e) { echo "pool $i: ", get_class($e), ': ', $e->getMessage(), "\n"; }
});
EventLoop::run();

// a fiber waiting in the driver stays alive until its wait ends (Revolt's suspension holds it);
// meanwhile {main} runs operations on other connections
$f = new Fiber(function () use ($m, $ns, $slow) { $m->executeQuery($ns, $slow(500))->toArray(); echo "unreferenced fiber: done\n"; });
$f->start();
unset($f);
var_dump($m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok);
EventLoop::run();

$c = $m->executeQuery($ns, $slow(50, ['batchSize' => 2]));
$it = new IteratorIterator($c);
$it->rewind();
EventLoop::queue(function () use ($it) { $it->next(); $it->next(); echo "A: k=", $it->current()->k, "\n"; });
EventLoop::queue(function () use ($c) { try { $c->next(); } catch (MongoDB\Driver\Exception\LogicException $e) { echo "B: ", $e->getMessage(), "\n"; } });
EventLoop::run();
?>
--EXPECT--
8 fibers: done=8 concurrent=true
fibers done during the {main} call: 3
pool 1: MongoDB\Driver\Exception\ConnectionTimeoutException: Timed out while checking out a connection from connection pool
pool 0: done
float(1)
unreferenced fiber: done
B: The cursor is in use by another fiber
A: k=2
