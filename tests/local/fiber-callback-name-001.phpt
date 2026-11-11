--TEST--
Fibers: the driver's event-loop callbacks are closures whose name does not end in '}'
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Needs revolt/event-loop:
REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has revolt/event-loop>. The driver
hands Revolt closures of internal functions. Xdebug (3.5, PHP >= 8.4) takes a closure whose name
ends in '}' for a user closure and formats its op_array's file name, which an internal function
does not have: a segfault on the first callback run. With Xdebug loaded this test runs that path
too (the fiber's wait runs the watcher callback).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
<?php if (extension_loaded('revolt')) die('skip the revolt extension declares Revolt\EventLoop'); ?>
--INI--
mongodb.native_event_loop=0
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command};
use Revolt\EventLoop;
use Revolt\EventLoop\Internal\AbstractDriver;
$m = new Manager(getenv('MONGODB_URI'));
class_exists(EventLoop::class);
$ping = fn () => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$names = [];
EventLoop::queue(function () use ($ping, &$names) {
    var_dump($ping());
    $callbacks = (new ReflectionProperty(AbstractDriver::class, 'callbacks'))->getValue(EventLoop::getDriver());
    foreach ($callbacks as $callback) {
        $function = new ReflectionFunction($callback->closure);
        if ($function->isInternal()) {
            $names[$function->getName()] = true;
        }
    }
});
EventLoop::run();
var_dump(array_keys($names));
?>
--EXPECT--
float(1)
array(1) {
  [0]=>
  string(27) "mongodb_event_loop_callback"
}
