--TEST--
Manager: UNIX domain socket hosts
--DESCRIPTION--
Local test (not upstream). Needs the standalone topology's UNIX domain socket
/tmp/mongodb-28010.sock (mongod's own; with Docker the relay that `tools/topo.sh start standalone`
starts). A host with a '/' and ".sock" is a socket path (percent-encoded in the URI); its server
is named by the path, with the default port. Connect and path errors carry libmongoc's messages,
without " calling hello on ..." (they happen while setting up the stream). Same output as the
reference.
--SKIPIF--
<?php if (!file_exists('/tmp/mongodb-28010.sock')) die('skip no /tmp/mongodb-28010.sock'); ?>
--FILE--
<?php
foreach ([
    'socket' => 'mongodb://%2Ftmp%2Fmongodb-28010.sock',
    'socket, database and options' => 'mongodb://%2Ftmp%2Fmongodb-28010.sock/admin?appname=x&serverSelectionTryOnce=true',
    'missing socket' => 'mongodb://%2Ftmp%2Fmdrop-missing-28010.sock/?serverSelectionTryOnce=true',
    'path too long' => 'mongodb://%2Ftmp%2F' . str_repeat('a', 100) . '.sock',
] as $name => $uri) {
    try {
        $m = new MongoDB\Driver\Manager($uri, [], ['disableClientPersistence' => true]);
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]))->toArray()[0];
        $s = $m->getServers()[0];
        echo "$name: ok=", $r->ok, ' host=', $s->getHost(), ' port=', $s->getPort(), ' type=', $s->getType(), "\n";
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo "$name: ", get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
    }
}
?>
--EXPECT--
socket: ok=1 host=/tmp/mongodb-28010.sock port=27017 type=1
socket, database and options: ok=1 host=/tmp/mongodb-28010.sock port=27017 type=1
missing socket: MongoDB\Driver\Exception\ConnectionTimeoutException 13053: No suitable servers found (`serverSelectionTryOnce` set): [Failed to connect to UNIX domain socket: No such file or directory]. Topology type: Single
path too long: MongoDB\Driver\Exception\ConnectionTimeoutException 13053: No suitable servers found (`serverSelectionTryOnce` set): [Failed to define socket address path.]. Topology type: Single
