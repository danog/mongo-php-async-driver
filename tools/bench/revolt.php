<?php
// The event loops compared: one measurement per process, so that each runs with its own INI and
// Revolt (tools/bench/revolt.sh runs them alternately, many rounds).
//   php -n -d extension=<so> [-d extension=ev.so]
//       tools/bench/revolt.php <uri> <loop> <case> <fibers> <ops>
// <loop>: "used" (EventLoop::getDriver() first: the loop is in use; an older build without the
// built-in classes (mongodb.native_event_loop=0) gets the PHP library from REVOLT_AUTOLOAD), or
// "none" (Revolt not touched), or "busy" (in use, with an unreferenced repeat timer of an hour:
// work the loop must watch, so {main} calls wait through it instead of blocking).
// <case>: ping, findOne or insertOne. <fibers>: 0 = sequential calls in {main}, else that many
// fibers sharing the <ops> calls (EventLoop::queue(), then EventLoop::run()).
// One JSON line: the Revolt driver, wall and CPU (user + system) ns per operation.
[, $uri, $loop, $case, $fibers, $ops] = $argv;
[$fibers, $ops] = [(int) $fibers, (int) $ops];
$driver = 'none';
if ($loop === 'used' || $loop === 'busy') {
    if (!class_exists(Revolt\EventLoop::class, false)) {
        require getenv('REVOLT_AUTOLOAD');
    }
    $driver = (new ReflectionClass(Revolt\EventLoop::getDriver()))->getShortName();
    if ($loop === 'busy') {
        Revolt\EventLoop::unreference(Revolt\EventLoop::repeat(3600, fn () => null));
    }
}
$m = new MongoDB\Driver\Manager($uri);
$ns = 'mdrop_bench.revolt';
$m->executeCommand('mdrop_bench', new MongoDB\Driver\Command(['drop' => 'revolt']));
$b = new MongoDB\Driver\BulkWrite;
$b->insert(['_id' => 1, 'name' => 'x', 'n' => 42]);
$m->executeBulkWrite($ns, $b);
$f = match ($case) {
    'ping' => fn () => $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1])),
    'findOne' => function () use ($m, $ns) { foreach ($m->executeQuery($ns, new MongoDB\Driver\Query(['_id' => 1], ['limit' => 1])) as $d) {} },
    'insertOne' => function () use ($m, $ns) { $b = new MongoDB\Driver\BulkWrite; $b->insert(['name' => 'y', 'n' => 1]); $m->executeBulkWrite($ns, $b); },
};
function cpu(): int { $r = getrusage(); return ($r['ru_utime.tv_sec'] + $r['ru_stime.tv_sec']) * 1000000000 + ($r['ru_utime.tv_usec'] + $r['ru_stime.tv_usec']) * 1000; }
$run = function (int $n) use ($f, $fibers) {
    if ($fibers === 0) {
        for ($i = 0; $i < $n; $i++) $f();
        return;
    }
    $left = $n;
    for ($k = 0; $k < $fibers; $k++) {
        Revolt\EventLoop::queue(function () use ($f, &$left) { while ($left-- > 0) $f(); });
    }
    Revolt\EventLoop::run();
};
// warm-up: the connections the measured run uses (one per concurrent fiber) exist already
$run(max(200, 4 * $fibers));
[$t0, $c0] = [hrtime(true), cpu()];
$run($ops);
[$t1, $c1] = [hrtime(true), cpu()];
echo json_encode(['driver' => $driver, 'ns_per_op' => intdiv($t1 - $t0, $ops), 'cpu_ns_per_op' => intdiv($c1 - $c0, $ops)]), "\n";
