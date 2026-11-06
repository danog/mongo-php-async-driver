<?php
// Test tooling: relays a UNIX domain socket to a TCP port, for mongod in Docker on macOS (its
// own UNIX sockets stay inside the VM). php unix-relay.php <socket path> <tcp port>
[, $path, $port] = $argv;
@unlink($path);
$server = stream_socket_server("unix://$path", $errno, $errstr) or exit("relay: $errstr\n");
$peer = []; // id => the other end
$all = [(int) $server => $server];
while (true) {
    $read = array_values($all);
    $w = $e = null;
    if (stream_select($read, $w, $e, null) === false) {
        exit(1);
    }
    foreach ($read as $s) {
        if ($s === $server) {
            $c = stream_socket_accept($server);
            $t = @stream_socket_client("tcp://127.0.0.1:$port");
            if (!$c || !$t) {
                $c && fclose($c);
                continue;
            }
            $all[(int) $c] = $c;
            $all[(int) $t] = $t;
            $peer[(int) $c] = $t;
            $peer[(int) $t] = $c;
            continue;
        }
        $data = fread($s, 65536);
        $o = $peer[(int) $s] ?? null;
        if ($data === '' || $data === false || $o === null || fwrite($o, $data) === false) {
            foreach ([$s, $o] as $x) {
                if ($x) {
                    unset($all[(int) $x], $peer[(int) $x]);
                    fclose($x);
                }
            }
        }
    }
}
