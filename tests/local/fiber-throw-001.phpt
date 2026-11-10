--TEST--
Fibers: an exception thrown into a fiber or {main} waiting in the driver ends the call with it
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Needs revolt/event-loop:
REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has revolt/event-loop>. The driver
waits on the suspension EventLoop::getSuspension() returns (in {main}: the one the driver keeps
between waits), so a Suspension::throw() by someone holding it (a cancellation), or a
Fiber::throw(), ends the driver call with that exception. The interrupted operation's
connection is not reused for a later reply (the next calls get their own results), its late
reply resumes nothing, and the loop is not kept alive by it.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$ns = 'mdrop_local.fiber_throw';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_throw']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'slow']);
$bw->insert(['_id' => 2, 'x' => 'fast']);
$m->executeBulkWrite($ns, $bw);
class_exists(EventLoop::class);
$slow = fn () => $m->executeQuery($ns, new Query(['_id' => 1, '$where' => 'sleep(300) || true']))->toArray()[0]->x;
$fast = fn () => $m->executeQuery($ns, new Query(['_id' => 2]))->toArray()[0]->x;
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$runs = function (string $what) {
    $t = hrtime(true);
    EventLoop::run();
    echo "$what: run() returned at once: ", var_export((hrtime(true) - $t) < 1e8, true), "\n";
};
$call = function (string $what, Closure $f) {
    try {
        $r = $f();
        echo "$what: returned ", var_export($r, true), "\n";
    } catch (Exception $e) {
        echo "$what: ", get_class($e), ": ", $e->getMessage(), "\n";
    }
};

echo "-- Suspension::throw() in {main}\n";
$s = EventLoop::getSuspension();
EventLoop::delay(0.05, fn () => $s->throw(new Exception('cancelled main')));
$call('{main}', $slow);
$call('{main}', $fast);
var_dump($ping());
echo "{main}'s suspension is still the same: ", var_export($s === EventLoop::getSuspension(), true), "\n";
// the interrupted query's reply arrives meanwhile
EventLoop::delay(0.4, fn () => null);
EventLoop::run();
$call('{main}', $fast);
$runs('{main}');

echo "-- Suspension::throw() in a fiber\n";
$out = [];
EventLoop::queue(function () use ($slow, $fast, $call) {
    $s = EventLoop::getSuspension();
    EventLoop::delay(0.05, fn () => $s->throw(new Exception('cancelled fiber')));
    $call('fiber', $slow);
    $call('fiber', $fast);
    echo "the fiber's suspension is still the same: ", var_export($s === EventLoop::getSuspension(), true), "\n";
});
// another fiber's and {main}'s calls go on meanwhile
EventLoop::queue(fn () => $call('other fiber', $slow));
$call('{main}', $fast);
EventLoop::run();
$runs('fibers');

echo "-- Fiber::throw()\n";
$f = new Fiber(function () use ($slow, $fast, $call) {
    $call('fiber', $slow);
    $call('fiber', $fast);
    $call('fiber', $slow);
});
EventLoop::delay(0.05, fn () => $f->throw(new Exception('thrown into the fiber')));
$f->start();
EventLoop::run();
echo "fiber ended: ", var_export($f->isTerminated(), true), "\n";
$runs('Fiber::throw()');
$call('{main}', $fast);
$call('{main}', $slow);
$runs('end');
?>
--EXPECT--
-- Suspension::throw() in {main}
{main}: Exception: cancelled main
{main}: returned 'fast'
float(1)
{main}'s suspension is still the same: true
{main}: returned 'fast'
{main}: run() returned at once: true
-- Suspension::throw() in a fiber
{main}: returned 'fast'
fiber: Exception: cancelled fiber
fiber: returned 'fast'
the fiber's suspension is still the same: true
other fiber: returned 'slow'
fibers: run() returned at once: true
-- Fiber::throw()
fiber: Exception: thrown into the fiber
fiber: returned 'fast'
fiber: returned 'slow'
fiber ended: true
Fiber::throw(): run() returned at once: true
{main}: returned 'fast'
{main}: returned 'slow'
end: run() returned at once: true
