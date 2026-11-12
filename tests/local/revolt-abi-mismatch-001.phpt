--TEST--
Revolt: mongodb refuses to start after a revolt extension of an incompatible embedding ABI
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). When a revolt-rs copy
whose embedding ABI the driver cannot use (another major version, or a table smaller than the
driver's) registered the Revolt classes first, the driver's waits could not go through the loop:
the extension's MINIT fails with one warning naming both ABIs, and PHP does not start. The
incompatible builds are revolt-rs's standalone extension built with its test features
test-abi-major-mismatch / test-abi-size-mismatch (REVOLT_EXT_ABI_MAJOR / REVOLT_EXT_ABI_SIZE,
as for revolt-rs's tools/embed-test.sh), loaded before this extension in a child PHP.
--SKIPIF--
<?php if (!getenv('REVOLT_EXT_ABI_MAJOR') || !getenv('REVOLT_EXT_ABI_SIZE')) die('skip REVOLT_EXT_ABI_MAJOR / REVOLT_EXT_ABI_SIZE not set'); ?>
--FILE--
<?php
foreach (['REVOLT_EXT_ABI_MAJOR', 'REVOLT_EXT_ABI_SIZE'] as $env) {
    // the incompatible revolt extension first, then this extension (run-tests' -d settings)
    $cmd = escapeshellarg(getenv('TEST_PHP_EXECUTABLE')) . ' -n -d ' . escapeshellarg('extension=' . getenv($env))
        . ' ' . getenv('TEST_PHP_EXTRA_ARGS') . ' -r ' . escapeshellarg('echo 7;') . ' 2>&1';
    exec($cmd, $out, $status);
    $text = implode("\n", $out);
    $out = [];
    echo $env, "\n";
    echo '  exit status: ', $status === 0 ? '0' : 'not 0', "\n";
    echo '  ', preg_match('/mongodb cannot start: [^\n]*/', $text, $m) ? $m[0] : 'no warning', "\n";
    echo '  ', substr_count($text, 'Unable to start mongodb module'), " Unable to start\n";
}
?>
--EXPECTF--
REVOLT_EXT_ABI_MAJOR
  exit status: not 0
  mongodb cannot start: the Revolt classes belong to an incompatible revolt extension loaded before it ("revolt" %s): its revolt-rs embedding ABI is %d.%d (a %d-byte table), mongodb requires %d.%d or a later %d.x (at least %d bytes). Load a revolt extension built with a compatible revolt-rs, or none in Unknown on line 0
  1 Unable to start
REVOLT_EXT_ABI_SIZE
  exit status: not 0
  mongodb cannot start: the Revolt classes belong to an incompatible revolt extension loaded before it ("revolt" %s): its revolt-rs embedding ABI is %d.%d (a %d-byte table), mongodb requires %d.%d or a later %d.x (at least %d bytes). Load a revolt extension built with a compatible revolt-rs, or none in Unknown on line 0
  1 Unable to start
