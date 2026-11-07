--TEST--
ClientEncryption and autoEncryption: no KMS provider at all is an error
--DESCRIPTION--
Local test (not upstream). As in libmongocrypt (_mongocrypt_opts_validate): kmsProviders must
name at least one provider (configured, named, or {} for on-demand credentials). Covers the
unified-format spec test valid-fail/kmsProviders-no_kms.
--FILE--
<?php
$m = new MongoDB\Driver\Manager('mongodb://127.0.0.1:1');
$cases = [
    'createClientEncryption' => fn ($k) => $m->createClientEncryption(['keyVaultNamespace' => 'k.d', 'kmsProviders' => $k]),
    'ClientEncryption' => fn ($k) => new MongoDB\Driver\ClientEncryption(['keyVaultClient' => $m, 'keyVaultNamespace' => 'k.d', 'kmsProviders' => $k]),
    'autoEncryption' => fn ($k) => new MongoDB\Driver\Manager('mongodb://127.0.0.1:1', [], ['autoEncryption' => ['keyVaultNamespace' => 'k.d', 'kmsProviders' => $k, 'bypassAutoEncryption' => true]]),
];
foreach ($cases as $name => $f) {
    foreach ([[], new stdClass(), ['aws' => new stdClass()]] as $k) {
        try {
            $f($k);
            echo $name, ": OK\n";
        } catch (MongoDB\Driver\Exception\EncryptionException $e) {
            echo $name, ': ', $e->getMessage(), "\n";
        }
    }
}
?>
--EXPECT--
createClientEncryption: no kms provider set
createClientEncryption: no kms provider set
createClientEncryption: OK
ClientEncryption: no kms provider set
ClientEncryption: no kms provider set
ClientEncryption: OK
autoEncryption: no kms provider set
autoEncryption: no kms provider set
autoEncryption: OK
