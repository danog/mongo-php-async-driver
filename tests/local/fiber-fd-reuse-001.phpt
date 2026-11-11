--TEST--
Fibers: a new connection whose socket reuses the number of a closed one still gets its replies
--DESCRIPTION--
Local test (not upstream). Needs Revolt: the native classes
(mongodb.native_event_loop=1), or revolt/event-loop with REVOLT_AUTOLOAD=<path to a composer vendor/autoload.php that has it>.
Sockets stay registered with the thread's epoll/kqueue instance from their first wait until they
close, so a closed socket must be forgotten there: else a new socket that gets the same number is
taken for registered and its waits never end. Rounds of three fibers, each with its own Manager
(no persistence: its server check and command connections close with it); the sockets of a round
get the numbers the previous round freed. socketTimeoutMS / connectTimeoutMS bound a missed
readiness (the test then prints the error instead of hanging).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!class_exists('Revolt\EventLoop', false) && !is_file((string) getenv('REVOLT_AUTOLOAD'))) die('skip no Revolt: no native Revolt\EventLoop, no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
if (is_file((string) getenv('REVOLT_AUTOLOAD'))) require getenv('REVOLT_AUTOLOAD'); // else the native classes
use MongoDB\Driver\{Manager, Command};
use Revolt\EventLoop;
$uri = getenv('MONGODB_URI');
$sep = str_contains($uri, '?') ? '&' : (str_ends_with($uri, '/') ? '?' : '/?');
$uri .= $sep . 'socketTimeoutMS=3000&connectTimeoutMS=3000&serverSelectionTimeoutMS=5000';
$ok = 0;
for ($round = 0; $round < 10; $round++) {
    for ($i = 0; $i < 3; $i++) {
        EventLoop::queue(function () use ($uri, &$ok) {
            try {
                $m = new Manager($uri, [], ['disableClientPersistence' => true]);
                for ($k = 0; $k < 3; $k++) {
                    $m->executeCommand('admin', new Command(['ping' => 1]));
                }
                $ok++;
            } catch (Throwable $e) {
                echo get_class($e), ': ', $e->getMessage(), "\n";
            }
        });
    }
    EventLoop::run();
}
// {main} with Revolt loaded waits through the loop as well
for ($round = 0; $round < 5; $round++) {
    $m = new Manager($uri, [], ['disableClientPersistence' => true]);
    $m->executeCommand('admin', new Command(['ping' => 1]));
    unset($m);
    $ok++;
}
echo "ok=$ok\n";
?>
--EXPECT--
ok=35
