--TEST--
Logging: no password, secret option, client hash or authentication payload reaches any log output
--DESCRIPTION--
Local test (not upstream). ext-mongodb logs, at TRACE level, hex dumps of every wire message
(authentication conversations, createUser passwords); before 2.5.8 (PHPC-2804) it also logged the
URI as given and the persistent-client hash (the URI's SHA-256 and every option value). Here no
wire dump is produced, and the client messages are 2.5.8's, without the hash. The test captures
the mongodb.debug stream and a LogSubscriber, runs failing and passing operations with secrets in
the URI, the URI options and a command, and checks that no secret appears. Listed in
DIVERGENCES.md (permanent, for safety).
--SKIPIF--
<?php require __DIR__ . "/../phpt/utils/basic-skipif.inc"; ?>
<?php skip_if_not_live(); ?>
--FILE--
<?php
require_once __DIR__ . "/../phpt/utils/basic.inc";

use MongoDB\Driver\Monitoring\LogSubscriber;

class Collect implements LogSubscriber
{
    public array $lines = [];

    public function log(int $level, string $domain, string $message): void
    {
        $this->lines[] = "$level $domain $message";
    }
}

$secrets = ['uriS3cr3t', 'optS3cr3t', 'pkS3cr3t', 'tokS3cr3t', 'cmdS3cr3t', 'drvS3cr3t'];

$dir = sys_get_temp_dir() . '/mdrop-log-' . getmypid();
@mkdir($dir);
ini_set('mongodb.debug', $dir);
$logger = new Collect;
MongoDB\Driver\Monitoring\addSubscriber($logger);

$host = parse_url(URI, PHP_URL_HOST) . ':' . (parse_url(URI, PHP_URL_PORT) ?: 27017);

// SCRAM against a server without that user: authentication fails and is logged
$m = new MongoDB\Driver\Manager("mongodb://alice:uriS3cr3t@$host/?authSource=admin&serverSelectionTryOnce=true", [], ['disableClientPersistence' => true]);
try {
    $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
} catch (MongoDB\Driver\Exception\Exception $e) {
    echo get_class($e), "\n";
}
// credentials in the URI options, a persistent client (hash logged), found again
for ($i = 0; $i < 2; $i++) {
    $m = new MongoDB\Driver\Manager("mongodb://$host/", ['username' => 'bob', 'password' => 'optS3cr3t'], ['driver' => ['name' => 'drvS3cr3t-not-secret']]);
    try {
        $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo get_class($e), "\n";
    }
}
// secret options that never connect (TLS key password, AWS session token)
foreach (["mongodb://$host/?tlsCertificateKeyFilePassword=pkS3cr3t", "mongodb://k@$host/?authMechanism=MONGODB-AWS&authMechanismProperties=AWS_SESSION_TOKEN:tokS3cr3t"] as $uri) {
    try {
        $m = new MongoDB\Driver\Manager($uri);
        $m->executeCommand('admin', new MongoDB\Driver\Command(['ping' => 1]));
    } catch (MongoDB\Driver\Exception\Exception $e) {
        echo get_class($e), "\n";
    }
}
// a command carrying a secret, on a working connection
$m = create_test_manager();
try {
    $m->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['createUser' => 'carol' . getmypid(), 'pwd' => 'cmdS3cr3t', 'roles' => []]));
    $m->executeCommand(DATABASE_NAME, new MongoDB\Driver\Command(['dropUser' => 'carol' . getmypid()]));
} catch (MongoDB\Driver\Exception\Exception $e) {
    echo get_class($e), "\n";
}
unset($m);

ini_set('mongodb.debug', '');
MongoDB\Driver\Monitoring\removeSubscriber($logger);

$stream = '';
foreach (glob($dir . '/*') as $f) {
    $stream .= file_get_contents($f);
    unlink($f);
}
rmdir($dir);
$all = $stream . "\n" . implode("\n", $logger->lines);

printf("stream has lines: %s\n", substr_count($stream, "\n") > 5 ? 'yes' : 'no');
printf("connection string logged: %s\n", str_contains($stream, 'Connection string') ? 'yes' : 'no');
printf("non-persistent client logged: %s\n", str_contains($stream, "> Created non-persistent client\n") ? 'yes' : 'no');
printf("persistent client logged: %s\n", str_contains($stream, "> Created persistent client\n") && str_contains($stream, "> Stored persistent client\n") ? 'yes' : 'no');
printf("found client logged: %s\n", str_contains($stream, "> Found persistent client\n") ? 'yes' : 'no');
printf("authentication failure logged: %s\n", str_contains($all, 'cluster ERROR') || str_contains($stream, 'cluster: ERROR   > Authentication failed') ? 'yes' : 'no');
printf("subscriber saw messages: %s\n", count($logger->lines) > 5 ? 'yes' : 'no');
// the driver name is not a secret: it shows that option values are searched for at all
printf("driver name logged: %s\n", str_contains($all, 'drvS3cr3t-not-secret') ? 'yes' : 'no');
foreach (array_slice($secrets, 0, 5) as $s) {
    printf("%s: %s\n", $s, str_contains($all, $s) ? 'LEAKED' : 'absent');
}
?>
===DONE===
--EXPECTF--
MongoDB\Driver\Exception\AuthenticationException
MongoDB\Driver\Exception\AuthenticationException
MongoDB\Driver\Exception\AuthenticationException
MongoDB\Driver\Exception\%s
MongoDB\Driver\Exception\%s
stream has lines: yes
connection string logged: no
non-persistent client logged: yes
persistent client logged: yes
found client logged: yes
authentication failure logged: yes
subscriber saw messages: yes
driver name logged: yes
uriS3cr3t: absent
optS3cr3t: absent
pkS3cr3t: absent
tokS3cr3t: absent
cmdS3cr3t: absent
===DONE===
