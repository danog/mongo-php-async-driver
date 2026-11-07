--TEST--
SDAM: every network error of a single-threaded client marks the server Unknown (current pool generation)
--DESCRIPTION--
Local test (not upstream). libmongoc's single-threaded client applies a network error with the
server's current connection-pool generation (_cluster_fetch_stream_single copies it into the
stream), so a second failure after a pool clear marks the server Unknown again.
The failCommand fail point is global to the server: run alone (it would replace and then turn off
the fail points of tests running in parallel).
--CONFLICTS--
all
--SKIPIF--
<?php require __DIR__ . '/../phpt/utils/basic-skipif.inc'; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_test_commands_disabled(); ?>
--FILE--
<?php
require_once __DIR__ . '/../phpt/utils/basic.inc';
use MongoDB\Driver\{Manager, Command};
$app = 'gen-test-' . getmypid();
$sub = new class implements MongoDB\Driver\Monitoring\SDAMSubscriber {
    public int $unknown = 0;
    public function serverChanged($e): void { if ($e->getNewDescription()->getType() === 'Unknown') $this->unknown++; }
    public function serverHeartbeatStarted($e): void {}
    public function serverHeartbeatSucceeded($e): void {}
    public function serverHeartbeatFailed($e): void {}
    public function serverClosed($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyClosed($e): void {}
    public function topologyOpening($e): void {}
};
$uri = URI;
$m = new Manager($uri . (str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?')) . 'appname=' . $app, [], ['disableClientPersistence' => true]);
$m->addSubscriber($sub);
$m->executeCommand('admin', new Command(['ping' => 1]));
$admin = create_test_manager();
configureFailPoint($admin, 'failCommand', ['times' => 2], ['failCommands' => ['ping'], 'closeConnection' => true, 'appName' => $app]);
for ($i = 1; $i <= 3; $i++) {
    try {
        $m->executeCommand('admin', new Command(['ping' => 1]));
        echo "ping $i: ok\n";
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo "ping $i: ", get_class($e), "\n";
    }
    echo "  servers marked Unknown so far: {$sub->unknown}\n";
}
configureFailPoint($admin, 'failCommand', 'off');
?>
--EXPECT--
ping 1: MongoDB\Driver\Exception\ConnectionTimeoutException
  servers marked Unknown so far: 1
ping 2: MongoDB\Driver\Exception\ConnectionTimeoutException
  servers marked Unknown so far: 2
ping 3: ok
  servers marked Unknown so far: 2
