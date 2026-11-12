--TEST--
Fibers: blocking {main} calls (mongodb.main_event_loop=0) while fibers wait for their replies
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). With the setting Off, a
{main} call takes the plain blocking path even though Revolt is loaded, while fibers still wait
through the loop. Here fibers (a plain Fiber, and one the loop started) are suspended waiting for
a reply that arrives while {main} blocks in calls on the same Manager and on another one; the
loop is not running then, so the fibers stay suspended, and once it runs they get their replies.
The blocking waits never take the readiness the loop's poller reports for the fibers'
sockets (each round's clusters are the previous round's, so {main} also uses sockets fibers waited
on before). With the setting On (the default), the same {main} calls wait through the loop and
the fibers finish during them.
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$ns = 'mdrop_local.fiber_mixed_main';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_main']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'fiber']);
$bw->insert(['_id' => 2, 'x' => 'main']);
$m->executeBulkWrite($ns, $bw);
$other = new Manager($uri, ['appname' => 'fiber-mixed-main']);
$slow = fn (int $ms, int $id) => new Query(['$where' => "sleep($ms) || true", '_id' => $id]);
$x = fn ($cursor) => $cursor->toArray()[0]->x;

foreach (['0', '1'] as $setting) {
    ini_set('mongodb.main_event_loop', $setting);
    echo "main_event_loop=$setting\n";
    for ($round = 0; $round < 3; $round++) {
        // a plain Fiber, suspended in its wait for the reply (200 ms)
        $plain = new Fiber(fn () => $x($m->executeQuery($ns, $slow(200, 1))));
        $plain->start();
        // a fiber the loop starts, while {main} waits on its own suspension (as Amp's delay())
        $queued = null;
        EventLoop::queue(function () use (&$queued, $m, $ns, $slow, $x) {
            $queued = $x($m->executeQuery($ns, $slow(200, 1)));
        });
        $s = EventLoop::getSuspension();
        EventLoop::delay(0.01, fn () => $s->resume());
        $s->suspend();
        $suspended = $plain->isSuspended() && $queued === null;

        $ticks = 0;
        $timer = EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; });
        // the fibers' replies arrive during these calls
        $main = [$x($m->executeQuery($ns, $slow(400, 2))), $x($other->executeQuery($ns, new Query(['_id' => 2])))];
        $main[] = $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok;
        EventLoop::cancel($timer);
        $during = $plain->isTerminated() && $queued !== null;
        if ($round === 0) {
            echo "fibers suspended before the {main} calls: ", var_export($suspended, true), "\n";
            echo "the loop ran during the {main} calls: ", var_export($ticks > 0, true), "\n";
            echo "fibers done during the {main} calls: ", var_export($during, true), "\n";
            echo "{main}: ", json_encode($main), "\n";
        }
        $t = hrtime(true);
        EventLoop::run();
        $ms = (hrtime(true) - $t) / 1e6;
        echo "round $round: ", $plain->getReturn(), ' ', $queued, $ms < 150 ? '' : " (the loop took $ms ms)", "\n";
    }
}
?>
--EXPECT--
main_event_loop=0
fibers suspended before the {main} calls: true
the loop ran during the {main} calls: false
fibers done during the {main} calls: false
{main}: ["main","main",1]
round 0: fiber fiber
round 1: fiber fiber
round 2: fiber fiber
main_event_loop=1
fibers suspended before the {main} calls: true
the loop ran during the {main} calls: true
fibers done during the {main} calls: true
{main}: ["main","main",1]
round 0: fiber fiber
round 1: fiber fiber
round 2: fiber fiber
