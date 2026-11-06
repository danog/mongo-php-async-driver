--TEST--
Manager: GSSAPI, MONGODB-AWS and MONGODB-OIDC fail closed while they are missing
--DESCRIPTION--
Local test (not upstream). SCRAM-SHA-1, SCRAM-SHA-256, PLAIN and MONGODB-X509 are implemented;
until GSSAPI, MONGODB-AWS and MONGODB-OIDC land, a client that must authenticate with one of them
never sends a command unauthenticated: every command fails with an
AuthenticationException, as with a libmongoc built without the mechanism. Listed in
DIVERGENCES.md (temporary gaps).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";
// the first seed of URI, alone
$seed = preg_replace('~^mongodb://(?:[^@/]*@)?([^,/?]+).*$~', '$1', URI);
foreach ([
    "mongodb://u@%s/?authMechanism=GSSAPI",
    "mongodb://%s/?authMechanism=MONGODB-AWS",
    "mongodb://%s/?authMechanism=MONGODB-OIDC&authMechanismProperties=ENVIRONMENT:test",
] as $f) {
    $m = new MongoDB\Driver\Manager(sprintf($f, $seed), ['directConnection' => true], ['disableClientPersistence' => true]);
    try {
        $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
        echo "sent\n";
    } catch (MongoDB\Driver\Exception\AuthenticationException $e) {
        echo get_class($e), ': ', $e->getMessage(), "\n";
    }
}
?>
--EXPECT--
MongoDB\Driver\Exception\AuthenticationException: The GSSAPI authentication mechanism is not supported by this build yet
MongoDB\Driver\Exception\AuthenticationException: The MONGODB-AWS authentication mechanism is not supported by this build yet
MongoDB\Driver\Exception\AuthenticationException: The MONGODB-OIDC authentication mechanism is not supported by this build yet
