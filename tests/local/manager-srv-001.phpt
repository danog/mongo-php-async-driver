--TEST--
Manager: mongodb+srv:// seed lists from SRV and TXT records (the specification's public test records)
--DESCRIPTION--
Local test (not upstream). Resolves the initial-dns-seedlist records in test.build.10gen.cc
(they point to localhost:27017-27019, where nothing listens in these suites: the server
selection error shows the seed list). Same output as the reference.
--SKIPIF--
<?php if (!@dns_get_record('_mongodb._tcp.test1.test.build.10gen.cc', DNS_SRV)) { die('skip needs DNS for test.build.10gen.cc'); } ?>
<?php if (@fsockopen('127.0.0.1', 27017)) { die('skip something listens on localhost:27017'); } ?>
--FILE--
<?php
foreach ([
    'mongodb+srv://test14.test.build.10gen.cc/',
    'mongodb+srv://test5.test.build.10gen.cc/?serverSelectionTimeoutMS=200',
    'mongodb+srv://test6.test.build.10gen.cc/',
    'mongodb+srv://test8.test.build.10gen.cc/',
    'mongodb+srv://test3.test.build.10gen.cc/',
    'mongodb+srv://test1.test.build.10gen.cc/?srvMaxHosts=1&serverSelectionTimeoutMS=200',
] as $uri) {
    echo $uri, "\n";
    try {
        $m = new MongoDB\Driver\Manager($uri);
        try {
            $m->selectServer();
        } catch (MongoDB\Driver\Exception\Exception $e) {
            preg_match_all("/'([^']+):(\d+)'/", $e->getMessage(), $hosts);
            echo '  ', get_class($e), ': ', preg_replace('/\[.*\]/', '[...]', $e->getMessage()), "\n";
            echo '  seeds: ', str_contains($uri, 'srvMaxHosts') ? count(array_unique($hosts[0])) : implode(',', array_unique($hosts[0])), "\n";
        }
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo '  ', get_class($e), ': ', $e->getMessage(), "\n";
    }
}
?>
===DONE===
--EXPECT--
mongodb+srv://test14.test.build.10gen.cc/
  MongoDB\Driver\Exception\InvalidArgumentException: Failed to parse URI options: Invalid host "localhost.not-test.build.10gen.cc" returned for service "test14.test.build.10gen.cc": host must be subdomain of service name
mongodb+srv://test5.test.build.10gen.cc/?serverSelectionTimeoutMS=200
  MongoDB\Driver\Exception\ConnectionTimeoutException: No suitable servers found (`serverSelectionTryOnce` set): [...]. Topology type: Unknown
  seeds: 'localhost.test.build.10gen.cc:27017'
mongodb+srv://test6.test.build.10gen.cc/
  MongoDB\Driver\Exception\InvalidArgumentException: Failed to parse URI options: Multiple TXT records for "test6.test.build.10gen.cc"
mongodb+srv://test8.test.build.10gen.cc/
  MongoDB\Driver\Exception\InvalidArgumentException: Failed to parse URI options: URI option "authSource" contains no "=" sign
mongodb+srv://test3.test.build.10gen.cc/
  MongoDB\Driver\Exception\ConnectionTimeoutException: No suitable servers found (`serverSelectionTryOnce` set): [...]. Topology type: Unknown
  seeds: 'localhost.test.build.10gen.cc:27017'
mongodb+srv://test1.test.build.10gen.cc/?srvMaxHosts=1&serverSelectionTimeoutMS=200
  MongoDB\Driver\Exception\ConnectionTimeoutException: No suitable servers found (`serverSelectionTryOnce` set): [...]. Topology type: Unknown
  seeds: 1
===DONE===
