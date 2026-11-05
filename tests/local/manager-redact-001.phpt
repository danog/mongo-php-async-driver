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
// an unescaped '@' in the password (the reference parses it as part of the host list)
echo preg_replace('~\s+~', ' ', print_r(new MongoDB\Driver\Manager('mongodb://user:s3c@r3t@127.0.0.1/'), true)), "\n";
foreach ([
    'mongodb://user:s3c/r3t@127.0.0.1/',
    'mongodb://127.0.0.1/?tlsCertificateKeyFilePassword=s3cr3t%2',
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
MongoDB\Driver\Manager Object ( [uri] => mongodb://user:******@127.0.0.1/ [cluster] => Array ( ) [cryptSharedVersion] => ) 
Failed to parse MongoDB URI: 'mongodb://user:******@127.0.0.1/'. Password must not have unescaped chars. Percent-encode username and password according to RFC 3986.
Failed to parse MongoDB URI: 'mongodb://127.0.0.1/?tlsCertificateKeyFilePassword=******'. Value for URI option "tlsCertificateKeyFilePassword" contains is invalid: ******.
