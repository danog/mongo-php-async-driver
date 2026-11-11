--TEST--
Fibers: pooled mode (clusters connect only to the selected server; one shared scan at a time)
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Concurrent operations in fibers"). Needs Revolt: the native classes
(mongodb.native_event_loop=1), or revolt/event-loop with REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has it>.
- 8 fibers at once on a fresh client: the 7 clusters made for them each open one connection, to
  the selected server, with no heartbeats (no scan of their own);
- heartbeatFrequencyMS=500 and rounds of 8 fibers once the topology is stale: one scan per round
  (one heartbeat per server), the other fibers wait for it.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!class_exists('Revolt\EventLoop', false) && !is_file((string) getenv('REVOLT_AUTOLOAD'))) die('skip no Revolt: no native Revolt\EventLoop, no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
if (is_file((string) getenv('REVOLT_AUTOLOAD'))) require getenv('REVOLT_AUTOLOAD'); // else the native classes
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$sub = new class implements MongoDB\Driver\Monitoring\SDAMSubscriber {
    public int $hb = 0;
    public function serverHeartbeatStarted($e): void { $this->hb++; }
    public function serverHeartbeatSucceeded($e): void {}
    public function serverHeartbeatFailed($e): void { echo 'heartbeat failed: ', $e->getError()->getMessage(), "\n"; }
    public function serverChanged($e): void {}
    public function serverClosed($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyClosed($e): void {}
    public function topologyOpening($e): void {}
};
$admin = new Manager($uri);
// the connections of the client whose appName is $app (idle ones included), on every server
$conns = function (string $app) use ($admin): int {
    $n = 0;
    foreach ($admin->getServers() as $s) {
        $n += count($s->executeCommand('admin', new Command(['currentOp' => 1, '$all' => true, 'appName' => $app]))->toArray()[0]->inprog);
    }
    return $n;
};
$admin->executeCommand('admin', new Command(['ping' => 1]));
$ns = 'mdrop_local.fiber_pooled';
$admin->executeCommand('mdrop_local', new Command(['drop' => 'fiber_pooled']));
$bw = new BulkWrite;
$bw->insert(['k' => 1]);
$admin->executeBulkWrite($ns, $bw);

$app = 'pooled1-' . getmypid();
$m = new Manager($uri . $sep . 'appname=' . $app, [], ['disableClientPersistence' => true]);
$m->addSubscriber($sub);
$m->executeCommand('admin', new Command(['ping' => 1]));
$servers = count($m->getServers());
$sub->hb = 0;
$done = 0;
for ($i = 0; $i < 8; $i++) EventLoop::queue(function () use ($m, $ns, &$done) { $m->executeQuery($ns, new Query(['$where' => 'sleep(100) || true']))->toArray(); $done++; });
EventLoop::run();
printf("done=%d heartbeats=%d new connections=%d\n", $done, $sub->hb, $conns($app) - $servers);

$app = 'pooled2-' . getmypid();
$m = new Manager($uri . $sep . 'heartbeatFrequencyMS=500&appname=' . $app, [], ['disableClientPersistence' => true]);
$m->addSubscriber($sub);
$m->executeCommand('admin', new Command(['ping' => 1]));
for ($r = 0; $r < 3; $r++) {
    $sub->hb = 0;
    usleep(700000);
    $done = 0;
    for ($i = 0; $i < 8; $i++) EventLoop::queue(function () use ($m, &$done) { $m->executeCommand('admin', new Command(['ping' => 1])); $done++; });
    EventLoop::run();
    printf("round %d: done=%d heartbeats per server=%s\n", $r, $done, $sub->hb / $servers);
}
?>
--EXPECT--
done=8 heartbeats=0 new connections=7
round 0: done=8 heartbeats per server=1
round 1: done=8 heartbeats per server=1
round 2: done=8 heartbeats per server=1
