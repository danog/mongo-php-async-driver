--TEST--
BSON encoding: a document larger than 2 GiB is refused while encoding; nothing is sent
--DESCRIPTION--
Local test (not upstream). As libbson's appends (phongo_bson_append_check): once a document would
exceed INT32_MAX bytes the encoder throws "Could not append BSON data for field path ...". BSON
and OP_MSG length fields are int32: a larger message would wrap them and the server would read
the remaining bytes as further messages. The client stays usable.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_not_enough_memory(0x80000000 * 2); ?>
--INI--
memory_limit=-1
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

$manager = create_test_manager();
$s = str_repeat('a', 1080000000);
try {
    $manager->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['insert' => COLLECTION_NAME, 'documents' => [['a' => $s, 'b' => $s]]]));
    echo "sent\n";
} catch (MongoDB\Driver\Exception\UnexpectedValueException $e) {
    echo $e->getMessage(), "\n";
}
unset($s);
var_dump($manager->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]))->toArray()[0]->ok);
?>
--EXPECT--
Could not append BSON data for field path "documents.0.b".
float(1)
