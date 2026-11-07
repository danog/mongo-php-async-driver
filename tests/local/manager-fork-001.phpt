--TEST--
Manager: no fork detection (as ext-mongodb): a child process uses the client, connections and cursors it inherited
--DESCRIPTION--
Local test (DIVERGENCES "fork()": parity). Using inherited objects from both processes at once is
the application's responsibility: here the parent waits for the child before using them again.
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
    // the parent's cursor advances in the child (getMore on the inherited connection)
    $xs = [$cursor->current()->x];
    for ($cursor->next(); $cursor->valid(); $cursor->next()) {
        $xs[] = $cursor->current()->x;
    }
    echo "child: ", implode(',', $xs), "\n";
    for ($i = 0; $i < 20; $i++) {
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['hello' => 1]))->toArray()[0];
        if (!isset($r->isWritablePrimary)) { echo "child: wrong reply\n"; }
    }
    echo "child: ok\n";
    exit(0);
}
pcntl_waitpid($pid, $status);
for ($i = 0; $i < 20; $i++) {
    $r = $m->executeCommand('admin', $ping)->toArray()[0];
    if (isset($r->isWritablePrimary) || $r->ok != 1) { echo "parent: wrong reply\n"; }
}
echo "parent: ok\n";
$m->executeCommand('t', new MongoDB\Driver\Command(['drop' => 'fork001']));
?>
--EXPECT--
child: 1,2,3
child: ok
parent: ok
