--TEST--
Fibers: {main}'s Revolt suspension after the loop ended without resuming it, and shared with the caller
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). The driver
waits on {main}'s suspension (the same object while it lives); Revolt makes a new one once a {main} suspend() ends
without a resume (an uncaught exception of a callback, Driver::stop()): the next driver call
must then use the new one, whether the driver's own wait or the caller's suspend() saw the end.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$ns = 'mdrop_local.fiber_main_suspension';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_main_suspension']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
class_exists(EventLoop::class);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$slow = fn () => $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true'], ['limit' => 1]))->toArray();
var_dump($ping());

echo "an uncaught exception of a callback during a {main} call\n";
EventLoop::delay(0.05, function () { throw new Exception('boom'); });
try {
    $slow();
    echo "no exception\n";
} catch (Throwable $e) {
    echo get_class($e), ': ', $e->getPrevious()?->getMessage(), "\n";
}
var_dump($ping());

echo "Driver::stop() during a {main} call\n";
EventLoop::delay(0.05, function () { EventLoop::getDriver()->stop(); });
try {
    $slow();
    echo "no exception\n";
} catch (Throwable $e) {
    echo get_class($e), ': ', substr($e->getMessage(), 0, 61), "\n";
}
var_dump($ping());

echo "the caller's {main} suspension (the driver's own) ends without a resume\n";
$s = EventLoop::getSuspension();
EventLoop::delay(0.01, function () { throw new Exception('boom2'); });
try {
    $s->suspend();
} catch (Throwable $e) {
    echo get_class($e), ': ', $e->getPrevious()?->getMessage(), "\n";
}
var_dump($ping());
echo "a new suspension: ", var_export($s !== EventLoop::getSuspension(), true), "\n";

echo "the caller suspends {main} between driver calls\n";
$s = EventLoop::getSuspension();
EventLoop::delay(0.01, fn () => $s->resume(5));
var_dump($s->suspend());
var_dump($ping());
var_dump($ping());
echo "the same suspension: ", var_export($s === EventLoop::getSuspension(), true), "\n";
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
?>
--EXPECT--
float(1)
an uncaught exception of a callback during a {main} call
Revolt\EventLoop\UncaughtThrowable: boom
float(1)
Driver::stop() during a {main} call
Error: Event loop terminated without resuming the current suspension
float(1)
the caller's {main} suspension (the driver's own) ends without a resume
Revolt\EventLoop\UncaughtThrowable: boom2
float(1)
a new suspension: true
the caller suspends {main} between driver calls
int(5)
float(1)
float(1)
the same suspension: true
run() returned at once: true
