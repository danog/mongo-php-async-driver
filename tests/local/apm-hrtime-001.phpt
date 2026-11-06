--TEST--
APM and LogSubscriber: the event's hrtime timestamp reaches only the signatures that accept it
--DESCRIPTION--
Local test (not upstream). Events are delivered after the operation (see "APM delivery time" in
DIVERGENCES.md), so each call carries one extra trailing int: hrtime(true) at the moment the
event (or log message) was created. It is passed only when the method declares exactly the
interface's parameters, or ends in a variadic that accepts an int (untyped, mixed, int or a union
with int), and is not passed by reference; every other signature is called with the interface's
arguments only, as the reference calls all of them (no TypeError, no default replaced). The
reference passes no extra argument to any of them.
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
<?php skip_if_load_balanced(); ?>
--FILE--
<?php
declare(strict_types=1);

require_once __DIR__ . "/../phpt/utils/basic.inc";

use MongoDB\Driver\Monitoring\{CommandFailedEvent, CommandStartedEvent, CommandSubscriber, CommandSucceededEvent};
use MongoDB\Driver\Monitoring\{LogSubscriber, SDAMSubscriber, TopologyClosedEvent};

abstract class Recorder
{
    public array $seen = [];
    protected function note(string $method, array $args): void
    {
        // an hrtime(true) value is far above any default the signatures here declare
        $this->seen[$method] = array_map(fn ($a) => is_int($a) && $a > 1000000 ? 'hrtime' : get_debug_type($a) . ':' . var_export($a, true), array_slice($args, 1));
        if (isset($args[1]) && is_int($args[1]) && $args[1] > 1000000) {
            $GLOBALS['ts'][static::class][$method] = $args[1];
        }
    }
}

