<?php

require_once __DIR__ . "/../utils/basic.inc";

/* autoEncryption requires keyVaultNamespace and a kmsProvider. Additionally,
 * disable mongocryptd spawning since it is not required for this test. */
$baseOptions = [
    'keyVaultNamespace' => CSFLE_KEY_VAULT_NS,
    'kmsProviders' => ['local' => ['key' => new MongoDB\BSON\Binary(CSFLE_LOCAL_KEY, 0)]],
    'extraOptions' => ['mongocryptdBypassSpawn' => true],
];

$encryptedFields = [
    'escCollection' => 'escCollectionName',
    'eccCollection' => 'eccCollectionName',
    'ecocCollection' => 'ecocCollectionName',
    'fields' => [
        [
            'path' => 'foo',
            'keyId' => new MongoDB\BSON\Binary(str_repeat('0', 16), MongoDB\BSON\Binary::TYPE_UUID),
            'bsonType' => 'string',
            'queries' => ['queryType' => 'equality'],
        ],
    ],
];

$tests = [
    [],
    ['autoEncryption' => $baseOptions],
    ['autoEncryption' => ['encryptedFieldsMap' => []] + $baseOptions],
    ['autoEncryption' => ['encryptedFieldsMap' => ['db.coll' => $encryptedFields]] + $baseOptions],
];

foreach ($tests as $i => $driverOptions) {
    $manager = create_test_manager(null, [], $driverOptions);
    var_dump($manager->getEncryptedFieldsMap());
}

?>
===DONE===
