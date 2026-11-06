--TEST--
Fibers: a driver call inside a Fiber without revolt/event-loop throws
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Inside a fiber the driver
suspends the fiber through Revolt\EventLoop while it waits for the network; without that class
the call throws a RuntimeException before any I/O. The same call in {main} is the synchronous one.
The reference blocks inside the fiber instead (no output difference for the {main} line).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (class_exists('Revolt\EventLoop')) die('skip Revolt is loaded'); ?>
--FILE--
<?php
$m = new MongoDB\Driver\Manager(getenv('MONGODB_URI'));
$f = new Fiber(function () use ($m) {
    try {
        $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
        echo "no exception\n";
    } catch (MongoDB\Driver\Exception\RuntimeException $e) {
        echo get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
    }
});
$f->start();
var_dump($m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]))->toArray()[0]->ok);
?>
--EXPECT--
MongoDB\Driver\Exception\RuntimeException 7: MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\EventLoop was not found
float(1)