final class Plain extends Recorder implements CommandSubscriber
{
    public function commandStarted(CommandStartedEvent $event): void { $this->note(__FUNCTION__, func_get_args()); $GLOBALS['dur'] = null; }
    public function commandSucceeded(CommandSucceededEvent $event): void { $this->note(__FUNCTION__, func_get_args()); $GLOBALS['dur'] = $event->getDurationMicros(); }
    public function commandFailed(CommandFailedEvent $event): void { $this->note(__FUNCTION__, func_get_args()); }
}
final class VarUntyped extends Recorder implements CommandSubscriber
{
    public function commandStarted(...$a): void { $this->note(__FUNCTION__, $a); }
    public function commandSucceeded(...$a): void { $this->note(__FUNCTION__, $a); }
    public function commandFailed(...$a): void { $this->note(__FUNCTION__, $a); }
}
final class VarMixed extends Recorder implements CommandSubscriber
{
    public function commandStarted(CommandStartedEvent $event, mixed ...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
    public function commandSucceeded(CommandSucceededEvent $event, int|string ...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
    public function commandFailed(CommandFailedEvent $event, mixed ...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
}
final class VarTyped extends Recorder implements CommandSubscriber
{
    public function commandStarted(CommandStartedEvent ...$a): void { $this->note(__FUNCTION__, $a); }
    public function commandSucceeded(CommandSucceededEvent ...$a): void { $this->note(__FUNCTION__, $a); }
    public function commandFailed(CommandFailedEvent ...$a): void { $this->note(__FUNCTION__, $a); }
}
final class VarByRef extends Recorder implements CommandSubscriber
{
    public function commandStarted(CommandStartedEvent $event, &...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
    public function commandSucceeded(CommandSucceededEvent $event, &...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
    public function commandFailed(CommandFailedEvent $event, &...$a): void { $this->note(__FUNCTION__, [$event, ...$a]); }
}
final class Optional extends Recorder implements CommandSubscriber
{
    public function commandStarted(CommandStartedEvent $event, array $x = [], ?stdClass $y = null): void { $this->note(__FUNCTION__, [$event, $x, $y]); }
    public function commandSucceeded(CommandSucceededEvent $event, int $x = 0, string $y = 'y', bool $z = false): void { $this->note(__FUNCTION__, [$event, $x, $y, $z]); }
    public function commandFailed(CommandFailedEvent $event, string $x = 'x'): void { $this->note(__FUNCTION__, [$event, $x]); }
}
final class Logs extends Recorder implements LogSubscriber
{
    public function log(int $level, string $domain, string $message): void { $this->note('plain', [null, ...array_slice(func_get_args(), 3)]); }
}
final class LogsTyped extends Recorder implements LogSubscriber
{
    public function log(int $level, string ...$parts): void { $this->note('typed', [null, ...array_slice($parts, 2)]); }
}
final class LogsOptional extends Recorder implements LogSubscriber
{
    public function log(int $level, string $domain, string $message, ?array $x = null): void { $this->note('optional', [null, $x]); }
}
final class Closed extends Recorder implements SDAMSubscriber
{
    public function __construct(private bool $typed) {}
    public function serverChanged($e): void {}
    public function serverClosed($e): void {}
    public function serverHeartbeatFailed($e): void {}
    public function serverHeartbeatStarted($e): void {}
    public function serverHeartbeatSucceeded($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyOpening($e): void {}
    public function topologyClosed($e, ...$a): void { $this->note($this->typed ? 'never' : 'topologyClosed', [$e, ...$a]); }
}
final class ClosedTyped extends Recorder implements SDAMSubscriber
{
    public function serverChanged($e): void {}
    public function serverClosed($e): void {}
    public function serverHeartbeatFailed($e): void {}
    public function serverHeartbeatStarted($e): void {}
    public function serverHeartbeatSucceeded($e): void {}
    public function serverOpening($e): void {}
    public function topologyChanged($e): void {}
    public function topologyOpening($e): void {}
    public function topologyClosed(TopologyClosedEvent $e, string $s = 's'): void { $this->note('topologyClosed', [$e, $s]); }
}

$subscribers = [new Plain, new VarUntyped, new VarMixed, new VarTyped, new VarByRef, new Optional];
$manager = create_test_manager(URI, [], ['disableClientPersistence' => true]);
foreach ($subscribers as $s) {
    $manager->addSubscriber($s);
}
$t0 = hrtime(true);
$manager->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['ping' => 1]));
$t1 = hrtime(true);
[$started, $succeeded] = [$ts['Plain']['commandStarted'], $ts['Plain']['commandSucceeded']];
var_dump($t0 < $started, $started < $succeeded, $succeeded < $t1);
// both stamps are taken around the command: their gap is the measured duration (within 1 ms)
var_dump(abs(($succeeded - $started) / 1000 - $dur) < 1000);
foreach (['VarUntyped', 'VarMixed'] as $c) {
    var_dump($t0 < $ts[$c]['commandStarted'] && $ts[$c]['commandSucceeded'] < $t1);
}
try {
    $manager->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['nosuchcommand' => 1]));
} catch (MongoDB\Driver\Exception\CommandException $e) {
}
foreach ($subscribers as $s) {
    echo get_class($s), ': ', json_encode($s->seen), "\n";
}

$loggers = [new Logs, new LogsTyped, new LogsOptional];
foreach ($loggers as $l) {
    MongoDB\Driver\Monitoring\addSubscriber($l);
}
$t0 = hrtime(true);
MongoDB\Driver\Monitoring\mongoc_log(MongoDB\Driver\Monitoring\LogSubscriber::LEVEL_WARNING, 'test', 'message');
$t1 = hrtime(true);
foreach ($loggers as $l) {
    echo get_class($l), ': ', json_encode($l->seen), "\n";
    MongoDB\Driver\Monitoring\removeSubscriber($l);
}
var_dump($t0 < $ts['Logs']['plain'] && $ts['Logs']['plain'] < $t1);

// a flush from the free handler: unset() in this strict_types file is the calling frame
$closed = [new Closed(false), new ClosedTyped];
$manager = create_test_manager(URI, [], ['disableClientPersistence' => true]);
foreach ($closed as $s) {
    MongoDB\Driver\Monitoring\addSubscriber($s);
}
$manager->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['ping' => 1]));
$t0 = hrtime(true);
unset($manager);
$t1 = hrtime(true);
foreach ($closed as $s) {
    echo get_class($s), ': ', json_encode($s->seen), "\n";
    MongoDB\Driver\Monitoring\removeSubscriber($s);
}
var_dump($t0 < $ts['Closed']['topologyClosed'] && $ts['Closed']['topologyClosed'] < $t1);
?>
===DONE===
<?php exit(0); ?>
--EXPECT--
bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
Plain: {"commandStarted":["hrtime"],"commandSucceeded":["hrtime"],"commandFailed":["hrtime"]}
VarUntyped: {"commandStarted":["hrtime"],"commandSucceeded":["hrtime"],"commandFailed":["hrtime"]}
VarMixed: {"commandStarted":["hrtime"],"commandSucceeded":["hrtime"],"commandFailed":["hrtime"]}
VarTyped: {"commandStarted":[],"commandSucceeded":[],"commandFailed":[]}
VarByRef: {"commandStarted":[],"commandSucceeded":[],"commandFailed":[]}
Optional: {"commandStarted":["array:array (\n)","null:NULL"],"commandSucceeded":["int:0","string:'y'","bool:false"],"commandFailed":["string:'x'"]}
Logs: {"plain":["hrtime"]}
LogsTyped: {"typed":[]}
LogsOptional: {"optional":["null:NULL"]}
bool(true)
Closed: {"topologyClosed":["hrtime"]}
ClosedTyped: {"topologyClosed":["string:'s'"]}
bool(true)
===DONE===
