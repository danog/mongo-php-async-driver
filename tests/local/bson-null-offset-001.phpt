--TEST--
Appending through a Document or PackedArray ($doc[]) throws instead of dereferencing a NULL offset
--DESCRIPTION--
Local test (not upstream): ext-mongodb 2.5.6 crashes here (NULL offset in read_dimension); this
driver throws the LogicException that phongo throws for writes. Divergence listed in
tests/NOT_APPLICABLE.md.
--FILE--
<?php
$d = MongoDB\BSON\Document::fromPHP(['a' => 1]);
try { $d[]['a'] = 1; } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
$p = MongoDB\BSON\PackedArray::fromPHP([1]);
try { $p[]['a'] = 1; } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
?>
===DONE===
--EXPECTF--
Notice: Indirect modification of overloaded element of MongoDB\BSON\Document has no effect in %s on line %d
MongoDB\Driver\Exception\LogicException: Cannot write to MongoDB\BSON\Document property

Notice: Indirect modification of overloaded element of MongoDB\BSON\PackedArray has no effect in %s on line %d
MongoDB\Driver\Exception\LogicException: Cannot write to MongoDB\BSON\PackedArray offset
===DONE===
