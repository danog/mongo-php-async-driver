<?php
// Initial scan time: a new client's first server selection (one scan of every seed).
//   php -n -d extension=<so> tools/bench/scan.php <uri> [reps] [read preference mode, default nearest]
// Prints the median, min and max wall time in milliseconds (JSON line).
$uri = $argv[1];
$reps = (int) ($argv[2] ?? 15);
$t = [];
for ($i = 0; $i < $reps; $i++) {
    $m = new MongoDB\Driver\Manager($uri, [], ['disableClientPersistence' => true]);
    $s = hrtime(true);
    $m->selectServer(new MongoDB\Driver\ReadPreference($argv[3] ?? 'nearest'));
    $t[] = (hrtime(true) - $s) / 1e6;
    unset($m);
}
sort($t);
echo json_encode(['uri' => $uri, 'reps' => $reps, 'p50_ms' => round($t[intdiv($reps, 2)], 2), 'min_ms' => round($t[0], 2), 'max_ms' => round(end($t), 2)]), "\n";
