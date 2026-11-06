<?php
// Fiber concurrency bench: REVOLT_AUTOLOAD=<vendor/autoload.php> php -n -d extension=<so> tools/bench/fibers.php <uri> [ops]
// Cases (one JSON line each: ops/s, CPU ns/op, wall ns/op):
//   seq-main      findOne in {main}, sequential (the synchronous path)
//   seq-fiber     findOne in one fiber, sequential (every wait suspends)
//   c<F>-p<P>     F fibers sharing `ops` findOnes, maxPoolSize=P
//   mixed         a {main} findOne loop while 50 fibers run findOnes (pool 4): {main} p50 latency
require getenv('REVOLT_AUTOLOAD') ?: __DIR__ . '/vendor/autoload.php';
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;

$uri = $argv[1] ?? 'mongodb://127.0.0.1:28010';
$ops = (int) ($argv[2] ?? 20000);
$ns = 'mdrop_bench.fib';
$sep = str_contains($uri, '?') ? '&' : '/?';
$m = new Manager($uri);
$m->executeCommand('mdrop_bench', new Command(['drop' => 'fib']));
$b = new BulkWrite;
$b->insert(['_id' => 1, 'name' => 'x', 'n' => 42]);
$m->executeBulkWrite($ns, $b);
$q = new Query(['_id' => 1], ['limit' => 1]);
$one = function (Manager $m) use ($ns, $q) {
    foreach ($m->executeQuery($ns, $q) as $d) {
    }
};
function cpu(): int { $r = getrusage(); return ($r['ru_utime.tv_sec'] + $r['ru_stime.tv_sec']) * 1000000000 + ($r['ru_utime.tv_usec'] + $r['ru_stime.tv_usec']) * 1000; }
function report(string $case, int $n, int $t0, int $c0, array $extra = []): void {
    $wall = hrtime(true) - $t0; $cpu = cpu() - $c0;
    echo json_encode(['case' => $case, 'ops_per_s' => round($n / ($wall / 1e9)), 'cpu_ns_per_op' => intdiv($cpu, $n), 'ns_per_op' => intdiv($wall, $n)] + $extra), "\n";
}
for ($i = 0; $i < 1000; $i++) $one($m);

[$t0, $c0] = [hrtime(true), cpu()];
for ($i = 0; $i < $ops; $i++) $one($m);
report('seq-main', $ops, $t0, $c0);

[$t0, $c0] = [hrtime(true), cpu()];
EventLoop::queue(function () use ($m, $one, $ops) { for ($i = 0; $i < $ops; $i++) $one($m); });
EventLoop::run();
report('seq-fiber', $ops, $t0, $c0);

foreach ([[50, 4], [50, 32], [200, 4], [200, 32]] as [$f, $p]) {
    $mp = new Manager($uri . $sep . "maxPoolSize=$p");
    $one($mp);
    $left = $ops;
    [$t0, $c0] = [hrtime(true), cpu()];
    for ($k = 0; $k < $f; $k++) {
        EventLoop::queue(function () use ($mp, $one, &$left) { while ($left-- > 0) $one($mp); });
    }
    EventLoop::run();
    report("c$f-p$p", $ops, $t0, $c0);
}

$mp = new Manager($uri . $sep . 'maxPoolSize=4');
$one($mp);
$stop = false;
for ($k = 0; $k < 50; $k++) {
    EventLoop::queue(function () use ($mp, $one, &$stop) { while (!$stop) $one($mp); });
}
$s = EventLoop::getSuspension();
EventLoop::defer(fn () => $s->resume());
$s->suspend();
$lat = [];
[$t0, $c0] = [hrtime(true), cpu()];
for ($i = 0; $i < 2000; $i++) { $t = hrtime(true); $one($mp); $lat[] = hrtime(true) - $t; }
$stop = true;
EventLoop::run();
sort($lat);
report('mixed-main', 2000, $t0, $c0, ['main_p50_ns' => $lat[1000], 'main_p99_ns' => $lat[1980]]);
