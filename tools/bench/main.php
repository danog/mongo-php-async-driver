<?php
// Sequential operations in {main}, with or without Revolt loaded.
// [RTT_REVOLT=<revolt autoload.php>] php -n -d extension=<so> main.php <uri> <n> <rounds> <case>
// One JSON line: Revolt driver, best-round wall ns/op, user and sys CPU ns/op (of the lowest-CPU round).
[$uri, $n, $rounds, $case] = [$argv[1], (int) $argv[2], (int) $argv[3], $argv[4]];
$rv = (string) getenv("RTT_REVOLT");
$driver = "none";
if ($rv !== "") {
    require $rv;
    $driver = (new ReflectionClass(Revolt\EventLoop::getDriver()))->getShortName();
}
$m = new MongoDB\Driver\Manager($uri);
$ns = "mdrop_bench.main";
$m->executeCommand("mdrop_bench", new MongoDB\Driver\Command(["drop" => "main"]));
$b = new MongoDB\Driver\BulkWrite;
$b->insert(["_id" => 1, "name" => "x", "n" => 42]);
$m->executeBulkWrite($ns, $b);
$f = match ($case) {
    "ping" => fn () => $m->executeCommand("admin", new MongoDB\Driver\Command(["ping" => 1])),
    "findOne" => function () use ($m, $ns) { foreach ($m->executeQuery($ns, new MongoDB\Driver\Query(["_id" => 1], ["limit" => 1])) as $d) {} },
    "insertOne" => function () use ($m, $ns) { $b = new MongoDB\Driver\BulkWrite; $b->insert(["name" => "y", "n" => 1]); $m->executeBulkWrite($ns, $b); },
};
function ru(): array { $r = getrusage(); return [$r["ru_utime.tv_sec"] * 1000000000 + $r["ru_utime.tv_usec"] * 1000, $r["ru_stime.tv_sec"] * 1000000000 + $r["ru_stime.tv_usec"] * 1000]; }
for ($i = 0; $i < 200; $i++) $f();
$best = PHP_INT_MAX; $bu = $bs = -1;
for ($r = 0; $r < $rounds; $r++) {
    [$u0, $s0] = ru(); $t0 = hrtime(true);
    for ($i = 0; $i < $n; $i++) $f();
    $w = intdiv(hrtime(true) - $t0, $n); [$u1, $s1] = ru();
    $best = min($best, $w);
    $u = intdiv($u1 - $u0, $n); $s = intdiv($s1 - $s0, $n);
    if ($bu < 0 || $u + $s < $bu + $bs) { [$bu, $bs] = [$u, $s]; }
}
echo json_encode(["case" => $case, "driver" => $driver, "ns_per_op" => $best, "cpu_ns_per_op" => $bu + $bs, "user_ns_per_op" => $bu, "sys_ns_per_op" => $bs]), "\n";
