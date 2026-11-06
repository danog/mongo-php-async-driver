--TEST--
Manager: TLS options are accepted; the TLS driver options of ext-mongodb 1.x are refused, not ignored
--DESCRIPTION--
Local test (not upstream). ext-mongodb 2.x reads only "ca_dir" and "crl_file" among the driver
options and silently ignores the TLS keys of 1.x ("pem_file", "ca_file", "context", ...) and any
other tls*/ssl* key, so an application that meant to configure TLS that way gets an unverified
or cleartext connection. Here they are refused. Listed in DIVERGENCES.md (permanent, for safety).
--FILE--
<?php
foreach ([
    ['mongodb://127.0.0.1:28099/?tls=true', [], []],
    ['mongodb://127.0.0.1:28099/?ssl=true', [], []],
    ['mongodb://127.0.0.1:28099/', ['tls' => true], []],
    ['mongodb://127.0.0.1:28099/', ['tlsCAFile' => '/nonexistent'], []],
    ['mongodb://127.0.0.1:28099/', [], ['ca_dir' => '/nonexistent']],
    ['mongodb://127.0.0.1:28099/', [], ['crl_file' => '/nonexistent']],
    ['mongodb://127.0.0.1:28099/?tls=true', [], ['pem_file' => '/x.pem']],
    ['mongodb://127.0.0.1:28099/?tls=true', [], ['context' => null]],
    ['mongodb://127.0.0.1:28099/?tls=true', [], ['weak_cert_validation' => true]],
    ['mongodb://127.0.0.1:28099/', [], ['tlsCAFile' => '/x.pem']],
    ['mongodb://127.0.0.1:28099/', [], ['sslAllowInvalidCertificates' => true]],
] as [$uri, $o, $d]) {
    try {
        new MongoDB\Driver\Manager($uri, $o, $d);
        echo "accepted\n";
    } catch (MongoDB\Driver\Exception\InvalidArgumentException $e) {
        echo $e->getMessage(), "\n";
    }
}
?>
--EXPECT--
accepted
accepted
accepted
accepted
accepted
accepted
Unsupported driver option "pem_file": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the "ca_dir" and "crl_file" driver options
Unsupported driver option "context": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the "ca_dir" and "crl_file" driver options
Unsupported driver option "weak_cert_validation": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the "ca_dir" and "crl_file" driver options
Unsupported driver option "tlsCAFile": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the "ca_dir" and "crl_file" driver options
Unsupported driver option "sslAllowInvalidCertificates": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the "ca_dir" and "crl_file" driver options
