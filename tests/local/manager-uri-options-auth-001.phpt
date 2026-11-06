--TEST--
Manager: credentials in uriOptions authenticate, and never share a client with other credentials
--DESCRIPTION--
Local test (not upstream), for an auth URI whose user has SCRAM credentials (tools/topo.sh
auth). The username and password come from the uriOptions array (phongo applies them with
mongoc_uri_set_username/password). Persistent clients are keyed by phongo's client hash, which
covers the options: a Manager with a wrong password does not reuse the authenticated client
of an earlier Manager for the same URI string. Same results as ext-mongodb 2.5.6.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_not_auth(); ?>
<?php skip_if_not_auth_mechanism(null); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

$p = parse_url(URI);
$uri = sprintf('mongodb://%s:%d/?directConnection=true', $p['host'], $p['port']);
$ping = new MongoDB\Driver\Command(['ping' => 1]);
foreach ([rawurldecode($p['pass']), 'wrong', rawurldecode($p['pass'])] as $password) {
    $m = new MongoDB\Driver\Manager($uri, ['username' => rawurldecode($p['user']), 'password' => $password, 'authSource' => 'admin']);
    try {
        $m->executeCommand('admin', $ping);
        echo "authenticated\n";
    } catch (MongoDB\Driver\Exception\AuthenticationException $e) {
        echo get_class($e), ': ', $e->getMessage(), "\n";
    }
}
echo throws(function () use ($uri) {
    new MongoDB\Driver\Manager($uri, ['username' => 1]);
}, 'MongoDB\Driver\Exception\InvalidArgumentException'), "\n";
?>
--EXPECTF--
authenticated
MongoDB\Driver\Exception\AuthenticationException: Authentication failed.
authenticated
OK: Got MongoDB\Driver\Exception\InvalidArgumentException
Expected string for "username" URI option, 32-bit integer given
