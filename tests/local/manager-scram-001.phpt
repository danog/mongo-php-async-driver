--TEST--
Manager: SCRAM-SHA-1 and SCRAM-SHA-256 authentication (negotiated, explicit, failures)
--DESCRIPTION--
Local test (not upstream), for a URI with SCRAM credentials whose user has both SCRAM-SHA-1 and
SCRAM-SHA-256 credentials (tools/topo.sh auth). Same results as ext-mongodb 2.5.6. The
authentication commands and the handshake's speculativeAuthenticate reply are never seen by
APM subscribers or in the server's hello response.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_not_auth(); ?>
<?php skip_if_not_auth_mechanism(null); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

class Names implements MongoDB\Driver\Monitoring\CommandSubscriber
{
    public $names = [];
    public function commandStarted($e): void { $this->names[] = $e->getCommandName(); }
    public function commandSucceeded($e): void {}
    public function commandFailed($e): void {}
}

$p = parse_url(URI);
$user = rawurldecode($p['user']);
$hostPort = $p['host'] . ':' . $p['port'];
$query = isset($p['query']) ? $p['query'] : '';
$uri = function ($password, $extra = '') use ($p, $hostPort, $query) {
    $q = trim($query . '&' . $extra, '&');
    return sprintf('mongodb://%s:%s@%s/%s', $p['user'], $password, $hostPort, $q === '' ? '' : '?' . $q);
};
$password = $p['pass'];
$cases = [
    'negotiated' => $uri($password),
    'SCRAM-SHA-1' => $uri($password, 'authMechanism=SCRAM-SHA-1'),
    'SCRAM-SHA-256' => $uri($password, 'authMechanism=SCRAM-SHA-256'),
    'wrong password' => $uri('wrong'),
    'wrong password SHA-1' => $uri('wrong', 'authMechanism=SCRAM-SHA-1'),
    'prohibited character' => $uri('a%07b'),
];
foreach ($cases as $name => $u) {
    $m = new MongoDB\Driver\Manager($u, ['directConnection' => true], ['disableClientPersistence' => true]);
    $s = new Names();
    $m->addSubscriber($s);
    try {
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['connectionStatus' => 1]))->toArray()[0];
        echo $name, ': ', $r->authInfo->authenticatedUsers[0]->user === $user ? 'authenticated' : 'other user', "\n";
        echo '  hello has speculativeAuthenticate: ', var_export(array_key_exists('speculativeAuthenticate', $m->getServers()[0]->getInfo()), true), "\n";
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo $name, ': ', get_class($e), ' (', $e->getCode(), '): ', $e->getMessage(), "\n";
    }
    echo '  commands seen: ', implode(',', $s->names), "\n";
}
?>
--EXPECT--
negotiated: authenticated
  hello has speculativeAuthenticate: false
  commands seen: connectionStatus
SCRAM-SHA-1: authenticated
  hello has speculativeAuthenticate: false
  commands seen: connectionStatus
SCRAM-SHA-256: authenticated
  hello has speculativeAuthenticate: false
  commands seen: connectionStatus
wrong password: MongoDB\Driver\Exception\AuthenticationException (11): Authentication failed.
  commands seen: 
wrong password SHA-1: MongoDB\Driver\Exception\AuthenticationException (11): Authentication failed.
  commands seen: 
prohibited character: MongoDB\Driver\Exception\RuntimeException (29): prohibited character included in password
  commands seen: 
