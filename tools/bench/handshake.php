<?php
// Connection-setup microbench: php -n -d extension=<so> tools/bench/handshake.php <uri> [iterations] [rounds]
// Each iteration creates a non-persistent Manager and runs one ping: TCP connect, TLS handshake
// (for a TLS URI), the MongoDB handshake (hello) and the command; the client is freed afterwards.
// Run it with a plain and a TLS URI on the same server: the difference is the TLS cost.
// Prints one JSON line: {"case": "connect+ping", "ns_per_op": <best round>, "cpu_ns_per_op": <best round>}.
$uri = $argv[1] ?? 'mongodb://127.0.0.1:28010';
$n = (int) ($argv[2] ?? 300);
$rounds = (int) ($argv[3] ?? 5);
$ping = new MongoDB\Driver\Command(['ping' => 1]);

function cpu_ns(): int
{
    $r = getrusage();
    return ($r['ru_utime.tv_sec'] + $r['ru_stime.tv_sec']) * 1000000000 + ($r['ru_utime.tv_usec'] + $r['ru_stime.tv_usec']) * 1000;
}

$f = function () use ($uri, $ping) {
    $m = new MongoDB\Driver\Manager($uri, [], ['disableClientPersistence' => true]);
    $m->executeCommand('admin', $ping);
};
for ($i = 0; $i < 20; $i++) {
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
echo json_encode(['case' => 'connect+ping', 'ns_per_op' => $best, 'cpu_ns_per_op' => $bestCpu]), "\n";
