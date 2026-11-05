--TEST--
Manager: TLS requests fail closed while the TLS transport is missing
--FILE--
<?php
foreach ([
    ['mongodb://127.0.0.1:28099/?tls=true', [], []],
    ['mongodb://127.0.0.1:28099/?ssl=true', [], []],
    ['mongodb://127.0.0.1:28099/', ['tls' => true], []],
    ['mongodb://127.0.0.1:28099/', ['tlsCAFile' => '/nonexistent'], []],
    ['mongodb://127.0.0.1:28099/', [], ['ca_dir' => '/nonexistent']],
] as [$uri, $o, $d]) {
    try {
        new MongoDB\Driver\Manager($uri, $o, $d);
        echo "accepted\n";
    } catch (MongoDB\Driver\Exception\InvalidArgumentException $e) {
        echo $e->getMessage(), "\n";
    }
}
new MongoDB\Driver\Manager('mongodb://127.0.0.1:28099/?tls=false');
echo "tls=false ok\n";
?>
--EXPECT--
Failed to parse URI options: Can't create SSL client, SSL not enabled in this build.
Failed to parse URI options: Can't create SSL client, SSL not enabled in this build.
Failed to parse URI options: Can't create SSL client, SSL not enabled in this build.
Failed to parse URI options: Can't create SSL client, SSL not enabled in this build.
Failed to parse URI options: Can't create SSL client, SSL not enabled in this build.
tls=false ok
