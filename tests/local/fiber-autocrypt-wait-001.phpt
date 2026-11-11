--TEST--
Fibers: a fiber waits for the automatic encryption another suspended fiber holds
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). A client with automatic
decryption (bypassAutoEncryption): fiber A's reply has a ciphertext, and its decryption suspends A
in the key vault read (held 400 ms by a failpoint), holding the client's encryption state. Fibers
B and C then run commands on that client: both wait until A puts the state back, then both run;
also when an exception is thrown into A meanwhile (A's call ends with it, and still puts the state
back: B and C must be resumed although that happens while A's exception is pending).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!class_exists('Revolt\EventLoop', false) && !is_file((string) getenv('REVOLT_AUTOLOAD'))) die('skip no Revolt: no native Revolt\EventLoop, no REVOLT_AUTOLOAD'); ?>
<?php
$m = new MongoDB\Driver\Manager(getenv('MONGODB_URI'));
try {
    $m->executeCommand('admin', new MongoDB\Driver\Command(['configureFailPoint' => 'failCommand', 'mode' => 'off']));
} catch (Exception $e) {
    die('skip failCommand not available (enableTestCommands)');
}
?>
--CONFLICTS--
all
--FILE--
<?php
if (is_file((string) getenv('REVOLT_AUTOLOAD'))) require getenv('REVOLT_AUTOLOAD'); // else the native classes
use MongoDB\Driver\{Manager, Command, BulkWrite, ClientEncryption};
use MongoDB\BSON\Binary;
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$kms = ['local' => ['key' => new Binary(str_repeat("\x01", 96), Binary::TYPE_GENERIC)]];
$kv = 'mdrop_local.fiber_autocrypt_wait_keys';
$admin = new Manager($uri);
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_autocrypt_wait_keys']));
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_autocrypt_wait']));
$ce = $admin->createClientEncryption(['keyVaultNamespace' => $kv, 'kmsProviders' => $kms]);
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 's' => $ce->encrypt('secret', ['keyId' => $ce->createDataKey('local'), 'algorithm' => ClientEncryption::AEAD_AES_256_CBC_HMAC_SHA_512_DETERMINISTIC])]);
$admin->executeBulkWrite('mdrop_local.fiber_autocrypt_wait', $bw);
$read = fn (Manager $m) => $m->executeCommand('mdrop_local', new Command(['aggregate' => 'fiber_autocrypt_wait', 'pipeline' => [], 'cursor' => new stdClass]))->toArray()[0]->s;
$ping = fn (Manager $m) => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
$runFor = function (float $s) {
    $suspension = EventLoop::getSuspension();
    EventLoop::delay($s, fn () => $suspension->resume());
    $suspension->suspend();
};

// A holds the encryption state (its key vault read suspended), B and C wait for it; $interrupt:
// an exception is thrown into A meanwhile (its hook still puts the state back)
$scenario = function (bool $interrupt) use ($uri, $kv, $kms, $admin, $read, $ping, $runFor) {
    $app = 'fiber-autocrypt-wait-' . (int) $interrupt;
    // a new client each time: no data key cached yet
    $m = new Manager($uri, ['appname' => $app], ['disableClientPersistence' => true, 'autoEncryption' => ['keyVaultNamespace' => $kv, 'kmsProviders' => $kms, 'bypassAutoEncryption' => true]]);
    $ping($m);
    $admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => ['times' => 1], 'data' => ['failCommands' => ['find'], 'blockConnection' => true, 'blockTimeMS' => 400, 'appName' => $app]]));
    $done = [];
    $a = new Fiber(function () use (&$done, $read, $m) {
        try {
            $done[] = 'A: ' . $read($m);
        } catch (Exception $e) {
            // the exception thrown into A (or one whose previous it is)
            for (; $e !== null && $e->getMessage() !== 'interrupted'; $e = $e->getPrevious());
            $done[] = 'A: ' . ($e === null ? 'another exception' : $e->getMessage());
        }
    });
    $a->start();
    $runFor(0.1);
    echo "A suspended: ", var_export($a->isSuspended(), true), "\n";
    $b = new Fiber(function () use (&$done, $ping, $m) {
        $done[] = 'B: ' . $ping($m);
    });
    $b->start();
    $c = new Fiber(function () use (&$done, $ping, $m) {
        $done[] = 'C: ' . $ping($m);
    });
    $c->start();
    // B and C reach the wait for A's encryption state
    $runFor(0.1);
    echo "B, C suspended: ", var_export($b->isSuspended() && $c->isSuspended() && $done === [], true), "\n";
    if ($interrupt) {
        $a->throw(new Exception('interrupted'));
    }
    EventLoop::run();
    // A first (it puts the state back before B and C can run); B and C in either order
    echo array_shift($done), "\n";
    sort($done);
    echo implode("\n", $done), "\n";
    echo "{main} afterwards: ", $ping($m), ' ', $read($m), "\n";
};
$scenario(false);
$scenario(true);
$admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => 'off']));
?>
--EXPECT--
A suspended: true
B, C suspended: true
A: secret
B: 1
C: 1
{main} afterwards: 1 secret
A suspended: true
B, C suspended: true
A: interrupted
B: 1
C: 1
{main} afterwards: 1 secret
