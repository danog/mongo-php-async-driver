--TEST--
Manager: a reference cycle through a subscriber is collected, without calling it
--DESCRIPTION--
Local test (not upstream). Listed in DIVERGENCES.md. A Manager holds its subscribers, and a
Server, Cursor, Session or WriteResult holds its Manager, so a subscriber that keeps one of them
(or the Manager) forms a cycle. ext-mongodb reports none of these references to the garbage
collector: such a cycle is never collected (gc_collect_cycles() returns 0) and lives until the
end of the request. Here they are reported, so the cycle is collected. While the collector
frees it, no subscriber is called (they may be part of the cycle): the closing events of the
cycle's disableClientPersistence client are not delivered.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

class S implements MongoDB\Driver\Monitoring\CommandSubscriber, MongoDB\Driver\Monitoring\SDAMSubscriber
{
    public $keep;
    public function __construct(private string $name) {}
    public function commandStarted($e): void { echo $this->name, ' started: ', $e->getCommandName(), "\n"; }
    public function commandSucceeded($e): void {}
    public function commandFailed($e): void {}
    public function serverChanged($e): void {}
    public function serverClosed($e): void {}
    public function serverHeartbeatFailed($e): void {}
    public function serverHeartbeatStarted($e): void {}
    public function serverHeartbeatSucceeded($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyClosed($e): void { echo $this->name, " topologyClosed\n"; }
    public function topologyOpening($e): void {}
}

gc_collect_cycles();

// subscriber -> Server -> Manager -> subscriber
$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$sub = new S('a');
$m->addSubscriber($sub);
$sub->keep = $m->selectServer();
unset($m, $sub);
var_dump(gc_collect_cycles() > 0);

// subscriber -> Cursor (with its implicit Session) -> Manager -> subscriber
$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$sub = new S('b');
$m->addSubscriber($sub);
$sub->keep = $m->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['ping' => 1]));
unset($m, $sub);
var_dump(gc_collect_cycles() > 0);

// subscriber -> Manager -> subscriber
$m = new MongoDB\Driver\Manager(URI, [], ['disableClientPersistence' => true]);
$sub = new S('c');
$m->addSubscriber($sub);
$sub->keep = $m;
unset($m, $sub);
var_dump(gc_collect_cycles() > 0);
echo "done\n";
?>
--EXPECT--
bool(true)
b started: ping
bool(true)
bool(true)
done
