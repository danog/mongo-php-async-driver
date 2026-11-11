--TEST--
Fibers: the native Revolt event loop (revolt-rs, embedded or the revolt extension) is one loop, used once EventLoop is
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). With
mongodb.native_event_loop=1 (the default) the extension registers the Revolt classes at MINIT
(revolt-rs, revolt/event-loop 1.0.9) unless another copy of revolt-rs (the revolt extension)
did: either way there is one loop per thread, the one Revolt\EventLoop shows. The classes exist
from the start, so {main} calls wait through the loop once it is in use (its driver exists),
and block before. Fibers suspend through the native loop; a TracingDriver (not a native driver)
is reached through PHP calls, as the PHP library is.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!class_exists('Revolt\EventLoop', false) || !(new ReflectionClass('Revolt\EventLoop'))->isInternal()) die('skip Revolt\EventLoop is not native'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
use Revolt\EventLoop\Driver\TracingDriver;
$owner = (new ReflectionClass(EventLoop::class))->getExtensionName();
echo "classes of mongodb (or revolt): ", var_export($owner === 'mongodb' || ($owner === 'revolt' && extension_loaded('revolt')), true), "\n";
$ns = 'mdrop_local.fiber_native';
$m = new Manager(getenv('MONGODB_URI'));
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;

// no driver yet: the {main} call blocks, nothing of the driver is on the loop made afterwards
var_dump($ping());
echo "callbacks after a {main} call before the loop was used: ", count(EventLoop::getIdentifiers()), "\n";
// the loop exists now: the next {main} call waits through it (its watcher stays, unreferenced)
var_dump($ping());
echo "callbacks after a {main} call through the loop: ", var_export(count(EventLoop::getIdentifiers()) >= 1, true), "\n";

$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_native']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
$ticks = 0;
$timer = EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; });
$done = 0;
for ($i = 0; $i < 4; $i++) {
    EventLoop::queue(function () use ($m, $ns, &$done) {
        $m->executeQuery($ns, new Query(['$where' => 'sleep(100) || true'], ['limit' => 1]))->toArray();
        $done++;
    });
}
$m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true'], ['limit' => 1]))->toArray();
echo "timer ran during the {main} call: ", var_export($ticks >= 1, true), "\n";
echo "fibers done during the {main} call: $done\n";
EventLoop::cancel($timer);
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";

// a TracingDriver over a native one: not native itself, reached through PHP calls
EventLoop::setDriver(new TracingDriver(EventLoop::getDriver()));
$done = 0;
for ($i = 0; $i < 3; $i++) {
    EventLoop::queue(function () use ($ping, &$done) {
        $ping();
        $done++;
    });
}
var_dump($ping());
EventLoop::run();
echo "fibers done on the TracingDriver: $done\n";
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
?>
--EXPECT--
classes of mongodb (or revolt): true
float(1)
callbacks after a {main} call before the loop was used: 0
float(1)
callbacks after a {main} call through the loop: true
timer ran during the {main} call: true
fibers done during the {main} call: 4
run() returned at once: true
float(1)
fibers done on the TracingDriver: 3
run() returned at once: true
