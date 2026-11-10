--TEST--
Fibers: EventLoop::setDriver() between driver calls (the next wait moves to the new driver)
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Needs revolt/event-loop:
REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has revolt/event-loop>. Revolt
allows a driver swap whenever its loop does not run (here: after EventLoop::run() returned).
The driver's watcher and timer live on the driver of the first wait; the first wait after a swap
cancels them on the old driver (when something still holds it) and makes them on the new one,
in {main} and in fibers alike. Nothing of the driver holds a replaced driver alive.
Afterwards nothing of the driver keeps either loop alive.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
use Revolt\EventLoop\Driver\StreamSelectDriver;
$ns = 'mdrop_local.fiber_set_driver';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_set_driver']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
class_exists(EventLoop::class);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$runs = function (string $what) {
    $t = hrtime(true);
    EventLoop::run();
    echo "$what: run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
};

var_dump($ping());
$runs('first driver');
$old = EventLoop::getDriver();
// the watcher, the deadline timer, the (disabled) keeper of {main}'s suspension (not with
// REVOLT_DRIVER_DEBUG_TRACE: a TracingDriver is not an AbstractDriver)
echo "callbacks on the first driver: ", count($old->getIdentifiers()), "\n";

foreach (['{main}', 'fiber'] as $where) {
    EventLoop::setDriver(new StreamSelectDriver);
    if ($where === '{main}') {
        // the loop of the new driver runs while {main} waits
        $ticks = 0;
        $timer = EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; });
        $m->executeQuery($ns, new Query(['$where' => 'sleep(200) || true'], ['limit' => 1]))->toArray();
        echo "$where: the new driver ran during the call: ", var_export($ticks >= 1, true), "\n";
        EventLoop::cancel($timer);
        var_dump($ping());
    } else {
        $done = 0;
        for ($i = 0; $i < 3; $i++) {
            EventLoop::queue(function () use ($ping, &$done) {
                $ping();
                $ping();
                $done++;
            });
        }
        EventLoop::run();
        echo "$where: fibers done: $done\n";
    }
    echo "$where: callbacks left on the old driver: ", count($old->getIdentifiers()), "\n";
    $runs($where);
    $old = EventLoop::getDriver();
}
// one more swap, then a {main} call: the replaced driver is freed by the swap as before (the
// driver holds nothing of it alive)
var_dump($ping());
unset($old);
$w = WeakReference::create(EventLoop::getDriver());
EventLoop::run();
EventLoop::setDriver(new StreamSelectDriver);
echo "the replaced driver is gone: ", var_export($w->get() === null, true), "\n";
var_dump($ping());
$runs('last driver');
?>
--EXPECTF--
float(1)
first driver: run() returned at once: true
callbacks on the first driver: %d
{main}: the new driver ran during the call: true
float(1)
{main}: callbacks left on the old driver: 0
{main}: run() returned at once: true
fiber: fibers done: 3
fiber: callbacks left on the old driver: 0
fiber: run() returned at once: true
float(1)
the replaced driver is gone: true
float(1)
last driver: run() returned at once: true
