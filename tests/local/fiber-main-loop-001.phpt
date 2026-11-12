--TEST--
Fibers: with Revolt loaded, a driver call in {main} runs the event loop while it waits
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Once the
class Revolt\EventLoop is loaded, every wait of a {main} call suspends {main} through Revolt
(EventLoop::getSuspension()->suspend() runs the loop): timers and queued fibers progress during
the call. With an idle loop the call simply completes, and nothing of the driver keeps the loop
alive afterwards.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$ns = 'mdrop_local.fiber_main_loop';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_main_loop']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
var_dump(class_exists(EventLoop::class)); // loaded from here on

$ticks = 0;
$timer = EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; });
$fiberDone = false;
EventLoop::queue(function () use ($m, $ns, &$fiberDone) {
    $m->executeQuery($ns, new Query(['$where' => 'sleep(100) || true'], ['limit' => 1]))->toArray();
    $fiberDone = true;
});
// {main}'s find outlasts the fibers' by 800 ms or more: under parallel test load, mongod's
// execution admission queue (read tickets) was seen holding a fiber's find for up to 400 ms
$m->executeQuery($ns, new Query(['$where' => 'sleep(1000) || true'], ['limit' => 1]))->toArray();
echo "timer ran during the {main} call: ", var_export($ticks >= 1, true), "\n";
echo "queued fiber done during the {main} call: ", var_export($fiberDone, true), "\n";
EventLoop::cancel($timer);

// an idle loop: the call completes; afterwards the loop has nothing referenced left
var_dump($m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok);
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
?>
--EXPECT--
bool(true)
timer ran during the {main} call: true
queued fiber done during the {main} call: true
float(1)
run() returned at once: true
