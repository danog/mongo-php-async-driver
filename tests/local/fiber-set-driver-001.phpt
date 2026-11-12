--TEST--
Fibers: EventLoop::setDriver() is refused; the driver's waits stay on the loop's native driver
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). The driver's sockets live
in the poller of the loop's native driver (revolt-rs), so that driver can never be replaced:
EventLoop::setDriver() throws UnsupportedFeatureException and changes nothing, before and after
driver calls, in {main} and in fibers, also while fibers wait in the driver. Afterwards nothing
of the driver keeps the loop alive.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
use Revolt\EventLoop\Driver\StreamSelectDriver;
use Revolt\EventLoop\UnsupportedFeatureException;
$ns = 'mdrop_local.fiber_set_driver';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_set_driver']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$driver = EventLoop::getDriver();
$swap = function (string $when) use ($driver) {
    try {
        EventLoop::setDriver(new StreamSelectDriver);
        echo "$when: swapped\n";
    } catch (UnsupportedFeatureException $e) {
        echo "$when: refused, same driver: ", var_export(EventLoop::getDriver() === $driver, true), "\n";
    }
};

var_dump($ping());
$swap('after a {main} call');
$done = 0;
for ($i = 0; $i < 3; $i++) {
    EventLoop::queue(function () use ($m, $ns, $ping, &$done) {
        $m->executeQuery($ns, new Query(['$where' => 'sleep(100) || true'], ['limit' => 1]))->toArray();
        $ping();
        $done++;
    });
}
// while the fibers wait in the driver
EventLoop::delay(0.05, fn () => $swap('while fibers wait'));
EventLoop::run();
echo "fibers done: $done\n";
var_dump($ping());
$t = hrtime(true);
EventLoop::run();
echo "run() returned at once: ", var_export((hrtime(true) - $t) < 1e9, true), "\n";
?>
--EXPECT--
float(1)
after a {main} call: refused, same driver: true
while fibers wait: refused, same driver: true
fibers done: 3
float(1)
run() returned at once: true
