--TEST--
Manager: debug output and URI errors never show the password or other secrets
--DESCRIPTION--
Local test (not upstream). ext-mongodb shows the URI as given in var_dump($manager) and in
"Failed to parse MongoDB URI" messages, password included. Here the password, the secret
options (tlsCertificateKeyFilePassword, sslClientCertificateKeyPassword) and an
authMechanismProperties value with a secret property (such as AWS_SESSION_TOKEN) are replaced
by "******"; the username is kept. Listed in DIVERGENCES.md (permanent, for safety).
--FILE--
<?php
$m = new MongoDB\Driver\Manager('mongodb://AKID:s3cr3t@127.0.0.1:1/?authMechanism=MONGODB-AWS&authMechanismProperties=AWS_SESSION_TOKEN:t0ken&appname=x');
$dump = print_r($m, true);
echo preg_replace('~\s+~', ' ', $dump), "\n";
var_dump(strpos($dump, 's3cr3t'), strpos($dump, 't0ken'));
$m = new MongoDB\Driver\Manager('mongodb://user:s3cr3t@127.0.0.1:1/admin');
var_dump(strpos(var_export($m, true), 's3cr3t'), strpos(json_encode((array) $m), 's3cr3t'));
// '@' in appname: the URI is split at the first '@', as the parser does
echo preg_replace('~\s+~', ' ', print_r(new MongoDB\Driver\Manager('mongodb://user:s3cr3t@127.0.0.1/?appname=a@b'), true)), "\n";
foreach ([
    // an unescaped '@' in the password (the reference takes the rest as a host name)
    'mongodb://user:s3c@r3t@127.0.0.1/',
    'mongodb://user:s3c/r3t@127.0.0.1/',
    'mongodb://127.0.0.1/?tlsCertificateKeyFilePassword=s3cr3t%2',
    'mongodb://k@127.0.0.1/?authMechanism=MONGODB-AWS&authMechanismProperties=AWS_SESSION_TOKEN:s3cr3t%2',
    'mongodb://user:s3cr3t@127.0.0.1/?tlsCertificateKeyFilePassword=s3c@r3t&w=-5',
    'mongodb://127.0.0.1/?sslClientCertificateKeyPassword=s3cr3t1&tlsCertificateKeyFilePassword=s3cr3t2',
] as $uri) {
    try {
        new MongoDB\Driver\Manager($uri);
    } catch (MongoDB\Driver\Exception\InvalidArgumentException $e) {
        echo $e->getMessage(), "\n";
    }
}
?>
--EXPECT--
MongoDB\Driver\Manager Object ( [uri] => mongodb://AKID:******@127.0.0.1:1/?authMechanism=MONGODB-AWS&authMechanismProperties=******&appname=x [cluster] => Array ( ) [cryptSharedVersion] => ) 
bool(false)
bool(false)
bool(false)
bool(false)
MongoDB\Driver\Manager Object ( [uri] => mongodb://user:******@127.0.0.1/?appname=a@b [cluster] => Array ( ) [cryptSharedVersion] => ) 
Failed to parse MongoDB URI: 'mongodb://user:******@127.0.0.1/'. Password must not have unescaped chars. Percent-encode username and password according to RFC 3986.
Failed to parse MongoDB URI: 'mongodb://user:******@127.0.0.1/'. Password must not have unescaped chars. Percent-encode username and password according to RFC 3986.
Failed to parse MongoDB URI: 'mongodb://127.0.0.1/?tlsCertificateKeyFilePassword=******'. Value for URI option "tlsCertificateKeyFilePassword" contains is invalid: ******.
Failed to parse MongoDB URI: 'mongodb://k@127.0.0.1/?authMechanism=MONGODB-AWS&authMechanismProperties=******'. Value for URI option "authMechanismProperties" contains is invalid: ******.
Failed to parse MongoDB URI: 'mongodb://user:******@127.0.0.1/?tlsCertificateKeyFilePassword=******&w=-5'. Error while parsing the 'w' URI option: Unsupported w value [w=-5].
Failed to parse MongoDB URI: 'mongodb://127.0.0.1/?sslClientCertificateKeyPassword=******&tlsCertificateKeyFilePassword=******'. Deprecated option 'sslclientcertificatekeypassword=******' conflicts with canonical name 'tlscertificatekeyfilepassword=******'.
