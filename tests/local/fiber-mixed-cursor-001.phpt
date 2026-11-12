--TEST--
Fibers: cursors and sessions shared between a suspended fiber and blocking {main} calls
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "A Cursor or Session used by two fibers at once",
"Driver calls inside a Fiber"). A fiber iterates a cursor (a getMore in flight) and runs a query
with a session; meanwhile {main} iterates its own cursor and runs commands with its own session,
and touches the fiber's cursor and session: those throw the LogicException before anything is
sent (in blocking {main} calls, made where PHP forbids fiber switches: a tick function, as in
those through the loop), and the fiber's iteration and query
end with the right results; the session works again afterwards.
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
    // one statement that ticks: a call (PHP 8.6 emits no tick after a plain assignment)
    declare(ticks=1) {
        max(0, 1);
    }
    unregister_tick_function($tick);
    return $result;
}
$uri = getenv('MONGODB_URI');
$ns = 'mdrop_local.fiber_mixed_cursor';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_cursor']));
$bw = new BulkWrite;
for ($i = 0; $i < 10; $i++) {
    $bw->insert(['_id' => $i]);
}
$m->executeBulkWrite($ns, $bw);
$try = function (string $what, callable $f) {
    try {
        $f();
        echo "$what: no exception\n";
    } catch (Exception $e) {
        echo "$what: ", get_class($e), ': ', $e->getMessage(), "\n";
    }
};
$ids = function ($cursor): string {
    $ids = [];
    foreach ($cursor as $d) {
        $ids[] = $d->_id;
    }
    return implode(',', $ids);
};

foreach (['blocking', 'loop'] as $mode) {
    echo "{main} $mode\n";
    $call = $mode === 'blocking' ? blocked(...) : fn (callable $f) => $f();
    // the fiber's cursor: batches of 2, each getMore takes 200 ms (the first batch is in {main})
    $fc = $m->executeQuery($ns, new Query(['$where' => 'this._id < 2 || sleep(100) || true'], ['batchSize' => 2]));
    $cursorFiber = new Fiber(fn () => $ids($fc));
    $cursorFiber->start();
    $s = $m->startSession();
    $sessionFiber = new Fiber(fn () => $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true', '_id' => 1]), ['session' => $s])->toArray()[0]->_id);
    $sessionFiber->start();
    echo "fibers suspended: ", var_export($cursorFiber->isSuspended() && $sessionFiber->isSuspended(), true), "\n";

    $call(function () use ($try, $ids, $fc, $m, $ns, $s) {
        $try('{main} iterates the fiber\'s cursor', function () use ($fc) { foreach ($fc as $d) {} });
        $try('{main} toArray() of the fiber\'s cursor', fn () => $fc->toArray());
        $try('{main} query with the fiber\'s session', fn () => $m->executeQuery($ns, new Query(['_id' => 2]), ['session' => $s])->toArray());
        $try('{main} startTransaction() of the fiber\'s session', fn () => $s->startTransaction());
        echo "{main} cursor: ", $ids($m->executeQuery($ns, new Query([], ['batchSize' => 3]))), "\n";
        $own = $m->startSession();
        echo "{main} command with its own session: ", $m->executeCommand('admin', new Command(['ping' => 1]), ['session' => $own])->toArray()[0]->ok, "\n";
        echo "{main} cursor with its own session: ", $ids($m->executeQuery($ns, new Query(['_id' => ['$gte' => 7]], ['batchSize' => 1]), ['session' => $own])), "\n";
    });

    EventLoop::run();
    echo "fiber's cursor: ", $cursorFiber->getReturn(), "\n";
    echo "fiber's query with the session: ", $sessionFiber->getReturn(), "\n";
    echo "the session afterwards: ", $m->executeQuery($ns, new Query(['_id' => 3]), ['session' => $s])->toArray()[0]->_id, "\n";
}
?>
--EXPECT--
{main} blocking
fibers suspended: true
{main} iterates the fiber's cursor: MongoDB\Driver\Exception\LogicException: The cursor is in use by another fiber
{main} toArray() of the fiber's cursor: MongoDB\Driver\Exception\LogicException: The cursor is in use by another fiber
{main} query with the fiber's session: MongoDB\Driver\Exception\LogicException: The session is in use by another fiber
{main} startTransaction() of the fiber's session: MongoDB\Driver\Exception\LogicException: The session is in use by another fiber
{main} cursor: 0,1,2,3,4,5,6,7,8,9
{main} command with its own session: 1
{main} cursor with its own session: 7,8,9
fiber's cursor: 0,1,2,3,4,5,6,7,8,9
fiber's query with the session: 1
the session afterwards: 3
{main} loop
fibers suspended: true
{main} iterates the fiber's cursor: MongoDB\Driver\Exception\LogicException: The cursor is in use by another fiber
{main} toArray() of the fiber's cursor: MongoDB\Driver\Exception\LogicException: The cursor is in use by another fiber
{main} query with the fiber's session: MongoDB\Driver\Exception\LogicException: The session is in use by another fiber
{main} startTransaction() of the fiber's session: MongoDB\Driver\Exception\LogicException: The session is in use by another fiber
{main} cursor: 0,1,2,3,4,5,6,7,8,9
{main} command with its own session: 1
{main} cursor with its own session: 7,8,9
fiber's cursor: 0,1,2,3,4,5,6,7,8,9
fiber's query with the session: 1
the session afterwards: 3
