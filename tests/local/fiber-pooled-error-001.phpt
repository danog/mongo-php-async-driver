--TEST--
Fibers: pooled mode, a new connection whose handshake fails carries the backpressure labels
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Concurrent operations in fibers"). Needs Revolt: the native classes
(mongodb.native_event_loop=1), or revolt/event-loop with REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has it>.
Two fibers at once: the second one gets a new cluster (pooled mode), whose connection's handshake
is closed by the server (failCommand). As libmongoc 2.5.8's pooled clients (_cluster_add_node,
_handle_network_error): the error is the hello's network error with the SystemOverloadedError and
RetryableError labels, no command events are sent for it, the topology is left alone and the next
operation connects again.
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
use MongoDB\Driver\{Manager, Command};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$app = 'pooledErr-' . getmypid();
$admin = new Manager($uri);
$m = new Manager($uri . $sep . 'appname=' . $app, [], ['disableClientPersistence' => true]);
$events = new class implements MongoDB\Driver\Monitoring\CommandSubscriber {
    public array $log = [];
    public function commandStarted($e): void { $this->log[] = 'started ' . $e->getCommandName(); }
    public function commandSucceeded($e): void { $this->log[] = 'succeeded ' . $e->getCommandName(); }
    public function commandFailed($e): void { $this->log[] = 'failed ' . $e->getCommandName(); }
};
$m->executeCommand('admin', new Command(['ping' => 1]));
// only the next connection's handshake (the hello of the second fiber's new cluster)
$admin->executeCommand('admin', new Command([
    'configureFailPoint' => 'failCommand',
    'mode' => ['times' => 1],
    'data' => ['failCommands' => ['hello', 'isMaster'], 'closeConnection' => true, 'appName' => $app],
]));
$m->addSubscriber($events);
$out = [];
foreach ([1, 2] as $i) {
    EventLoop::queue(function () use ($m, $i, &$out) {
        try {
            $m->executeCommand('admin', new Command(['ping' => 1]));
            $out[$i] = "fiber $i: ok";
        } catch (MongoDB\Driver\Exception\Exception $e) {
            $out[$i] = sprintf("fiber %d: %s: %s\n  SystemOverloadedError=%s RetryableError=%s", $i, get_class($e), $e->getMessage(),
                var_export($e->hasErrorLabel('SystemOverloadedError'), true), var_export($e->hasErrorLabel('RetryableError'), true));
        }
    });
}
EventLoop::run();
ksort($out);
echo implode("\n", $out), "\n";
echo implode(', ', $events->log), "\n";
$types = array_map(fn ($s) => $s->getType() !== MongoDB\Driver\Server::TYPE_UNKNOWN, $m->getServers());
echo 'servers known: ', var_export(!in_array(false, $types, true), true), "\n";
// both clusters work again (the failed one connects anew)
$ok = 0;
foreach ([1, 2] as $i) {
    EventLoop::queue(function () use ($m, &$ok) { $m->executeCommand('admin', new Command(['ping' => 1])); $ok++; });
}
EventLoop::run();
echo "again: $ok ok\n";
$admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => 'off']));
?>
--EXPECT--
fiber 1: ok
fiber 2: MongoDB\Driver\Exception\ConnectionTimeoutException: Failed to send "hello" command with database "admin": Failed to read 4 bytes: socket error or timeout
  SystemOverloadedError=true RetryableError=true
started ping, succeeded ping
servers known: true
again: 2 ok
