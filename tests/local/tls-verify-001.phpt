--TEST--
TLS: certificate, host name and CRL verification, client certificates and encrypted keys
--DESCRIPTION--
Local test (not upstream). Needs `tools/topo.sh start tls` (mongod with --tlsMode requireTLS on
28016, and on 28017 with a certificate for 127.0.0.2 only) and SSL_DIR = the output directory
of tools/tls-certs.sh. Verification is on by default; each insecure option only turns off what
it names. Failure texts differ from the reference, which reports most failed TLS server checks
as "connection closed" or "connection error" (see DIVERGENCES.md).
--SKIPIF--
<?php if (!getenv('SSL_DIR') || !is_file(getenv('SSL_DIR') . '/ca.pem')) die('skip SSL_DIR not set'); ?>
<?php if (!@fsockopen('127.0.0.1', 28016)) die('skip no TLS topology on 28016'); ?>
--FILE--
<?php
$d = getenv('SSL_DIR');
$cases = [
    'ca' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem"], []],
    'ca, IP address' => ['mongodb://127.0.0.1:28016/?ssl=true', ['tlsCAFile' => "$d/ca.pem"], []],
    'other ca' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/other-ca.pem"], []],
    'missing ca file' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/nope.pem"], []],
    'name mismatch' => ['mongodb://127.0.0.1:28017/?tls=true', ['tlsCAFile' => "$d/ca.pem"], []],
    'name mismatch, tlsAllowInvalidHostnames' => ['mongodb://127.0.0.1:28017/?tls=true', ['tlsCAFile' => "$d/ca.pem", 'tlsAllowInvalidHostnames' => true], []],
    'other ca, tlsAllowInvalidHostnames' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/other-ca.pem", 'tlsAllowInvalidHostnames' => true], []],
    'other ca, tlsAllowInvalidCertificates' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/other-ca.pem", 'tlsAllowInvalidCertificates' => true], []],
    'tlsInsecure' => ['mongodb://127.0.0.1:28017/?tls=true&tlsInsecure=true', [], []],
    'client certificate' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem", 'tlsCertificateKeyFile' => "$d/client.pem"], []],
    'encrypted PKCS#8 key' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem", 'tlsCertificateKeyFile' => "$d/client-pkcs8-enc.pem", 'tlsCertificateKeyFilePassword' => 'qwerty'], []],
    'legacy encrypted key' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem", 'tlsCertificateKeyFile' => "$d/client-legacy-enc.pem", 'tlsCertificateKeyFilePassword' => 'qwerty'], []],
    'wrong key password' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem", 'tlsCertificateKeyFile' => "$d/client-pkcs8-enc.pem", 'tlsCertificateKeyFilePassword' => 'nope'], []],
    'crl, not revoked' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem"], ['crl_file' => "$d/crl-empty.pem"]],
    'crl, revoked' => ['mongodb://localhost:28016/?tls=true', ['tlsCAFile' => "$d/ca.pem"], ['crl_file' => "$d/crl-revoked.pem"]],
    'ca_dir (hashed)' => ['mongodb://localhost:28016/?tls=true', [], ['ca_dir' => "$d/hashed"]],
    'ca_dir alone enables TLS' => ['mongodb://localhost:28016/', [], ['ca_dir' => "$d/hashed"]],
];
foreach ($cases as $name => [$uri, $o, $do]) {
    $o += ['serverSelectionTimeoutMS' => 2000, 'connectTimeoutMS' => 1000];
    try {
        $m = new MongoDB\Driver\Manager($uri, $o, $do + ['disableClientPersistence' => true]);
        $r = $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]))->toArray()[0];
        echo "$name: ok\n";
    } catch (MongoDB\Driver\Exception\ConnectionTimeoutException $e) {
        echo "$name: ", preg_replace('~.*\[(.*) calling hello.*~', '$1', $e->getMessage()), "\n";
    }
}
?>
--EXPECTF--
ca: ok
ca, IP address: ok
other ca: TLS handshake failed: certificate verify failed (20): unable to get local issuer certificate
missing ca file: TLS handshake failed: Cannot load Certificate Authorities from '%s/nope.pem' and '(null)'
name mismatch: TLS handshake failed: certificate verify failed (64): IP address mismatch
name mismatch, tlsAllowInvalidHostnames: ok
other ca, tlsAllowInvalidHostnames: TLS handshake failed: certificate verify failed (20): unable to get local issuer certificate
other ca, tlsAllowInvalidCertificates: ok
tlsInsecure: ok
client certificate: ok
encrypted PKCS#8 key: ok
legacy encrypted key: ok
wrong key password: TLS handshake failed: Cannot find private key in: '%s/client-pkcs8-enc.pem'
crl, not revoked: ok
crl, revoked: TLS handshake failed: certificate verify failed (23): certificate revoked
ca_dir (hashed): ok
ca_dir alone enables TLS: ok
