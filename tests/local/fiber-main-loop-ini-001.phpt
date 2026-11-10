--TEST--
Fibers: mongodb.main_event_loop=0 makes {main} calls block even with Revolt loaded; ini_set() switches at once
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). With the setting Off, a
{main} call blocks as with ext-mongodb (a Revolt timer does not run during it); set back On
with ini_set(), the next {main} call waits through the loop again, without any class being
declared in between (the synchronous fast path is re-evaluated when the setting changes).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--INI--
mongodb.main_event_loop=0
--FILE--
<?php
require getenv('REVOLT_AUTOLOAD');
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
use Revolt\EventLoop;
var_dump(ini_get('mongodb.main_event_loop'));
$ns = 'mdrop_local.fiber_main_loop_ini';
$m = new Manager(getenv('MONGODB_URI'));
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_main_loop_ini']));
$bw = new BulkWrite;
$bw->insert(['x' => 1]);
$m->executeBulkWrite($ns, $bw);
var_dump(class_exists(EventLoop::class));

$ticks = 0;
$timer = EventLoop::repeat(0.02, function () use (&$ticks) { $ticks++; });
$slow = new Query(['$where' => 'sleep(200) || true'], ['limit' => 1]);

$m->executeQuery($ns, $slow)->toArray();
echo "Off: timer ran during the {main} call: ", var_export($ticks > 0, true), "\n";

var_dump(ini_set('mongodb.main_event_loop', '1'));
$ticks = 0;
$m->executeQuery($ns, $slow)->toArray();
echo "On: timer ran during the {main} call: ", var_export($ticks > 0, true), "\n";

ini_set('mongodb.main_event_loop', '0');
$ticks = 0;
$m->executeQuery($ns, $slow)->toArray();
echo "Off again: timer ran during the {main} call: ", var_export($ticks > 0, true), "\n";
EventLoop::cancel($timer);
?>
--EXPECT--
string(1) "0"
bool(true)
Off: timer ran during the {main} call: false
string(1) "0"
On: timer ran during the {main} call: true
Off again: timer ran during the {main} call: false
