--TEST--
A server-supplied __pclass reaches the autoloader only as a valid class name
--DESCRIPTION--
Local test (not upstream). Safety review of ODM decoding: phongo (and this driver) call
zend_fetch_class with autoload before checking Persistable. PHP's zend_lookup_class_ex rejects
names that are not valid class names before calling any autoloader, so no ".", "/", NUL or empty
name gets there. Same behaviour as ext-mongodb 2.5.6 (not a divergence).
--FILE--
<?php
spl_autoload_register(function ($c) { echo "autoload(", json_encode($c), ")\n"; });
foreach (["../../etc/passwd", "Foo\\Bar", "Foo.Bar", "a/b", "x\0y", "Ünï", "9abc", "\\Lead", "Trail\\", ""] as $name) {
    $b = MongoDB\BSON\Document::fromPHP(['__pclass' => new MongoDB\BSON\Binary($name, 0x80), 'x' => 1]);
    $v = $b->toPHP();
    echo json_encode($name), " -> ", get_class($v), "\n";
}
?>
--EXPECT--
"..\/..\/etc\/passwd" -> stdClass
autoload("Foo\\Bar")
"Foo\\Bar" -> stdClass
"Foo.Bar" -> stdClass
"a\/b" -> stdClass
"x\u0000y" -> stdClass
autoload("\u00dcn\u00ef")
"\u00dcn\u00ef" -> stdClass
autoload("9abc")
"9abc" -> stdClass
autoload("Lead")
"\\Lead" -> stdClass
autoload("Trail\\")
"Trail\\" -> stdClass
"" -> stdClass
