<?php
// Round-trip microbench: php -n -d extension=<so> tools/bench/rtt.php <uri> [iterations] [rounds]
// Prints one JSON line per case: {"case": ..., "ns_per_op": <best round>, "cpu_ns_per_op": <best round>}.
// Cases: findOne on a one-document collection, insertOne (acknowledged, w:1), ping.
$uri = $argv[1] ?? 'mongodb://127.0.0.1:28010';
$n = (int) ($argv[2] ?? 5000);
$rounds = (int) ($argv[3] ?? 5);
$m = new MongoDB\Driver\Manager($uri);
$ns = 'mdrop_bench.rtt';
$m->executeCommand('mdrop_bench', new MongoDB\Driver\Command(['drop' => 'rtt']));
$b = new MongoDB\Driver\BulkWrite;
$b->insert(['_id' => 1, 'name' => 'x', 'n' => 42]);
$m->executeBulkWrite($ns, $b);

$cases = [
    'findOne' => function () use ($m, $ns) {
        $c = $m->executeQuery($ns, new MongoDB\Driver\Query(['_id' => 1], ['limit' => 1]));
        foreach ($c as $d) {
        }
    },
    'insertOne' => function () use ($m, $ns) {
        $b = new MongoDB\Driver\BulkWrite;
        $b->insert(['name' => 'y', 'n' => 1]);
        $m->executeBulkWrite($ns, $b);
    },
    'ping' => function () use ($m) {
        $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
    },
];

function cpu_ns(): int
{
    $r = getrusage();
    return ($r['ru_utime.tv_sec'] + $r['ru_stime.tv_sec']) * 1000000000 + ($r['ru_utime.tv_usec'] + $r['ru_stime.tv_usec']) * 1000;
}

foreach ($cases as $name => $f) {
    for ($i = 0; $i < 200; $i++) {
        $f();
    }
    $best = PHP_INT_MAX;
    $bestCpu = PHP_INT_MAX;
    for ($r = 0; $r < $rounds; $r++) {
        $c0 = cpu_ns();
        $t0 = hrtime(true);
        for ($i = 0; $i < $n; $i++) {
            $f();
        }
        $best = min($best, intdiv(hrtime(true) - $t0, $n));
        $bestCpu = min($bestCpu, intdiv(cpu_ns() - $c0, $n));
    }
    echo json_encode(['case' => $name, 'ns_per_op' => $best, 'cpu_ns_per_op' => $bestCpu]), "\n";
}
$m->executeCommand('mdrop_bench', new MongoDB\Driver\Command(['drop' => 'rtt']));
