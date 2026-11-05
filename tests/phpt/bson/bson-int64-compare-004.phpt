--TEST--
MongoDB\BSON\Int64 comparisons with scalars (64-bit values, all platforms)
--FILE--
<?php

// Use 2**33 to ensure it still fits in a float
$int64 = new MongoDB\BSON\Int64('8589934592');

$tests = [
    'matching float' => (float) 2**33,
    'wrong int' => 0,
];

foreach ($tests as $name => $value) {
    printf('Testing %s: %s' . PHP_EOL, $name, var_export($int64 == $value, true));
}

var_dump($int64 > 123);

?>
===DONE===
<?php exit(0); ?>
--EXPECT--
Testing matching float: true
Testing wrong int: false
bool(true)
===DONE===
