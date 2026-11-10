--TEST--
BSON encoding: containers changed or freed by user code while they are encoded
--DESCRIPTION--
Local test (not upstream). Listed in DIVERGENCES.md. bsonSerialize() (or a destructor) may change
or free an array or object the encoder is walking: through a PHP reference, an object handle or
a declared property. ext-mongodb keeps raw pointers into the hash table, so a table that grows
or is freed meanwhile is read from freed memory (valgrind: invalid reads). Here every container
being encoded is held (as PHP's own foreach by value holds its array), so PHP separates it before
changing it: arrays and dynamic properties are encoded as they were when their encoding started,
a value freed meanwhile is still encoded, and declared property values are read when reached.
The property table of a class with its own get_properties handler (DateInterval, ...) is copied
first (such a handler may change its table in place).
--FILE--
<?php
final class S implements MongoDB\BSON\Serializable {
    public function __construct(private Closure $f) {}
    public function bsonSerialize(): array { ($this->f)(); return ['s' => 1]; }
}
$cases = [
    'array grows' => function () {
        $inner = ['a' => new S(function () use (&$inner) { for ($i = 0; $i < 100; $i++) { $inner["k$i"] = $i; } }), 'b' => 'x'];
        return ['x' => &$inner];
    },
    'array freed' => function () {
        $inner = ['a' => new S(function () use (&$inner) { $inner = null; }), 'b' => str_repeat('y', 3)];
        return ['x' => &$inner];
    },
    'packed array becomes a hash' => function () {
        $inner = [new S(function () use (&$inner) { $inner['k'] = 1; for ($i = 0; $i < 100; $i++) { $inner[] = $i; } }), 'b'];
        return ['x' => &$inner];
    },
    'array changed in place' => function () {
        $inner = ['a' => new S(function () use (&$inner) { $inner['b'] = 'changed'; unset($inner['c']); }), 'b' => 'x', 'c' => 'y'];
        return ['x' => &$inner];
    },
    'dynamic properties grow' => function () {
        $o = new stdClass;
        $o->a = new S(function () use ($o) { for ($i = 0; $i < 100; $i++) { $o->{"p$i"} = $i; } });
        $o->b = 'x';
        return ['o' => $o];
    },
    'dynamic properties changed' => function () {
        $o = new stdClass;
        $o->a = new S(function () use ($o) { $o->b = 'changed'; unset($o->c); $o->d = 'added'; });
        $o->b = 'x';
        $o->c = 'y';
        return ['o' => $o];
    },
    'object freed' => function () {
        $o = new stdClass;
        $o->a = new S(function () use (&$o) { $o = null; });
        $o->b = str_repeat('z', 3);
        return ['o' => &$o];
    },
    'object freed through a declared property' => function () {
        $p = new class { public $a; public $b = 'x'; };
        $holder = new class { public $o; public $after = 'after'; };
        $holder->o = $p;
        $p->a = new S(function () use ($holder) { $holder->o = null; });
        return $holder;
    },
    'declared properties changed' => function () {
        $p = new class { public $a; public $b = 'x'; public $c = 'y'; };
        $p->a = new S(function () use ($p) { $p->b = 'changed'; unset($p->c); });
        return ['p' => $p];
    },
    // a class with its own get_properties handler: its table is copied first, same output
    'DateInterval' => fn () => ['di' => new DateInterval('P1DT2H')],
];
foreach ($cases as $name => $make) {
    echo $name, ': ', MongoDB\BSON\Document::fromPHP($make())->toRelaxedExtendedJSON(), "\n";
}
?>
--EXPECT--
array grows: { "x" : { "a" : { "s" : 1 }, "b" : "x" } }
array freed: { "x" : { "a" : { "s" : 1 }, "b" : "yyy" } }
packed array becomes a hash: { "x" : [ { "s" : 1 }, "b" ] }
array changed in place: { "x" : { "a" : { "s" : 1 }, "b" : "x", "c" : "y" } }
dynamic properties grow: { "o" : { "a" : { "s" : 1 }, "b" : "x" } }
dynamic properties changed: { "o" : { "a" : { "s" : 1 }, "b" : "x", "c" : "y" } }
object freed: { "o" : { "a" : { "s" : 1 }, "b" : "zzz" } }
object freed through a declared property: { "o" : { "a" : { "s" : 1 }, "b" : "x" }, "after" : "after" }
declared properties changed: { "p" : { "a" : { "s" : 1 }, "b" : "changed" } }
DateInterval: { "di" : { "y" : 0, "m" : 0, "d" : 1, "h" : 2, "i" : 0, "s" : 0, "f" : 0.0, "invert" : 0, "days" : false, "from_string" : false } }
