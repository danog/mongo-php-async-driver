<?php
/*
 * BSON encode/decode micro-benchmark: run the same script under two builds of the extension
 * and compare. Prints one JSON line per case: {"case", "ops", "ns_per_op"} (best of REPS runs).
 *   php -n -d extension=<so> tools/bench/bson.php [reps=5] [filter]
 */
use MongoDB\BSON\{Binary, Document, Int64, ObjectId, PackedArray, Persistable, Regex, UTCDateTime, Decimal128, Timestamp};

final class Point implements Persistable
{
    public function __construct(public int $x = 0, public int $y = 0) {}
    public function bsonSerialize(): array { return ['x' => $this->x, 'y' => $this->y]; }
    public function bsonUnserialize(array $data): void { $this->x = $data['x']; $this->y = $data['y']; }
}

$reps = (int) ($argv[1] ?? 5);
$filter = $argv[2] ?? '';

$small = ['_id' => 1, 'name' => 'Alice', 'age' => 30, 'active' => true, 'score' => 12.5];
$typed = [
    '_id' => new ObjectId('5f1a2b3c4d5e6f7081920a1b'),
    'created' => new UTCDateTime(1700000000000),
    'big' => new Int64('9007199254740993'),
    'dec' => new Decimal128('1234.5678'),
    'bin' => new Binary(str_repeat("\x01", 32), Binary::TYPE_GENERIC),
    're' => new Regex('^ab+c', 'i'),
    'ts' => new Timestamp(1, 1700000000),
    'nil' => null,
];
$nested = [
    'user' => ['name' => 'Bob', 'tags' => ['a', 'b', 'c'], 'address' => ['city' => 'Rome', 'zip' => '00100', 'geo' => [41.9, 12.5]]],
    'items' => array_map(fn ($i) => ['sku' => "sku-$i", 'qty' => $i, 'price' => $i * 1.25], range(1, 20)),
    'meta' => ['v' => 3, 'flags' => [true, false, true]],
];
$big = ['docs' => array_map(fn ($i) => ['_id' => $i, 'name' => "user$i", 'email' => "user$i@example.com", 'n' => $i * 3, 'f' => $i / 7], range(1, 1000))];
$text = ['body' => str_repeat('lorem ipsum dolor sit amet ', 4000)];
$objs = ['points' => array_map(fn ($i) => new Point($i, -$i), range(1, 50))];

$docs = compact('small', 'typed', 'nested', 'big', 'text', 'objs');
$cases = [];
foreach ($docs as $name => $value) {
    $bson = Document::fromPHP($value);
    $json = $bson->toCanonicalExtendedJSON();
    $rjson = $bson->toRelaxedExtendedJSON();
    $cases["$name/fromPHP"] = fn () => Document::fromPHP($value);
    $cases["$name/toPHP"] = fn () => $bson->toPHP();
    $cases["$name/toPHP-array"] = fn () => $bson->toPHP(['root' => 'array', 'document' => 'array', 'array' => 'array']);
    $cases["$name/fromBSON"] = fn () => Document::fromBSON((string) $bson);
    $cases["$name/toCanonicalJSON"] = fn () => $bson->toCanonicalExtendedJSON();
    $cases["$name/toRelaxedJSON"] = fn () => $bson->toRelaxedExtendedJSON();
    $cases["$name/fromJSON"] = fn () => Document::fromJSON($rjson);
}
$small = Document::fromPHP($small);
$cases['small/get'] = fn () => $small->get('name');
$cases['small/has-miss'] = fn () => $small->has('zzz');
$cases['small/iterate'] = function () use ($small) { foreach ($small as $k => $v) {} };
$arr = PackedArray::fromPHP(range(1, 100));
$cases['packed100/toPHP'] = fn () => $arr->toPHP();
$cases['objectid/new'] = fn () => new ObjectId();
$cases['int64/add'] = function () { $a = new Int64(5); return $a + 7; };

foreach ($cases as $name => $fn) {
    if ($filter !== '' && !str_contains($name, $filter)) {
        continue;
    }
    // calibrate to ~50 ms per run
    $n = 1;
    while (true) {
        $t = hrtime(true);
        for ($i = 0; $i < $n; $i++) { $fn(); }
        $dt = hrtime(true) - $t;
        if ($dt > 20e6) { break; }
        $n *= 2;
    }
    $n = max(1, (int) ($n * 50e6 / $dt));
    $best = INF;
    for ($r = 0; $r < $reps; $r++) {
        $t = hrtime(true);
        for ($i = 0; $i < $n; $i++) { $fn(); }
        $best = min($best, (hrtime(true) - $t) / $n);
    }
    echo json_encode(['case' => $name, 'ops' => $n, 'ns_per_op' => round($best, 1)]), "\n";
}
