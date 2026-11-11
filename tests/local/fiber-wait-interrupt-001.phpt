--TEST--
Fibers: what an interrupted fiber held goes to the fibers waiting for it
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "A fiber wait interrupted by an exception"). Fiber A holds
something fiber B waits for, and an exception is thrown into A (Fiber::throw()): A's call ends
with it, and B goes on (it used to wait forever: A gave the thing back while its exception was
pending, when PHP skips the call that resumes B).
- pool: maxPoolSize=1, A's query holds the only cluster, B waits to check one out; B then runs.
- scan: A scans a server that accepts connections and never answers, B waits for that scan; the
  scan is abandoned, and B's selection fails as after any scan that found nothing
  (serverSelectionTryOnce).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$ns = 'mdrop_local.fiber_wait_interrupt';
$admin = new Manager($uri);
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_wait_interrupt']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1]);
$admin->executeBulkWrite($ns, $bw);
$ping = fn (Manager $m) => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$fiber = fn (string $name, Closure $f) => new Fiber(function () use ($name, $f) {
    try {
        $r = $f();
    } catch (Exception $e) {
        $r = $e->getMessage() === 'interrupted' ? 'interrupted' : get_class($e);
    }
    echo "$name: $r\n";
});
// A and B start; once both wait, an exception is thrown into A
$run = function (Fiber $a, Fiber $b) {
    $a->start();
    $b->start();
    $s = EventLoop::getSuspension();
    EventLoop::delay(0.2, fn () => $s->resume());
    $s->suspend();
    echo "both suspended: ", var_export($a->isSuspended() && $b->isSuspended(), true), "\n";
    $a->throw(new Exception('interrupted'));
    EventLoop::run();
};

echo "pool\n";
$m = new Manager($uri, ['maxPoolSize' => 1], ['disableClientPersistence' => true]);
$ping($m);
$run(
    $fiber('A', fn () => count($m->executeQuery($ns, new Query(['$where' => 'sleep(1000) || true']))->toArray())),
    $fiber('B', fn () => $ping($m)),
);

echo "scan\n";
// a server that completes the TCP handshake (the listen backlog) and never answers
$mute = stream_socket_server('tcp://127.0.0.1:0');
$port = parse_url('tcp://' . stream_socket_get_name($mute, false), PHP_URL_PORT);
$m = new Manager("mongodb://127.0.0.1:$port/?connectTimeoutMS=5000&serverSelectionTimeoutMS=5000", [], ['disableClientPersistence' => true]);
$run($fiber('A', fn () => $ping($m)), $fiber('B', fn () => $ping($m)));
?>
--EXPECT--
pool
both suspended: true
A: interrupted
B: 1
scan
both suspended: true
A: interrupted
B: MongoDB\Driver\Exception\ConnectionTimeoutException
