--TEST--
ClientEncryption: range errors never show the plaintext values being encrypted
--DESCRIPTION--
Local test (not upstream). libmongocrypt's range errors print the value being encrypted, the
query bounds (and their order-preserving encodings) and, for encryptExpression, the whole
expression as Extended JSON: what the application meant to protect would end up in exception
messages and logs. Here those values read "<redacted>" (also in the appended expression); the
configured index bounds and the structure (operators, types, field names) are kept. Listed in
DIVERGENCES.md (permanent, for safety).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_server_version('<', '7.0'); ?>
<?php skip_if_not_clean(CSFLE_KEY_VAULT_DATABASE_NAME, CSFLE_KEY_VAULT_COLLECTION_NAME);
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";
use MongoDB\Driver\ClientEncryption as CE;
$ce = create_test_manager()->createClientEncryption([
    'keyVaultNamespace' => CSFLE_KEY_VAULT_NS,
    'kmsProviders' => ['local' => ['key' => new MongoDB\BSON\Binary(CSFLE_LOCAL_KEY, 0)]],
]);
$id = $ce->createDataKey('local');
$opts = ['keyId' => $id, 'algorithm' => CE::ALGORITHM_RANGE, 'contentionFactor' => 0, 'rangeOpts' => ['min' => 0, 'max' => 200]];
foreach ([
    fn() => $ce->encrypt(98765, $opts),
    fn() => $ce->encryptExpression(['$and' => [['age' => ['$gt' => 98765]]]], $opts + ['queryType' => CE::QUERY_TYPE_RANGE]),
    fn() => $ce->encryptExpression(['$and' => [['age' => ['$foo' => 98765]]]], $opts + ['queryType' => CE::QUERY_TYPE_RANGE]),
    fn() => $ce->encryptExpression(['$and' => [['age' => ['$gt' => 200]]]], $opts + ['queryType' => CE::QUERY_TYPE_RANGE]),
] as $f) {
    try {
        $f();
    } catch (MongoDB\Driver\Exception\EncryptionException $e) {
        echo $e->getMessage(), "\n";
        var_dump(strpos($e->getMessage(), '98765'));
    }
}
?>
===DONE===
--EXPECT--
Value must be greater than or equal to the minimum value and less than or equal to the maximum value, got min: 0, max: 200, value: <redacted>
bool(false)
Lower bound (<redacted>) must be less than or equal to the range maximum (200)
bool(false)
unsupported operator: $foo: { "$and" : [ { "age" : { "$foo" : "<redacted>" } } ] }
bool(false)
Lower bound (<redacted>) must be less than the range maximum (200) if lower bound is excluded from range
bool(false)
===DONE===
