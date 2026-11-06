--TEST--
Exception codes: libmongoc's error codes after its enum restarts at MAX_TIME_MS_EXPIRED = 50
--DESCRIPTION--
Local test (not upstream). The codes of client-side errors are libmongoc's mongoc_error_code_t
values: TRANSACTION_INVALID_STATE is 53 and CLIENT_INVALID_LOAD_BALANCER is 63 (the enum
continues from MAX_TIME_MS_EXPIRED = 50). loadBalanced=true against a server that is not behind
a load balancer fails as libmongoc does. Same output as the reference.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_load_balanced(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

$m = create_test_manager();
$s = $m->startSession();
$s->startTransaction();
echo throws(fn () => $s->startTransaction(), MongoDB\Driver\Exception\RuntimeException::class), "\n";
try {
    $s->startTransaction();
} catch (MongoDB\Driver\Exception\RuntimeException $e) {
    var_dump($e->getCode());
}
$s->abortTransaction();

$server = $m->selectServer();
$lb = create_test_manager(sprintf('mongodb://%s:%d/?loadBalanced=true', $server->getHost(), $server->getPort()));
try {
    $lb->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
} catch (MongoDB\Driver\Exception\RuntimeException $e) {
    echo get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
}
?>
===DONE===
<?php exit(0); ?>
--EXPECT--
OK: Got MongoDB\Driver\Exception\RuntimeException
Transaction already in progress
int(53)
MongoDB\Driver\Exception\RuntimeException 63: Driver attempted to initialize in load balancing mode, but the server does not support this mode.
===DONE===
