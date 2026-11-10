--TEST--
Type map: fieldPaths changed or freed by an autoloader while the type map is parsed
--DESCRIPTION--
Local test (not upstream). Listed in DIVERGENCES.md. Parsing a fieldPaths class name runs the
autoloader, which may change or free the fieldPaths array (here reached through a reference).
ext-mongodb keeps iterating the freed table (valgrind: invalid reads). Here the table is held while
it is parsed: the type map is the fieldPaths array as it was when parsing started.
--FILE--
<?php
$fp = ['a' => 'LazyA'];
$fp['b'] = 'array';
$fp['c.d'] = 'array';
spl_autoload_register(function ($c) use (&$fp) {
    echo "autoload $c\n";
    eval('final class LazyA implements MongoDB\BSON\Unserializable { public function bsonUnserialize(array $d): void { echo "LazyA::bsonUnserialize\n"; } }');
    for ($i = 0; $i < 100; $i++) { $fp["k$i"] = 'object'; }
    $fp = null;
});
$tm = ['root' => 'object', 'fieldPaths' => &$fp];
$doc = MongoDB\BSON\Document::fromPHP(['a' => ['x' => 1], 'b' => ['y' => 2], 'c' => ['d' => ['z' => 3]], 'k1' => ['w' => 4]]);
var_dump($doc->toPHP($tm));
var_dump($fp);
?>
--EXPECTF--
autoload LazyA
LazyA::bsonUnserialize
object(stdClass)#%d (4) {
  ["a"]=>
  object(LazyA)#%d (0) {
  }
  ["b"]=>
  array(1) {
    ["y"]=>
    int(2)
  }
  ["c"]=>
  object(stdClass)#%d (1) {
    ["d"]=>
    array(1) {
      ["z"]=>
      int(3)
    }
  }
  ["k1"]=>
  object(stdClass)#%d (1) {
    ["w"]=>
    int(4)
  }
}
NULL
