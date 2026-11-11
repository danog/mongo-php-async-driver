--TEST--
Fibers: a driver call inside a Fiber without revolt/event-loop throws
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Inside a fiber the driver
suspends the fiber through Revolt\EventLoop while it waits for the network; without that class
the call throws a RuntimeException (code 0) before any I/O, for every kind of operation. The same
call in {main} is the synchronous one. The class is missing only with mongodb.native_event_loop=0
(else the extension registers it), without the PHP library and the revolt extension.
The reference blocks inside the fiber instead (no output difference for the {main} line).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (extension_loaded('revolt')) die('skip the revolt extension declares Revolt\EventLoop'); ?>
--INI--
mongodb.native_event_loop=0
--FILE--
<?php
$m = new MongoDB\Driver\Manager(getenv('MONGODB_URI'));
$f = new Fiber(function () use ($m) {
    $bulk = new MongoDB\Driver\BulkWrite();
    $bulk->insert(['x' => 1]);
    $ops = [
        fn () => $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1])),
        fn () => $m->executeQuery('test.fiber_no_event_loop', new MongoDB\Driver\Query([])),
        fn () => $m->executeBulkWrite('test.fiber_no_event_loop', $bulk),
        fn () => $m->selectServer(),
    ];
    foreach ($ops as $op) {
        try {
            $op();
            echo "no exception\n";
        } catch (MongoDB\Driver\Exception\RuntimeException $e) {
            echo get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
        }
    }
});
$f->start();
var_dump($m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]))->toArray()[0]->ok);
?>
--EXPECT--
MongoDB\Driver\Exception\RuntimeException 0: MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\EventLoop was not found
MongoDB\Driver\Exception\RuntimeException 0: MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\EventLoop was not found
MongoDB\Driver\Exception\RuntimeException 0: MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\EventLoop was not found
MongoDB\Driver\Exception\RuntimeException 0: MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\EventLoop was not found
float(1)
