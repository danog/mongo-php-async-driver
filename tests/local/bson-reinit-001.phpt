--TEST--
Re-initialising a Document, PackedArray or Javascript keeps the bytes that live iterators and decodes borrow
--DESCRIPTION--
Local test (not upstream): __unserialize() and __construct() on a live object must not free a buffer
that an Iterator still reads; phongo leaks the old buffer, so iterators keep seeing the old data.
--FILE--
<?php
$d = MongoDB\BSON\Document::fromPHP(['a' => 1, 'b' => str_repeat('x', 64)]);
$it = $d->getIterator();
$d->__unserialize(['data' => base64_encode((string) MongoDB\BSON\Document::fromPHP(['z' => 26]))]);
for (; $it->valid(); $it->next()) {
    var_dump($it->key(), is_string($it->current()) ? strlen($it->current()) : $it->current());
}
var_dump($d->toPHP());

$p = MongoDB\BSON\PackedArray::fromPHP([1, 2, 3]);
$it = $p->getIterator();
$p->__unserialize(['data' => base64_encode((string) MongoDB\BSON\PackedArray::fromPHP([9]))]);
foreach ($it as $k => $v) {
    echo "$k=$v\n";
}

class Reenter implements MongoDB\BSON\Unserializable
{
    public static ?MongoDB\BSON\Javascript $js = null;
    public function bsonUnserialize(array $data): void
    {
        // replaces the scope being decoded
        self::$js->__construct('other', ['q' => str_repeat('y', 100)]);
    }
}
Reenter::$js = new MongoDB\BSON\Javascript('code', ['s' => new Reenter(), 't' => 'tail']);
var_dump(array_keys((array) Reenter::$js->getScope()));
?>
===DONE===
--EXPECT--
string(1) "a"
int(1)
string(1) "b"
int(64)
object(stdClass)#3 (1) {
  ["z"]=>
  int(26)
}
0=1
1=2
2=3
array(2) {
  [0]=>
  string(1) "s"
  [1]=>
  string(1) "t"
}
===DONE===
