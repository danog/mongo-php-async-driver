--TEST--
Manager: a client used before fork() is reset in the child (no inherited socket is used)
--SKIPIF--
<?php if (!function_exists('pcntl_fork')) die('skip pcntl not available'); ?>
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
$m = new MongoDB\Driver\Manager(getenv('MONGODB_URI'));
$ping = new MongoDB\Driver\Command(['ping' => 1]);
$m->executeCommand('admin', $ping);
$m->executeCommand('t', new MongoDB\Driver\Command(['drop' => 'fork001']));
$m->executeCommand('t', new MongoDB\Driver\Command(['insert' => 'fork001', 'documents' => [['x' => 1], ['x' => 2], ['x' => 3]]]));
$cursor = $m->executeCommand('t', new MongoDB\Driver\Command(['find' => 'fork001', 'batchSize' => 1]));
$cursor->rewind();
$pid = pcntl_fork();
if ($pid === 0) {
    // the parent's cursor cannot advance after the reset
    try {
        $cursor->next();
        $cursor->next();
        echo "child: cursor advanced\n";
    } catch (MongoDB\Driver\Exception\RuntimeException $e) {
        echo "child: ", $e->getMessage(), "\n";
    }
    for ($i = 0; $i < 50; $i++) {
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['hello' => 1]))->toArray()[0];
        if (!isset($r->isWritablePrimary)) { echo "child: wrong reply\n"; }
    }
    echo "child: ok\n";
    exit(0);
}
for ($i = 0; $i < 50; $i++) {
    $r = $m->executeCommand('admin', $ping)->toArray()[0];
    if (isset($r->isWritablePrimary)) { echo "parent: wrong reply\n"; }
}
pcntl_waitpid($pid, $status);
// the parent's cursor still works on the parent's connection
$n = 1;
foreach ((function () use ($cursor) { $cursor->next(); while ($cursor->valid()) { yield $cursor->current(); $cursor->next(); } })() as $d) { $n++; }
echo "parent: ", $n, " documents\n";
$m->executeCommand('t', new MongoDB\Driver\Command(['drop' => 'fork001']));
?>
--EXPECT--
child: Cannot advance cursor after client reset
child: ok
parent: 3 documents
