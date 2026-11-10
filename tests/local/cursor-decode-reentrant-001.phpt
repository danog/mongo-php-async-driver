--TEST--
Cursor: advancing or changing a cursor from user code its own decoding runs
--DESCRIPTION--
Local test (not upstream). Listed in DIVERGENCES.md. A bsonUnserialize() (or an autoloader) run
while a cursor decodes its current document may call that cursor's next(): ext-mongodb then sends
the getMore, frees the reply it is decoding and keeps reading it (valgrind: invalid reads). Here
next(), rewind(), toArray() and setTypeMap() on a cursor that is decoding a document throw
LogicException; the document being decoded is finished normally.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

final class P implements MongoDB\BSON\Persistable {
    public static ?MongoDB\Driver\Cursor $cursor = null;
    public $data;
    public function bsonSerialize(): array { return ['x' => $this->data]; }
    public function bsonUnserialize(array $d): void {
        $this->data = $d['x'];
        if (self::$cursor !== null) {
            foreach (['next', 'rewind', 'toArray'] as $m) {
                try {
                    self::$cursor->$m();
                } catch (MongoDB\Driver\Exception\LogicException $e) {
                    echo $m, ': ', $e->getMessage(), "\n";
                }
            }
            try {
                self::$cursor->setTypeMap(['root' => 'array']);
            } catch (MongoDB\Driver\Exception\LogicException $e) {
                echo 'setTypeMap: ', $e->getMessage(), "\n";
            }
            self::$cursor = null;
        }
    }
}
$m = create_test_manager();
$m->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['drop' => COLLECTION_NAME]));
$bw = new MongoDB\Driver\BulkWrite;
for ($i = 0; $i < 4; $i++) {
    $p = new P;
    $p->data = str_repeat(chr(65 + $i), 3);
    $bw->insert(['_id' => $i, 'p' => $p]);
}
$m->executeBulkWrite(NS, $bw);
$c = $m->executeQuery(NS, new MongoDB\Driver\Query([], ['batchSize' => 2, 'sort' => ['_id' => 1]]));
P::$cursor = $c;
foreach ($c as $doc) {
    echo $doc->_id, ' ', $doc->p->data, "\n";
}
?>
--EXPECT--
next: The cursor cannot be advanced or changed while it decodes a document
rewind: The cursor cannot be advanced or changed while it decodes a document
toArray: The cursor cannot be advanced or changed while it decodes a document
setTypeMap: The cursor cannot be advanced or changed while it decodes a document
0 AAA
1 BBB
2 CCC
3 DDD
