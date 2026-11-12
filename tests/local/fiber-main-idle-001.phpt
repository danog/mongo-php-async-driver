--TEST--
Fibers: a {main} call on an idle loop blocks as without a loop; any other work makes it run the loop
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Once the loop is in use, a
{main} call waits through it only when the loop has something else to do: an enabled callback
(referenced or not: timer, watcher, signal watcher, defer), something queued, or another wait of
the driver (a fiber's). With nothing else, the call blocks in its own send/receive exactly as
without a loop (no poll, no tick: the driver makes no deadline timer either). A disabled callback
is no work. Work present during the call is serviced on time: timers and signal watchers run,
fibers that become runnable resume and finish during the call.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$ns = 'mdrop_local.fiber_main_idle';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_main_idle']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$slow = fn () => $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true'], ['limit' => 1]))->toArray();
// the driver's callbacks: its deadline timer, made only by waits through the loop
$driverCallbacks = fn (array $before) => count(array_diff(EventLoop::getIdentifiers(), $before));

EventLoop::getDriver();
var_dump($ping());
echo "idle loop: driver callbacks: ", $driverCallbacks([]), "\n";

$t = EventLoop::delay(10, fn () => null);
EventLoop::disable($t);
var_dump($ping());
echo "a disabled timer only: driver callbacks: ", $driverCallbacks([$t]), "\n";
EventLoop::cancel($t);

// an unreferenced repeat timer: work; it runs during the call
$ticks = 0;
$r = EventLoop::unreference(EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; }));
$slow();
echo "unreferenced timer ran during the call: ", var_export($ticks >= 5, true), "\n";
echo "driver callbacks: ", var_export($driverCallbacks([$r]) >= 1, true), "\n";
EventLoop::cancel($r);
foreach (EventLoop::getIdentifiers() as $id) {
    EventLoop::cancel($id); // the driver's deadline timer: the loop is idle again
}
var_dump($ping());
echo "idle again: driver callbacks: ", $driverCallbacks([]), "\n";

// a fiber suspended on its Revolt suspension, resumed by a timer: never starved by {main}
$order = [];
$f = new Fiber(function () use (&$order) {
    $s = EventLoop::getSuspension();
    EventLoop::delay(0.05, fn () => $s->resume());
    $s->suspend();
    $order[] = 'fiber resumed';
});
$f->start();
$slow();
$order[] = '{main} call returned';
echo implode(', ', $order), "\n";

// a fiber waiting in the driver finishes during the {main} call
$f = new Fiber(function () use ($m, $ns, &$order) {
    $m->executeQuery($ns, new Query(['$where' => 'sleep(50) || true'], ['limit' => 1]))->toArray();
    $order[] = 'fiber call returned';
});
$order = [];
$f->start();
$slow();
$order[] = '{main} call returned';
echo implode(', ', $order), "\n";

// a signal watcher (Unix): work; the signal is handled during the call
if (DIRECTORY_SEPARATOR !== '\\') {
    $got = [];
    $sig = EventLoop::unreference(EventLoop::onSignal(PHP_OS_FAMILY === 'Darwin' ? 31 : 12, function () use (&$got) { $got[] = 'signal'; }));
    $p = proc_open(['sh', '-c', 'sleep 0.1; kill -' . (PHP_OS_FAMILY === 'Darwin' ? 31 : 12) . ' ' . getmypid()], [], $pipes);
    $slow();
    $got[] = '{main} call returned';
    proc_close($p);
    EventLoop::cancel($sig);
    echo implode(', ', $got), "\n";
} else {
    echo "signal, {main} call returned\n";
}
foreach (EventLoop::getIdentifiers() as $id) {
    EventLoop::cancel($id);
}
var_dump($ping());
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
?>
--EXPECT--
float(1)
idle loop: driver callbacks: 0
float(1)
a disabled timer only: driver callbacks: 0
unreferenced timer ran during the call: true
driver callbacks: true
float(1)
idle again: driver callbacks: 0
fiber resumed, {main} call returned
fiber call returned, {main} call returned
signal, {main} call returned
float(1)
run() returned at once: true
