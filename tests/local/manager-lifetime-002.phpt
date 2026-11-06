--TEST--
Cursor, Session and WriteResult keep their Manager alive (subscribers, endSessions after unset)
--DESCRIPTION--
Local test (not upstream). As in ext-mongodb, a Cursor, a Session and a WriteResult hold a
reference to their Manager: after `unset($manager)` the Manager's subscribers still receive the
cursor's getMore events, and its disableClientPersistence client sends endSessions (and the
closing SDAM events) when the last of them goes. Same output as the reference.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_not_clean(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

class S implements MongoDB\Driver\Monitoring\CommandSubscriber, MongoDB\Driver\Monitoring\SDAMSubscriber
{
    public function commandStarted($e): void { echo 'started: ', $e->getCommandName(), "\n"; }
    public function commandSucceeded($e): void { echo 'succeeded: ', $e->getCommandName(), "\n"; }
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
$bw = new MongoDB\Driver\BulkWrite();
for ($i = 0; $i < 5; $i++) {
    $bw->insert(['x' => $i]);
}
$m->executeBulkWrite(NS, $bw);
$m->addSubscriber(new S());
$cursor = $m->executeQuery(NS, new MongoDB\Driver\Query([], ['batchSize' => 2, 'sort' => ['x' => 1]]));
unset($m);
echo "unset manager\n";
foreach ($cursor as $d) {
    echo $d->x, "\n";
}
echo "unset cursor\n";
unset($cursor);

echo "--- session\n";
$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$m->addSubscriber(new S());
$s = $m->startSession();
$m->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['ping' => 1]), ['session' => $s]);
unset($m);
echo "unset manager\n";
unset($s);

echo "--- writeresult\n";
$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$m->addSubscriber(new S());
$bw = new MongoDB\Driver\BulkWrite();
$bw->insert(['y' => 1]);
$wr = $m->executeBulkWrite(NS, $bw);
unset($m);
echo "unset manager\n";
var_dump($wr->getServer() instanceof MongoDB\Driver\Server);
unset($wr);
echo "done\n";
?>
--EXPECT--
started: find
succeeded: find
unset manager
0
1
started: getMore
succeeded: getMore
2
3
started: getMore
succeeded: getMore
4
unset cursor
started: endSessions
succeeded: endSessions
topologyClosed
--- session
started: ping
succeeded: ping
unset manager
started: endSessions
succeeded: endSessions
topologyClosed
--- writeresult
started: insert
succeeded: insert
unset manager
bool(true)
started: endSessions
succeeded: endSessions
topologyClosed
done
