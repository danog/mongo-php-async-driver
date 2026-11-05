--TEST--
MongoDB\BSON\Int64 debug output
--FILE--
<?php

$tests = [
    '9223372036854775807',
    '-9223372036854775808',
    '0',
];

foreach ($tests as $test) {
    var_dump(new MongoDB\BSON\Int64($test));
}

?>
===DONE===
<?php exit(0); ?>
--EXPECTF--
object(MongoDB\BSON\Int64)#%d (%d) {
  ["integer"]=>
  string(19) "9223372036854775807"
}
object(MongoDB\BSON\Int64)#%d (%d) {
  ["integer"]=>
  string(20) "-9223372036854775808"
}
object(MongoDB\BSON\Int64)#%d (%d) {
  ["integer"]=>
  string(1) "0"
}
===DONE===
