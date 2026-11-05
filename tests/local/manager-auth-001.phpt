--TEST--
Manager: URIs with credentials fail closed while authentication is missing
--DESCRIPTION--
Local test (not upstream). Until SCRAM/X509/PLAIN land, a client that requires authentication
(a username or an authMechanism in the URI: libmongoc's cluster->requires_auth) never sends a
command unauthenticated; every command fails with an AuthenticationException, as with a
libmongoc built without the mechanism. Listed in DIVERGENCES.md (temporary gaps).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";
// the first seed of URI, alone
$seed = preg_replace('~^mongodb://(?:[^@/]*@)?([^,/?]+).*$~', '$1', URI);
foreach (["mongodb://u:p@%s/", "mongodb://%s/?authMechanism=MONGODB-X509"] as $f) {
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
MongoDB\Driver\Exception\AuthenticationException: The default authentication mechanism is not supported by this build yet
MongoDB\Driver\Exception\AuthenticationException: The MONGODB-X509 authentication mechanism is not supported by this build yet
