--TEST--
Fibers: the backoff before an overload retry suspends the fiber
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). A fiber's command fails four
times with SystemOverloadedError/RetryableError (failpoint), so it is retried four times
(maxAdaptiveRetries=4), each after a random pause of up to 100, 200, 400 and 800 ms. The pauses
suspend the fiber: a 5 ms repeat timer keeps running meanwhile (it used to stop for each pause,
as the pause blocked the thread).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
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
use MongoDB\Driver\{Manager, Command};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$admin = new Manager($uri);
$m = new Manager($uri, ['appname' => 'fiber-overload-backoff', 'maxAdaptiveRetries' => 4], ['disableClientPersistence' => true]);
$m->executeCommand('admin', new Command(['ping' => 1]));
$admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => ['times' => 4], 'data' => ['failCommands' => ['ping'], 'errorCode' => 462, 'errorLabels' => ['SystemOverloadedError', 'RetryableError'], 'appName' => 'fiber-overload-backoff']]));
$last = hrtime(true);
$gap = 0;
$timer = EventLoop::repeat(0.005, function () use (&$last, &$gap) {
    $now = hrtime(true);
    $gap = max($gap, $now - $last);
    $last = $now;
});
EventLoop::queue(function () use ($m, $timer) {
    echo "ping: ", $m->executeCommand('admin', new Command(['ping' => 1]))->toArray()[0]->ok, "\n";
    EventLoop::cancel($timer);
});
EventLoop::run();
// a pause that blocked shows as one gap of its length (the chance that all four are below the
// bound is under 1%); suspended, the timer runs every few ms
$ms = intdiv($gap, 1000000);
echo "longest timer gap below 80 ms: ", var_export($ms < 80, true), $ms < 80 ? '' : " ($ms ms)", "\n";
$admin->executeCommand('admin', new Command(['configureFailPoint' => 'failCommand', 'mode' => 'off']));
?>
--EXPECT--
ping: 1
longest timer gap below 80 ms: true
