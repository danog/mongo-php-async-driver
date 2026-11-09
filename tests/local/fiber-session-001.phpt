--TEST--
Fibers: a Session used while another fiber's operation holds it throws LogicException
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "A Cursor or Session used by two fibers at once").
Needs revolt/event-loop: REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has
revolt/event-loop>. While fiber A's query with a session waits for the server, every use of
that session by another fiber or by {main} throws MongoDB\Driver\Exception\LogicException
(code 0) without sending anything; a cursor whose getMore is refused that way stays usable.
Once A's operation ends, the session works again.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$ns = 'mdrop_local.fiber_session';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_session']));
$bw = new BulkWrite;
for ($i = 0; $i < 3; $i++) $bw->insert(['k' => $i]);
$m->executeBulkWrite($ns, $bw);
$s = $m->startSession();
$try = function (string $label, callable $f) {
    try {
        $f();
        echo "$label: ok\n";
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo "$label: ", get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
    }
};
// a cursor of the session with a pending getMore
$c = $m->executeQuery($ns, new Query([], ['batchSize' => 1, 'sort' => ['k' => 1]]), ['session' => $s]);
$it = new IteratorIterator($c);
$it->rewind();
echo "cursor: k=", $it->current()->k, "\n";

EventLoop::queue(function () use ($m, $ns, $s) {
    $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true'], ['limit' => 1]), ['session' => $s])->toArray();
    echo "A: done\n";
});
EventLoop::queue(function () use ($m, $ns, $s, $it, $try) {
    $bw = new BulkWrite;
    $bw->insert(['k' => 9]);
    $try('B executeCommand', fn () => $m->executeCommand('admin', new Command(['ping' => 1]), ['session' => $s]));
    $try('B executeQuery', fn () => $m->executeQuery($ns, new Query([]), ['session' => $s]));
    $try('B executeBulkWrite', fn () => $m->executeBulkWrite($ns, $bw, ['session' => $s]));
    $try('B getClusterTime', fn () => $s->getClusterTime());
    $try('B startTransaction', fn () => $s->startTransaction());
    $try('B endSession', fn () => $s->endSession());
    $try('B cursor getMore', fn () => $it->next());
    $try('B setTypeMap', fn () => $it->getInnerIterator()->setTypeMap(['root' => 'stdClass']));
});
// {main} once A waits and B is done
$main = EventLoop::getSuspension();
EventLoop::defer(fn () => $main->resume());
$main->suspend();
$try('{main} executeCommand', fn () => $m->executeCommand('admin', new Command(['ping' => 1]), ['session' => $s]));
EventLoop::run();

$try('after: executeCommand', fn () => $m->executeCommand('admin', new Command(['ping' => 1]), ['session' => $s]));
$it->next();
echo "after: cursor k=", $it->current()->k, "\n";
$it->next();
echo "after: cursor k=", $it->current()->k, "\n";
$try('after: endSession', fn () => $s->endSession());
?>
--EXPECT--
cursor: k=0
B executeCommand: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B executeQuery: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B executeBulkWrite: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B getClusterTime: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B startTransaction: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B endSession: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B cursor getMore: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
B setTypeMap: ok
{main} executeCommand: MongoDB\Driver\Exception\LogicException 0: The session is in use by another fiber
A: done
after: executeCommand: ok
after: cursor k=1
after: cursor k=2
after: endSession: ok
