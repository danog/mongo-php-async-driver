--TEST--
Fibers: EventLoop::setDriver() while a fiber waits in the driver (its wait stays on its driver)
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Needs revolt/event-loop:
REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has revolt/event-loop>. A fiber
started from {main} suspends in a driver wait before any loop ran; Revolt then allows a driver
swap. As with a Revolt watcher of its own, the fiber's wait belongs to the driver it suspended
on: it does not keep the new driver's loop alive (run() returns at once, {main} calls and other
fibers work there), and it ends when that driver runs again. A fiber abandoned on a replaced
driver that never runs again stays suspended (its reply, delivered by whichever loop runs, here
a {main} call on the new driver, resumes it onto its own loop); unlike with a Revolt watcher of
its own, the cycle collector does not free it and its driver (the driver holds the suspension
while the fiber is suspended in it): they go at the end of the request (DIVERGENCES).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
use Revolt\EventLoop\Driver\StreamSelectDriver;
$ns = 'mdrop_local.fiber_set_driver_2';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_set_driver_2']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
class_exists(EventLoop::class);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$slow = fn () => $m->executeQuery($ns, new Query(['$where' => 'sleep(300) || true'], ['limit' => 1]))->toArray()[0]->x;
$runs = function (string $what, float $max = 0.1) {
    $t = hrtime(true);
    EventLoop::run();
    echo "$what: run() returned ", (hrtime(true) - $t) / 1e9 < $max ? "at once" : "late", "\n";
};
// the callbacks keeping a driver's loop alive
$alive = function ($d) {
    $n = 0;
    foreach ($d->getIdentifiers() as $id) {
        $n += $d->isEnabled($id) && $d->isReferenced($id);
    }
    return $n;
};

foreach (['back to the first driver', 'fiber destroyed'] as $case) {
    echo "-- $case\n";
    $first = EventLoop::getDriver();
    $f = new Fiber(function () use ($slow, $case) {
        try {
            return $slow();
        } finally {
            echo "$case: the fiber ends\n";
        }
    });
    $f->start();
    echo "fiber waits: ", var_export($f->isSuspended(), true), "\n";
    EventLoop::setDriver(new StreamSelectDriver);
    // {main} and fibers on the new driver while the first fiber's wait stays on the first one
    var_dump($ping());
    $done = 0;
    for ($i = 0; $i < 3; $i++) {
        EventLoop::queue(function () use ($ping, &$done) {
            $ping();
            $done++;
        });
    }
    var_dump($ping());
    $runs('new driver');
    echo "new driver: fibers done: $done; fiber still waits: ", var_export($f->isSuspended(), true), "\n";
    // one watcher; on Windows, also the wait's socket watchers
    echo "first driver keeps the loop alive for its wait: ", var_export($alive($first) > 0, true), "\n";
    $second = EventLoop::getDriver();
    if ($case === 'back to the first driver') {
        EventLoop::setDriver($first);
        $t = hrtime(true);
        EventLoop::run();
        echo "first driver: run() returned when the fiber ended: ", var_export($f->isTerminated(), true), ", result ", $f->getReturn(), "\n";
        var_dump($ping());
        $runs('first driver');
        echo "callbacks referenced on the first driver: ", $alive($first), "\n";
        // the second driver's state is retired when the first is current again: nothing waits there
        var_dump($ping());
    } else {
        $w = WeakReference::create($first);
        $wf = WeakReference::create($f);
        unset($f, $first);
        gc_collect_cycles();
        echo "the fiber lives while its wait is pending: ", var_export($wf->get() !== null, true), "\n";
        // the reply of the fiber's query arrives during this one's wait
        var_dump($slow());
        gc_collect_cycles();
        echo "the abandoned fiber and its driver are kept: ", var_export($w->get() !== null && $wf->get()?->isSuspended(), true), "\n";
        var_dump($ping());
        $runs('second driver');
    }
    echo "callbacks referenced on the current driver: ", $alive(EventLoop::getDriver()), "\n";
    $runs('end');
    EventLoop::setDriver(new StreamSelectDriver);
    unset($second);
}
?>
--EXPECT--
-- back to the first driver
fiber waits: true
float(1)
float(1)
new driver: run() returned at once
new driver: fibers done: 3; fiber still waits: true
first driver keeps the loop alive for its wait: true
back to the first driver: the fiber ends
first driver: run() returned when the fiber ended: true, result 1
float(1)
first driver: run() returned at once
callbacks referenced on the first driver: 0
float(1)
callbacks referenced on the current driver: 0
end: run() returned at once
-- fiber destroyed
fiber waits: true
float(1)
float(1)
new driver: run() returned at once
new driver: fibers done: 3; fiber still waits: true
first driver keeps the loop alive for its wait: true
the fiber lives while its wait is pending: true
int(1)
the abandoned fiber and its driver are kept: true
float(1)
second driver: run() returned at once
callbacks referenced on the current driver: 0
end: run() returned at once
fiber destroyed: the fiber ends
