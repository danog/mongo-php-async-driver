--TEST--
Fibers: a reply that is there when the deadline timer runs first is not a timeout
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). A fiber's reply arrives
in time, but a blocking {main} call (mongodb.main_event_loop=0) keeps the loop from running until
after the fiber's deadline (socketTimeoutMS=300). A loop may then run the deadline timer before
the watcher that reports the reply (Revolt's EvDriver does; here the watcher is disabled for
that tick, as the StreamSelectDriver runs watchers first): the timer finds the reply and the
fiber gets it, not a socket timeout. Unix only (the epoll/kqueue backend; on Windows each wait
has watchers of its own).
--SKIPIF--
<?php if (PHP_OS_FAMILY === 'Windows') die('skip not on Windows'); ?>
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--INI--
mongodb.main_event_loop=0
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
use Revolt\EventLoop\CallbackType;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$ns = 'mdrop_local.fiber_mixed_timeout_timer';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_timeout_timer']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'fiber']);
$bw->insert(['_id' => 2, 'x' => 'main']);
$m->executeBulkWrite($ns, $bw);
$short = new Manager($uri . $sep . 'socketTimeoutMS=300&retryReads=false');
$short->executeCommand('admin', new Command(['ping' => 1]));
$slow = fn (int $ms, int $id) => new Query(['$where' => "sleep($ms) || true", '_id' => $id]);

$f = new Fiber(function () use ($short, $ns, $slow) {
    try {
        return $short->executeQuery($ns, $slow(50, 1))->toArray()[0]->x;
    } catch (Exception $e) {
        return get_class($e) . ': ' . $e->getMessage();
    }
});
$f->start();
echo "{main}: ", $m->executeQuery($ns, $slow(600, 2))->toArray()[0]->x, "\n";
// the driver's watcher (the only readable one) misses the first tick: the deadline timer runs first
$watchers = array_values(array_filter(EventLoop::getIdentifiers(), fn ($id) => EventLoop::getType($id) === CallbackType::Readable && EventLoop::isEnabled($id)));
echo "watchers: ", count($watchers), "\n";
EventLoop::disable($watchers[0]);
EventLoop::delay(0.05, function () use ($watchers) { EventLoop::enable($watchers[0]); });
EventLoop::run();
echo "fiber: ", $f->getReturn(), "\n";
?>
--EXPECT--
{main}: main
watchers: 1
fiber: fiber
