--TEST--
Manager: BSON-typed uriOptions (authMechanismProperties, readPreferenceTags) and an invalid "driver" option
--DESCRIPTION--
Local test (not upstream). phongo hands authMechanismProperties and readPreferenceTags from
uriOptions to libmongoc as BSON documents, so their values keep their types: a property that is
not a string is refused by type when the URI is finalized, and tag sets come back from
getReadPreference() as given. An invalid "driver" option throws, but phongo still creates and
stores the client: a second Manager with the same arguments finds the persistent client and does
not throw. Same output as the reference, which then crashes at shutdown (SIGSEGV) after the
non-array "driver" option (DIVERGENCES.md).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";
$cases = [
    ['mongodb://127.0.0.1/?authMechanism=MONGODB-AWS', ['authMechanismProperties' => ['AWS_SESSION_TOKEN' => 1]]],
    ['mongodb://u@127.0.0.1/?authMechanism=GSSAPI', ['authMechanismProperties' => ['SERVICE_NAME' => 'x', 'CANONICALIZE_HOST_NAME' => true]]],
    ['mongodb://127.0.0.1/?authMechanism=MONGODB-OIDC', ['authMechanismProperties' => ['ENVIRONMENT' => ['azure']]]],
    ['mongodb://127.0.0.1/?authMechanism=MONGODB-OIDC', ['authMechanismProperties' => ['ENVIRONMENT' => 1.5, 'NOPE' => 1]]],
    ['mongodb://127.0.0.1/?authMechanism=MONGODB-AWS', ['authMechanismProperties' => ['AWS_SESSION_TOKEN' => null]]],
    ['mongodb://127.0.0.1/?authMechanism=MONGODB-AWS', ['authMechanismProperties' => ['AWS_SESSION_TOKEN' => 'token']]],
];
foreach ($cases as [$uri, $options]) {
    try {
        new MongoDB\Driver\Manager($uri, $options);
        echo "ok\n";
    } catch (MongoDB\Driver\Exception\InvalidArgumentException $e) {
        echo $e->getMessage(), "\n";
    }
}

$m = new MongoDB\Driver\Manager('mongodb://127.0.0.1/?readPreference=secondaryPreferred', ['readPreferenceTags' => [['dc' => 1, 'rack' => true, 'zone' => 'a'], []]]);
var_dump($m->getReadPreference()->getTagSets());

foreach ([['driver' => 'x'], ['driver' => ['name' => 1]]] as $driverOptions) {
    for ($i = 0; $i < 2; $i++) {
        try {
            new MongoDB\Driver\Manager('mongodb://127.0.0.1:27999/?appname=typed-001', [], $driverOptions);
            echo "created\n";
        } catch (MongoDB\Driver\Exception\InvalidArgumentException $e) {
            echo $e->getMessage(), "\n";
        }
    }
}
?>
--EXPECT--
Failed to parse URI options: 'AWS_SESSION_TOKEN' authentication mechanism property 'MONGODB-AWS' has incorrect type 'INT32', should be 'UTF8'
ok
Failed to parse URI options: 'ENVIRONMENT' authentication mechanism property 'MONGODB-OIDC' has incorrect type 'ARRAY', should be 'UTF8'
Failed to parse URI options: 'ENVIRONMENT' authentication mechanism property 'MONGODB-OIDC' has incorrect type 'DOUBLE', should be 'UTF8'
Failed to parse URI options: 'AWS_SESSION_TOKEN' authentication mechanism property 'MONGODB-AWS' has incorrect type 'NULL', should be 'UTF8'
ok
array(2) {
  [0]=>
  array(3) {
    ["dc"]=>
    int(1)
    ["rack"]=>
    bool(true)
    ["zone"]=>
    string(1) "a"
  }
  [1]=>
  array(0) {
  }
}
Expected "driver" driver option to be an array, string given
created
Expected "name" handshake option to be a string, int given
created
