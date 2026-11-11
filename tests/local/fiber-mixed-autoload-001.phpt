--TEST--
Fibers: Revolt autoloaded by a fiber's driver call, then blocking or loop-waiting {main} calls
--DESCRIPTION--
Local test (not upstream; DIVERGENCES "Driver calls inside a Fiber"). Revolt\EventLoop is not
loaded when the script starts, only autoloadable: {main} calls block; a fiber's call loads it
(the driver looks it up by name, autoloading it in a fiber) and suspends. A {main} call
afterwards blocks with mongodb.main_event_loop=0 (the fiber stays suspended until the loop runs)
and waits through the now loaded loop with the default On (the fiber finishes meanwhile).
--SKIPIF--
<?php if (!getenv('MONGODB_URI')) die('skip no MONGODB_URI'); ?>
<?php if (!getenv('REVOLT_AUTOLOAD') || !is_file(getenv('REVOLT_AUTOLOAD'))) die('skip no REVOLT_AUTOLOAD'); ?>
--FILE--
<?php
use MongoDB\Driver\{Manager, Command, Query, BulkWrite};
// Revolt's classes come from its composer autoloader, registered when first needed
spl_autoload_register(function (string $class) {
    static $loader;
    if (str_starts_with($class, 'Revolt\\')) {
        $loader ??= require getenv('REVOLT_AUTOLOAD');
        $loader->loadClass($class);
    }
});
$uri = getenv('MONGODB_URI');
$ns = 'mdrop_local.fiber_mixed_autoload';
$m = new Manager($uri);
$m->executeCommand('mdrop_local', new Command(['drop' => 'fiber_mixed_autoload']));
$bw = new BulkWrite;
$bw->insert(['_id' => 1, 'x' => 'fiber']);
$bw->insert(['_id' => 2, 'x' => 'main']);
$m->executeBulkWrite($ns, $bw);
$slow = fn (int $ms, int $id) => new Query(['$where' => "sleep($ms) || true", '_id' => $id]);
$x = fn ($cursor) => $cursor->toArray()[0]->x;
$loaded = fn () => var_export(class_exists('Revolt\EventLoop', false), true);

foreach (['0', '1'] as $setting) {
    ini_set('mongodb.main_event_loop', $setting);
    echo "main_event_loop=$setting\n";
    if ($setting === '0') {
        echo "{main} before: ", $x($m->executeQuery($ns, new Query(['_id' => 2]))), ", Revolt loaded: ", $loaded(), "\n";
    }
    $f = new Fiber(fn () => $x($m->executeQuery($ns, $slow(200, 1))));
    $f->start();
    echo "fiber suspended: ", var_export($f->isSuspended(), true), ", Revolt loaded: ", $loaded(), "\n";
    echo "{main}: ", $x($m->executeQuery($ns, $slow(400, 2))), "\n";
    echo "fiber done during the {main} call: ", var_export($f->isTerminated(), true), "\n";
    Revolt\EventLoop::run();
    echo "fiber: ", $f->getReturn(), "\n";
}
?>
--EXPECT--
main_event_loop=0
{main} before: main, Revolt loaded: false
fiber suspended: true, Revolt loaded: true
{main}: main
fiber done during the {main} call: false
fiber: fiber
main_event_loop=1
fiber suspended: true, Revolt loaded: true
{main}: main
fiber done during the {main} call: true
fiber: fiber
