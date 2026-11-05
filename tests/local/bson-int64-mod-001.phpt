--TEST--
Int64: INT64_MIN % -1 is 0 instead of a SIGFPE
--DESCRIPTION--
Local test (not upstream). ext-mongodb 2.5.6 computes the remainder with C's %, which traps
(SIGFPE: the process dies) for INT64_MIN % -1 on x86-64; arm64 returns 0. This driver returns
Int64(0) everywhere, as PHP's own % does. Listed in DIVERGENCES.md (safety).
--FILE--
<?php
$a = new MongoDB\BSON\Int64('-9223372036854775808');
var_dump($a % -1);
var_dump($a % new MongoDB\BSON\Int64(-1));
try { $a % 0; } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
?>
--EXPECT--
object(MongoDB\BSON\Int64)#2 (1) {
  ["integer"]=>
  string(1) "0"
}
object(MongoDB\BSON\Int64)#3 (1) {
  ["integer"]=>
  string(1) "0"
}
Division by zero
