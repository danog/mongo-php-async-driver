--TEST--
Fibers: a blocking {main} call while a suspended fiber runs the client's first scan
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber", "Concurrent operations in
fibers"). A fresh client: the first fiber's operation starts the scan and suspends in it (connect
or hello), a second fiber waits for that scan. Server scans are shared, one at a time, but a
{main} call with mongodb.main_event_loop=0 cannot wait for a scan whose fiber cannot run while it
blocks: it scans on its own (no deadlock) and succeeds, and the fibers' scan still counts as
running: a third fiber needing a scan afterwards (heartbeatFrequencyMS=500 passed meanwhile)
waits for it instead of starting another one. Once the loop runs, every fiber
succeeds. Heartbeats: the first fiber's scan and {main}'s, one per server each.
With the setting On, {main} waits for the fibers' scan through the loop, and the third fiber
scans.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$servers = count((new Manager($uri))->selectServer()->getInfo()['hosts'] ?? [0]);
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
$ping = fn (Manager $m) => $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;

foreach (['0', '1'] as $setting) {
    ini_set('mongodb.main_event_loop', $setting);
    echo "main_event_loop=$setting\n";
    $sub->hb = 0;
    $m = new Manager($uri . $sep . 'heartbeatFrequencyMS=500', ['appname' => "fiber-mixed-scan-$setting"], ['disableClientPersistence' => true]);
    $m->addSubscriber($sub);
    $fibers = [];
    for ($i = 0; $i < 2; $i++) {
        $fibers[$i] = new Fiber(fn () => $ping($m));
        $fibers[$i]->start();
    }
    echo "fibers suspended: ", var_export($fibers[0]->isSuspended() && $fibers[1]->isSuspended(), true), "\n";
    echo "{main}: ", $ping($m), "\n";
    // {main} goes on without the loop until the topology is stale
    usleep(600_000);
    $fibers[2] = new Fiber(fn () => $ping($m));
    $fibers[2]->start();
    EventLoop::run();
    foreach ($fibers as $i => $f) {
        echo "fiber $i: ", $f->getReturn(), "\n";
    }
    echo "{main} after: ", $ping($m), "\n";
    echo "scans: ", $sub->hb / $servers, "\n";
}
?>
--EXPECT--
main_event_loop=0
fibers suspended: true
{main}: 1
fiber 0: 1
fiber 1: 1
fiber 2: 1
{main} after: 1
scans: 2
main_event_loop=1
fibers suspended: true
{main}: 1
fiber 0: 1
fiber 1: 1
fiber 2: 1
{main} after: 1
scans: 2
