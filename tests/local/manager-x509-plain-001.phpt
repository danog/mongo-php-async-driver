--TEST--
Manager: MONGODB-X509 and PLAIN authentication
--DESCRIPTION--
Local test (not upstream). Needs `tools/topo.sh start x509` (mongod with --auth and --tlsMode
requireTLS on 28014, mechanisms MONGODB-X509, PLAIN and SCRAM; the client.pem subject is a user
of $external) and SSL_DIR = the output directory of tools/tls-certs.sh. X509 authenticates with
the client certificate, with or without the user name in the URI (without it the server takes
the name from the certificate); the handshake carries a speculative X509 authentication. PLAIN
is sent as libmongoc sends it (saslStart on $external whatever the authSource); a community
mongod has no PLAIN users (that needs LDAP), so its answer is the server's error. The PLAIN
payload is bounded as libmongoc's 4096-byte base64 buffer bounds it. Same output as the reference.
--SKIPIF--
<?php if (!getenv('SSL_DIR') || !is_file(getenv('SSL_DIR') . '/client.pem')) die('skip SSL_DIR not set'); ?>
<?php if (!@fsockopen('127.0.0.1', 28014)) die('skip no x509 topology on 28014'); ?>
--FILE--
<?php
$d = getenv('SSL_DIR');
$tls = "tls=true&tlsCAFile=$d/ca.pem&directConnection=true";
$cert = "&tlsCertificateKeyFile=$d/client.pem";
$cases = [
    'x509, no user name' => "mongodb://127.0.0.1:28014/?authMechanism=MONGODB-X509&$tls$cert",
    'x509, user name' => "mongodb://" . rawurlencode('CN=client,OU=drivers,O=mdrop-client') . "@127.0.0.1:28014/?authMechanism=MONGODB-X509&$tls$cert",
    'x509, authSource' => "mongodb://127.0.0.1:28014/?authMechanism=MONGODB-X509&authSource=\$external&$tls$cert",
    'x509, wrong user name' => "mongodb://nobody@127.0.0.1:28014/?authMechanism=MONGODB-X509&$tls$cert",
    'x509, no certificate' => "mongodb://127.0.0.1:28014/?authMechanism=MONGODB-X509&$tls",
    'plain' => "mongodb://root:toor@127.0.0.1:28014/?authMechanism=PLAIN&$tls",
    'plain, authSource admin' => "mongodb://root:toor@127.0.0.1:28014/?authMechanism=PLAIN&authSource=admin&$tls",
    'plain, 3066-byte password' => "mongodb://u:" . str_repeat('x', 3066) . "@127.0.0.1:28014/?authMechanism=PLAIN&$tls",
    'plain, 3067-byte password' => "mongodb://u:" . str_repeat('x', 3067) . "@127.0.0.1:28014/?authMechanism=PLAIN&$tls",
    'scram' => "mongodb://root:toor@127.0.0.1:28014/?$tls",
];
foreach ($cases as $name => $uri) {
    $m = new MongoDB\Driver\Manager($uri, [], ['disableClientPersistence' => true]);
    try {
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['connectionStatus' => 1]))->toArray()[0];
        echo "$name: ", json_encode($r->authInfo->authenticatedUsers), "\n";
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo "$name: ", get_class($e), ' ', $e->getCode(), ': ', $e->getMessage(), "\n";
    }
}
?>
--EXPECT--
x509, no user name: [{"user":"CN=client,OU=drivers,O=mdrop-client","db":"$external"}]
x509, user name: [{"user":"CN=client,OU=drivers,O=mdrop-client","db":"$external"}]
x509, authSource: [{"user":"CN=client,OU=drivers,O=mdrop-client","db":"$external"}]
x509, wrong user name: MongoDB\Driver\Exception\AuthenticationException 11: Authentication failed.
x509, no certificate: MongoDB\Driver\Exception\AuthenticationException 11: Authentication failed.
plain: MongoDB\Driver\Exception\AuthenticationException 11: Unsupported mechanism 'PLAIN' on authentication database '$external'
plain, authSource admin: MongoDB\Driver\Exception\AuthenticationException 11: Unsupported mechanism 'PLAIN' on authentication database '$external'
plain, 3066-byte password: MongoDB\Driver\Exception\AuthenticationException 11: Unsupported mechanism 'PLAIN' on authentication database '$external'
plain, 3067-byte password: MongoDB\Driver\Exception\AuthenticationException 11: failed base64 encoding message
scram: [{"user":"root","db":"admin"}]
