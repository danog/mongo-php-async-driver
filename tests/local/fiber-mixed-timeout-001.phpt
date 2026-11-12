--TEST--
Fibers: a fiber's deadline passes while {main} blocks (socketTimeoutMS, maxTimeMS)
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). A fiber's operation is in
flight with a deadline shorter than a blocking {main} call (made where PHP forbids fiber
switches: a tick function) that keeps the loop from running past it. When the loop runs
afterwards, the fiber gets its reply when it is there by then (also one that came after the
deadline: the wait could not end earlier), the server's maxTimeMS error, or the socket timeout
when no reply is there; never a hang, never another operation's reply. Made anywhere else,
{main}'s call waits through the loop (busy with the fiber's wait), so the fiber's wait ends at
its deadline.
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
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$ns = 'mdrop_local.fiber_mixed_timeout';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_timeout']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'fiber']);
$bw->insert(['_id' => 2, 'x' => 'main']);
$m->executeBulkWrite($ns, $bw);
// reads are not retried: a socket timeout is reported as it happens
$short = new Manager($uri . $sep . 'socketTimeoutMS=300&retryReads=false');
$slow = fn (int $ms, int $id, array $opts = []) => new Query(['$where' => "sleep($ms) || true", '_id' => $id], $opts);
$cases = [
    'socketTimeoutMS=300, reply after 50 ms' => fn () => $short->executeQuery($ns, $slow(50, 1)),
    'socketTimeoutMS=300, reply after 450 ms' => fn () => $short->executeQuery($ns, $slow(450, 1)),
    'socketTimeoutMS=300, reply after 900 ms' => fn () => $short->executeQuery($ns, $slow(900, 1)),
    'maxTimeMS=100 expires (450 ms)' => fn () => $m->executeQuery($ns, $slow(450, 1, ['maxTimeMS' => 100])),
    'maxTimeMS=300 not reached (50 ms)' => fn () => $m->executeQuery($ns, $slow(50, 1, ['maxTimeMS' => 300])),
];

foreach (['blocking', 'loop'] as $mode) {
    echo "{main} $mode\n";
    $call = $mode === 'blocking' ? blocked(...) : fn (callable $f) => $f();
    foreach ($cases as $name => $op) {
        // connected, with a fresh topology: the operation sends its command at once
        $short->executeCommand('admin', new Command(['ping' => 1]));
        $f = new Fiber(function () use ($op) {
            try {
                return $op()->toArray()[0]->x;
            } catch (Exception $e) {
                return get_class($e) . ': ' . $e->getMessage();
            }
        });
        $f->start();
        // 600 ms
        $main = $call(fn () => $m->executeQuery($ns, $slow(600, 2))->toArray()[0]->x);
        EventLoop::run();
        echo "$name: ", $f->getReturn(), " ({main}: $main)\n";
    }
}
?>
--EXPECTF--
{main} blocking
socketTimeoutMS=300, reply after 50 ms: fiber ({main}: main)
socketTimeoutMS=300, reply after 450 ms: fiber ({main}: main)
socketTimeoutMS=300, reply after 900 ms: MongoDB\Driver\Exception\ConnectionTimeoutException: Failed to send "find" command with database "mdrop_local": Failed to read 4 bytes: socket error or timeout ({main}: main)
maxTimeMS=100 expires (450 ms): MongoDB\Driver\Exception\ExecutionTimeoutException: %s ({main}: main)
maxTimeMS=300 not reached (50 ms): fiber ({main}: main)
{main} loop
socketTimeoutMS=300, reply after 50 ms: fiber ({main}: main)
socketTimeoutMS=300, reply after 450 ms: MongoDB\Driver\Exception\ConnectionTimeoutException: Failed to send "find" command with database "mdrop_local": Failed to read 4 bytes: socket error or timeout ({main}: main)
socketTimeoutMS=300, reply after 900 ms: MongoDB\Driver\Exception\ConnectionTimeoutException: Failed to send "find" command with database "mdrop_local": Failed to read 4 bytes: socket error or timeout ({main}: main)
maxTimeMS=100 expires (450 ms): MongoDB\Driver\Exception\ExecutionTimeoutException: %s ({main}: main)
maxTimeMS=300 not reached (50 ms): fiber ({main}: main)
