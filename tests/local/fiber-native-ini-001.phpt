--TEST--
Fibers: mongodb.native_event_loop=0 registers no Revolt classes; the setting is PHP_INI_SYSTEM
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). With the setting Off the
extension registers no Revolt\EventLoop (nor any other Revolt class) and never uses its native
loop: Revolt is the PHP library (or the revolt extension), reached through PHP calls. It can
only be set at startup (php.ini, -d): ini_set() fails.
--SKIPIF--
<?php if (extension_loaded('revolt')) die('skip the revolt extension declares Revolt\EventLoop'); ?>
--INI--
mongodb.native_event_loop=0
--FILE--
<?php
var_dump(ini_get('mongodb.native_event_loop'));
var_dump(ini_set('mongodb.native_event_loop', '1'));
var_dump(ini_get('mongodb.native_event_loop'));
var_dump(ini_get_all('mongodb')['mongodb.native_event_loop']['access'] === INI_SYSTEM);
var_dump(class_exists('Revolt\EventLoop', false), interface_exists('Revolt\EventLoop\Driver', false));
var_dump(array_values(array_filter(get_declared_classes(), fn ($c) => str_starts_with($c, 'Revolt\\'))));
?>
--EXPECT--
string(1) "0"
bool(false)
string(1) "0"
bool(true)
bool(false)
bool(false)
array(0) {
}
