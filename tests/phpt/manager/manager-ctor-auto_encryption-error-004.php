<?php

require_once __DIR__ . '/../utils/basic.inc';

$baseOptions = [
    'keyVaultNamespace' => CSFLE_KEY_VAULT_NS,
    'kmsProviders' => ['local' => ['key' => new MongoDB\BSON\Binary(CSFLE_LOCAL_KEY, 0)]],
];

$tests = [
    ['extraOptions' => ['cryptSharedLibPath' => '/not/found']],
    ['extraOptions' => ['cryptSharedLibRequired' => true]],
];

foreach ($tests as $autoEncryptionOptions) {
    echo throws(function() use ($autoEncryptionOptions, $baseOptions) {
        create_test_manager(null, [], ['autoEncryption' => $autoEncryptionOptions + $baseOptions]);
    }, MongoDB\Driver\Exception\EncryptionException::class), "\n\n";
}

var_dump(get_module_info('crypt_shared library version'));

?>
===DONE===
