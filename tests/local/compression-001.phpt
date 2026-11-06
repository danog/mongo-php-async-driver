--TEST--
Wire compression: snappy, zlib and zstd are negotiated and used both ways
--DESCRIPTION--
Local test (not upstream). With the compressors URI option, the handshake negotiates the first
compressor the server supports, and every command except the handshake and authentication is
sent as OP_COMPRESSED; the server then compresses its replies with the same compressor. The
server's serverStatus counters show both directions. Needs a server with compression enabled
(mongod's default: snappy,zstd,zlib). The reference passes it when built with all three
libraries (a reference built without snappy does not negotiate snappy).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_load_balanced(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

function counters(MongoDB\Driver\Manager $m, string $c): array
{
    $s = $m->executeCommand('admin', new MongoDB\Driver\Command(['serverStatus' => 1]))->toArray()[0];
    $x = $s->network->compression->$c ?? null;
    return $x === null ? [0, 0] : [$x->decompressor->bytesOut, $x->compressor->bytesIn];
}

$plain = create_test_manager(URI, [], ['disableClientPersistence' => true]);
foreach (['zstd', 'zlib', 'snappy', 'zlib&zlibCompressionLevel=9'] as $c) {
    $name = explode('&', $c)[0];
    $options = ['compressors' => $name];
    if ($c !== $name) {
        $options['zlibCompressionLevel'] = 9;
    }
    $m = create_test_manager(URI, $options, ['disableClientPersistence' => true]);
    [$in0, $out0] = counters($plain, $name);
    $bw = new MongoDB\Driver\BulkWrite;
    $bw->delete([]);
    $bw->insert(['_id' => 1, 'x' => str_repeat('compress me ', 30000)]);
    $m->executeBulkWrite(NS, $bw);
    $doc = $m->executeQuery(NS, new MongoDB\Driver\Query(['_id' => 1]))->toArray()[0];
    $negotiated = $m->getServers()[0]->getInfo()['compression'] ?? [];
    [$in1, $out1] = counters($plain, $name);
    printf("%s: negotiated %s, read back %d bytes, server decompressed >= 360000: %s, compressed >= 360000: %s\n",
        $c, json_encode($negotiated), strlen($doc->x), var_export($in1 - $in0 >= 360000, true), var_export($out1 - $out0 >= 360000, true));
    unset($m);
}
?>
===DONE===
<?php exit(0); ?>
--EXPECT--
zstd: negotiated ["zstd"], read back 360000 bytes, server decompressed >= 360000: true, compressed >= 360000: true
zlib: negotiated ["zlib"], read back 360000 bytes, server decompressed >= 360000: true, compressed >= 360000: true
snappy: negotiated ["snappy"], read back 360000 bytes, server decompressed >= 360000: true, compressed >= 360000: true
zlib&zlibCompressionLevel=9: negotiated ["zlib"], read back 360000 bytes, server decompressed >= 360000: true, compressed >= 360000: true
===DONE===
