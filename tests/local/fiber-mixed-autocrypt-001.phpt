--TEST--
Fibers: a blocking {main} call while a suspended fiber holds the client's automatic encryption
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). A client with automatic
decryption (bypassAutoEncryption): a fiber's reply has a ciphertext, and its decryption suspends
the fiber in the key vault read (held 400 ms by a failpoint), holding the client's encryption
state. Another fiber, or {main} waiting through the loop, waits for it. A {main} call with
mongodb.main_event_loop=0 cannot (that fiber cannot run while it blocks; it used to wait forever):
it throws a LogicException before sending anything. The fiber then decrypts its reply, and the
client works in {main} again.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
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
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, BulkWrite, ClientEncryption};
use MongoDB\BSON\Binary;
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$kms = ['local' => ['key' => new Binary(str_repeat("\x01", 96), Binary::TYPE_GENERIC)]];
$kv = 'mdrop_local.fiber_mixed_autocrypt_keys';
$admin = new Manager($uri);
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_autocrypt_keys']));
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_autocrypt']));
$ce = $admin->createClientEncryption(['keyVaultNamespace' => $kv, 'kmsProviders' => $kms]);
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 's' => $ce->encrypt('secret', ['keyId' => $ce->createDataKey('local'), 'algorithm' => ClientEncryption::AEAD_AES_256_CBC_HMAC_SHA_512_DETERMINISTIC])]);
$admin->executeBulkWrite('mdrop_local.fiber_mixed_autocrypt', $bw);
// a fiber's aggregate whose reply needs a data key: the key vault read (a find) suspends it
$read = fn (Manager $m) => $m->executeCommand('mdrop_local', new Command(['aggregate' => 'fiber_mixed_autocrypt', 'pipeline' => [], 'cursor' => new stdClass]))->toArray()[0]->s;
$ping = fn (Manager $m) => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;

foreach (['0', '1'] as $setting) {
    ini_set('mongodb.main_event_loop', $setting);
    echo "main_event_loop=$setting\n";
    $app = "fiber-mixed-autocrypt-$setting";
    // a new client each time: no data key cached yet
    $m = new Manager($uri, ['appname' => $app], ['disableClientPersistence' => true, 'autoEncryption' => ['keyVaultNamespace' => $kv, 'kmsProviders' => $kms, 'bypassAutoEncryption' => true]]);
    $ping($m);
    $admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => ['times' => 1], 'data' => ['failCommands' => ['find'], 'blockConnection' => true, 'blockTimeMS' => 400, 'appName' => $app]]));
    $f = new Fiber(fn () => $read($m));
    $f->start();
    // the loop runs until the fiber waits for its key vault read
    $s = EventLoop::getSuspension();
    EventLoop::delay(0.1, fn () => $s->resume());
    $s->suspend();
    echo "fiber suspended: ", var_export($f->isSuspended(), true), "\n";
    try {
        $r = $ping($m);
    } catch (Exception $e) {
        $r = get_class($e) . ' ' . $e->getCode() . ': ' . $e->getMessage();
    }
    echo "{main}: $r\n";
    EventLoop::run();
    echo "fiber: ", $f->getReturn(), "\n";
    echo "{main} afterwards: ", $ping($m), ' ', $read($m), "\n";
}
$admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => 'off']));
?>
--EXPECT--
main_event_loop=0
fiber suspended: true
{main}: MongoDB\Driver\Exception\LogicException 0: Automatic encryption is in use by another fiber
fiber: secret
{main} afterwards: 1 secret
main_event_loop=1
fiber suspended: true
{main}: 1
fiber: secret
{main} afterwards: 1 secret
