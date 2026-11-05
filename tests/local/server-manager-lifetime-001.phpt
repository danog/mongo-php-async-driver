--TEST--
Server: keeps the client alive, not the Manager object
--DESCRIPTION--
Local test (not upstream). Listed in DIVERGENCES.md (temporary gaps). In ext-mongodb a Server
holds a reference to its Manager, so the Manager outlives `unset($manager)`: its subscribers keep
receiving events, and a disableClientPersistence client is destroyed (topologyClosed) only when
the Server goes. Here a Server holds the client only: the Manager is freed at once, its
subscribers receive nothing more, and the client is dropped without endSessions and without
the closing events. The reference prints "started: ping" after "unset", and
"started: endSessions" and "topologyClosed" after "end".
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

class S implements MongoDB\Driver\Monitoring\CommandSubscriber, MongoDB\Driver\Monitoring\SDAMSubscriber
{
    public function commandStarted($e): void { echo 'started: ', $e->getCommandName(), "\n"; }
    public function commandSucceeded($e): void {}
    public function commandFailed($e): void {}
    public function serverChanged($e): void {}
    public function serverClosed($e): void {}
    public function serverHeartbeatFailed($e): void {}
    public function serverHeartbeatStarted($e): void {}
    public function serverHeartbeatSucceeded($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyClosed($e): void { echo "topologyClosed\n"; }
    public function topologyOpening($e): void {}
}

$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$m->addSubscriber(new S());
$server = $m->selectServer();
unset($m);
echo "unset\n";
$server->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
echo "end\n";
unset($server);
echo "done\n";
?>
--EXPECT--
unset
end
done
