<?php
// php -n -d extension=<so> fromjson.php [n]
// fromJSON of 1,000 small documents (relaxed and canonical Extended JSON) and of a long string; best of 7 rounds.
use MongoDB\BSON\Document;
$big = ["docs" => array_map(fn ($i) => ["_id" => $i, "name" => "user$i", "email" => "user$i@example.com", "n" => $i * 3, "f" => $i / 7], range(1, 1000))];
$text = ["s" => str_repeat("lorem ipsum \u{e9}\"\\ ", 5000)];
$cases = ["big-relaxed" => Document::fromPHP($big)->toRelaxedExtendedJSON(), "big-canonical" => Document::fromPHP($big)->toCanonicalExtendedJSON(), "text" => Document::fromPHP($text)->toRelaxedExtendedJSON()];
$n = (int) ($argv[1] ?? 300);
foreach ($cases as $name => $json) {
    $best = INF;
    for ($r = 0; $r < 7; $r++) {
        $t = hrtime(true);
        for ($i = 0; $i < $n; $i++) { Document::fromJSON($json); }
        $best = min($best, (hrtime(true) - $t) / $n);
    }
    echo json_encode(["case" => $name, "ns_per_op" => round($best)]), "\n";
}
