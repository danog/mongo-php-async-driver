--TEST--
Fibers: a blocking {main} call while fibers hold all maxPoolSize clusters uses the spare one
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Concurrent operations in fibers"). maxPoolSize=1 and 2:
fibers hold every cluster with an operation in flight and one more fiber waits for a cluster. A
blocking {main} call (made where PHP forbids fiber switches: a tick function) cannot wait for
one (the fibers holding them cannot run while it blocks): it uses the client's spare cluster at
once, kept for such calls (a second {main} call reuses its connection), and never handed to a
fiber: the waiting fiber still waits for a fiber's cluster, and later fibers still run at most
maxPoolSize operations at once. Made anywhere else, {main} waits for a cluster through the loop
as a fiber does (after the fiber already waiting).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
// $f() where PHP forbids fiber switches (in a tick function): {main}'s driver calls block there,
// while fibers wait in the driver (elsewhere such calls wait through the busy loop)
function blocked(callable $f): mixed {
    $result = null;
    $done = false;
    $tick = function () use ($f, &$result, &$done) {
        if (!$done) {
            $done = true;
            $result = $f();
        }
    };
    register_tick_function($tick);
    declare(ticks=1) {
        $done = $done;
    }
    unregister_tick_function($tick);
    return $result;
}
$uri = getenv('MONGODB_URI');
$ns = 'mdrop_local.fiber_mixed_pool';
$admin = new Manager($uri);
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_pool']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'fiber']);
$bw->insert(['_id' => 2, 'x' => 'main']);
$admin->executeBulkWrite($ns, $bw);
// the connections of the client whose appName is $app (idle ones included), on every server
$conns = function (string $app) use ($admin): int {
    $n = 0;
    foreach ($admin->getServers() as $s) {
        $n += count($s->executeCommand('admin', new Command(['currentOp' => 1, '$all' => true, 'appName' => $app]))->toArray()[0]->inprog);
    }
    return $n;
};
$ms = fn (int $t) => (hrtime(true) - $t) / 1e6;
// $n fibers at once, each with a 300 ms query: started (and suspended), not awaited
$start = function (Manager $m, int $n) use ($ns): array {
    $fibers = [];
    for ($i = 0; $i < $n; $i++) {
        $fibers[$i] = new Fiber(fn () => $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true', '_id' => 1]))->toArray()[0]->x);
        $fibers[$i]->start();
    }
    return $fibers;
};

foreach (['blocking', 'loop'] as $mode) {
    echo "{main} $mode\n";
    $call = $mode === 'blocking' ? blocked(...) : fn (callable $f) => $f();
    foreach ([1, 2] as $max) {
        $app = "fiber-mixed-pool-$mode-$max";
        $m = new Manager($uri, ['maxPoolSize' => $max, 'appname' => $app], ['disableClientPersistence' => true]);
        $m->executeCommand('admin', new Command(['ping' => 1]));
        $t = hrtime(true);
        $fibers = $start($m, $max + 1);
        $t1 = hrtime(true);
        $main = $call(fn () => $m->executeQuery($ns, new Query(['_id' => 2]))->toArray()[0]->x);
        $mainMs = $ms($t1);
        $same = null;
        if ($mode === 'blocking') {
            $same = blocked(function () use ($conns, $app, $m) {
                $before = $conns($app);
                $m->executeCommand('admin', new Command(['ping' => 1]));
                return $conns($app) === $before;
            });
        }
        EventLoop::run();
        $all = $ms($t);
        echo "maxPoolSize=$max: {main} $main, ", $mainMs < 250 ? 'at once' : 'after waiting for a cluster', ", fibers ", implode(' ', array_map(fn ($f) => $f->getReturn(), $fibers)), "\n";
        if ($same !== null) {
            echo "  a second {main} call reuses the spare's connection: ", var_export($same, true), "\n";
        }
        echo "  the last fiber waited for a fiber's cluster: ", var_export($all >= 550, true), "\n";
        $t = hrtime(true);
        $fibers = $start($m, $max + 1);
        EventLoop::run();
        echo "  then ", $max + 1, " fibers at once still take two rounds: ", var_export($ms($t) >= 550, true), "\n";
    }
}
?>
--EXPECT--
{main} blocking
maxPoolSize=1: {main} main, at once, fibers fiber fiber
  a second {main} call reuses the spare's connection: true
  the last fiber waited for a fiber's cluster: true
  then 2 fibers at once still take two rounds: true
maxPoolSize=2: {main} main, at once, fibers fiber fiber fiber
  a second {main} call reuses the spare's connection: true
  the last fiber waited for a fiber's cluster: true
  then 3 fibers at once still take two rounds: true
{main} loop
maxPoolSize=1: {main} main, after waiting for a cluster, fibers fiber fiber
  the last fiber waited for a fiber's cluster: true
  then 2 fibers at once still take two rounds: true
maxPoolSize=2: {main} main, after waiting for a cluster, fibers fiber fiber fiber
  the last fiber waited for a fiber's cluster: true
  then 3 fibers at once still take two rounds: true
