#!/bin/bash
# Test topologies for the PHPT/PHPLIB suites: topo.sh start|stop standalone|replset|sharded|all
#   standalone  mongodb://127.0.0.1:28010, also through the UNIX domain socket /tmp/mongodb-28010.sock (mongod's own;
#               with Docker a relay, tools/unix-relay.php)
#   replset     mongodb://127.0.0.1:28011,127.0.0.1:28012,127.0.0.1:28013/?replicaSet=rs0
#   sharded     mongodb://127.0.0.1:28030,127.0.0.1:28031 (2 mongos; config rs cfg 28020, shards sh1 28021, sh2 28022)
#   auth        mongodb://root:toor@127.0.0.1:28015/?authSource=admin (standalone with --auth; users root/toor,
#               and the auth specification's test users: user/pencil, sha1/sha1, sha256/sha256, both/both, IX/IX,
#               \u2168/\u2163, created through the localhost exception)
#   tls         mongodb://127.0.0.1:28016/?tls=true&tlsCAFile=$SSL_DIR/ca.pem (standalone with --tlsMode requireTLS,
#               server.pem of tools/tls-certs.sh $SSL_DIR; client certificates of ca.pem are accepted, not required);
#               28017 the same with server-ip.pem (valid for 127.0.0.2 only)
#   x509        mongodb://127.0.0.1:28014/?tls=true&tlsCAFile=$SSL_DIR/ca.pem&tlsCertificateKeyFile=$SSL_DIR/client.pem
#               &authMechanism=MONGODB-X509 (--auth, TLS as tls; mechanisms MONGODB-X509, PLAIN, SCRAM; users root/toor
#               and the client.pem subject CN=client,OU=drivers,O=mdrop-client in $external)
#   compress    mongodb://127.0.0.1:28018/?compressors=zstd (standalone with --networkMessageCompressors
#               snappy,zstd,zlib); 28019 the same with compression disabled
# Env: MONGO_BIN (dir with mongod/mongos), TOPO_DATA (data dir), TOPO_PHP (a PHP with a mongodb extension, to initiate).
#      TOPO_DOCKER=1 (default on macOS): every mongod/mongos runs in its own container (image TOPO_IMAGE,
#      default mongo:8.0) on the host network, so the addresses are the same; containers are named
#      mdrop-<name> and removed when stopped (data lives in the container).
set -e
BIN=${MONGO_BIN:-/zf/async/mongo/bin}
DATA=${TOPO_DATA:-/zf/mdrop/data/topo}
PHPI=${TOPO_PHP:-php8.4 -n -d extension=mongodb}
H=127.0.0.1
DOCKER=${TOPO_DOCKER:-$([ "$(uname)" = Darwin ] && echo 1 || echo 0)}
IMAGE=${TOPO_IMAGE:-mongo:8.0}
RELAY_PID=/tmp/mdrop-unix-relay-28010.pid

wait_port() { # port
  for _ in $(seq 1 120); do nc -z $H "$1" 2>/dev/null && return 0; sleep 0.25; done
  echo "port $1 not listening" >&2; return 1
}
mongod_start() { # name port extra...
  local name=$1 port=$2; shift 2
  if [ "$DOCKER" = 1 ]; then
    docker run -d --rm --name "mdrop-$name" --network host ${DOCKER_EXTRA:-} "$IMAGE" mongod --port "$port" --bind_ip $H \
      --wiredTigerCacheSizeGB 0.5 --setParameter enableTestCommands=1 "$@" >/dev/null
    wait_port "$port"
    return
  fi
  mkdir -p "$DATA/$name"
  "$BIN/mongod" --dbpath "$DATA/$name" --port "$port" --bind_ip $H --fork --logpath "$DATA/$name.log" \
    --wiredTigerCacheSizeGB 0.5 --setParameter enableTestCommands=1 "$@" >/dev/null
}
mongos_start() { # port
  if [ "$DOCKER" = 1 ]; then
    docker run -d --rm --name "mdrop-mongos-$1" --network host "$IMAGE" mongos --configdb cfg/$H:28020 --port "$1" \
      --bind_ip $H --setParameter enableTestCommands=1 >/dev/null
    wait_port "$1"
    return
  fi
  "$BIN/mongos" --configdb cfg/$H:28020 --port "$1" --bind_ip $H --fork --logpath "$DATA/mongos-$1.log" --setParameter enableTestCommands=1 >/dev/null
}
mongod_stop() {
  if [ "$DOCKER" = 1 ]; then docker stop "mdrop-$1" >/dev/null 2>&1 || true; return; fi
  [ -d "$DATA/$1" ] && "$BIN/mongod" --dbpath "$DATA/$1" --shutdown >/dev/null 2>&1 || true
}
mongos_stop() {
  if [ "$DOCKER" = 1 ]; then docker stop "mdrop-mongos-$1" >/dev/null 2>&1 || true; return; fi
  pkill -f "mongos --configdb cfg/$H:28020 --port $1" || true
}
initiate() { # port json
  $PHPI -r '$m = new MongoDB\Driver\Manager("mongodb://'$H:$1'/?directConnection=true");
    try { $m->executeCommand("admin", new MongoDB\Driver\Command(["replSetInitiate" => json_decode($argv[1])])); } catch (Exception $e) { if (strpos($e->getMessage(), "already") === false) throw $e; }
    for ($i = 0; $i < 120; $i++) { $r = $m->executeCommand("admin", new MongoDB\Driver\Command(["hello" => 1]))->toArray()[0]; if (!empty($r->isWritablePrimary) || !empty($r->primary)) exit(0); usleep(250000); }
    fwrite(STDERR, "no primary\n"); exit(1);' "$2"
}
members() { # set port...
  local set=$1; shift; local i=0 s=""
  for p in "$@"; do s="$s{\"_id\":$i,\"host\":\"$H:$p\"},"; i=$((i+1)); done
  echo "{\"_id\":\"$set\",\"members\":[${s%,}]}"
}

start() {
  case $1 in
    standalone)
      mongod_start standalone 28010
      # mongod's own /tmp/mongodb-28010.sock stays inside the Docker VM: relay one for the
      # UNIX-domain-socket tests
      if [ "$DOCKER" = 1 ]; then
        rm -f /tmp/mongodb-28010.sock
        nohup $PHPI "$(dirname "$0")/unix-relay.php" /tmp/mongodb-28010.sock 28010 >/dev/null 2>&1 &
        echo $! > "$RELAY_PID"
        for _ in $(seq 1 40); do [ -S /tmp/mongodb-28010.sock ] && break; sleep 0.25; done
      fi ;;
    replset) for p in 28011 28012 28013; do mongod_start rs0-$p $p --replSet rs0; done; initiate 28011 "$(members rs0 28011 28012 28013)" ;;
    sharded)
      mongod_start cfg 28020 --replSet cfg --configsvr; initiate 28020 '{"_id":"cfg","configsvr":true,"members":[{"_id":0,"host":"'$H':28020"}]}'
      mongod_start sh1 28021 --replSet sh1 --shardsvr; initiate 28021 "$(members sh1 28021)"
      mongod_start sh2 28022 --replSet sh2 --shardsvr; initiate 28022 "$(members sh2 28022)"
      for p in 28030 28031; do mongos_start $p; done
      $PHPI -r '$m = new MongoDB\Driver\Manager("mongodb://'$H':28030");
        foreach (["sh1/'$H':28021", "sh2/'$H':28022"] as $s) { try { $m->executeCommand("admin", new MongoDB\Driver\Command(["addShard" => $s])); } catch (Exception $e) { if (strpos($e->getMessage(), "already") === false) throw $e; } }' ;;
    auth)
      mongod_start auth 28015 --auth
      $PHPI -r '$m = new MongoDB\Driver\Manager("mongodb://'$H':28015/?directConnection=true");
        $u = [["root", "toor", ["SCRAM-SHA-1", "SCRAM-SHA-256"]], ["user", "pencil", ["SCRAM-SHA-1", "SCRAM-SHA-256"]],
          ["sha1", "sha1", ["SCRAM-SHA-1"]], ["sha256", "sha256", ["SCRAM-SHA-256"]], ["both", "both", ["SCRAM-SHA-1", "SCRAM-SHA-256"]],
          ["IX", "IX", ["SCRAM-SHA-256"]], ["\u{2168}", "\u{2163}", ["SCRAM-SHA-256"]]];
        foreach ($u as $i => [$n, $p, $mech]) {
          // the first user goes through the localhost exception, then root authenticates
          if ($i === 1) { $m = new MongoDB\Driver\Manager("mongodb://root:toor@'$H':28015/?directConnection=true"); }
          try { $m->executeCommand("admin", new MongoDB\Driver\Command(["createUser" => $n, "pwd" => $p, "roles" => ["root"], "mechanisms" => $mech])); }
          catch (Exception $e) {
            // a restarted mongod keeps its users: the localhost exception is closed then
            if ($i === 0 && $e instanceof MongoDB\Driver\Exception\CommandException && $e->getCode() === 13) { continue; }
            if (strpos($e->getMessage(), "already exists") === false) throw $e;
          }
        }' ;;
    tls)
      : "${SSL_DIR:?set SSL_DIR (tools/tls-certs.sh output)}"
      if [ "$DOCKER" = 1 ]; then C=/certs; DOCKER_EXTRA="-v $SSL_DIR:/certs:ro"; else C=$SSL_DIR; fi
      DOCKER_EXTRA=${DOCKER_EXTRA:-} mongod_start tls 28016 --tlsMode requireTLS --tlsCertificateKeyFile $C/server.pem \
        --tlsCAFile $C/ca.pem --tlsAllowConnectionsWithoutCertificates
      # 28017: a certificate for 127.0.0.2 only (host name mismatch when reached as 127.0.0.1)
      DOCKER_EXTRA=${DOCKER_EXTRA:-} mongod_start tls-ip 28017 --tlsMode requireTLS --tlsCertificateKeyFile $C/server-ip.pem \
        --tlsCAFile $C/ca.pem --tlsAllowConnectionsWithoutCertificates ;;
    x509)
      # MONGODB-X509 and PLAIN: TLS (client certificates of ca.pem accepted, not required) with --auth;
      # users root/toor (admin, through the localhost exception) and the client.pem subject in $external
      : "${SSL_DIR:?set SSL_DIR (tools/tls-certs.sh output)}"
      if [ "$DOCKER" = 1 ]; then C=/certs; DOCKER_EXTRA="-v $SSL_DIR:/certs:ro"; else C=$SSL_DIR; fi
      DOCKER_EXTRA=${DOCKER_EXTRA:-} mongod_start x509 28014 --auth --tlsMode requireTLS --tlsCertificateKeyFile $C/server.pem \
        --tlsCAFile $C/ca.pem --tlsAllowConnectionsWithoutCertificates \
        --setParameter authenticationMechanisms=MONGODB-X509,PLAIN,SCRAM-SHA-1,SCRAM-SHA-256
      $PHPI -r '$o = "tls=true&tlsCAFile='$SSL_DIR'/ca.pem&directConnection=true";
        $m = new MongoDB\Driver\Manager("mongodb://'$H':28014/?" . $o);
        try { $m->executeCommand("admin", new MongoDB\Driver\Command(["createUser" => "root", "pwd" => "toor", "roles" => ["root"]])); }
        catch (MongoDB\Driver\Exception\CommandException $e) { if ($e->getCode() !== 13 && strpos($e->getMessage(), "already exists") === false) throw $e; }
        $m = new MongoDB\Driver\Manager("mongodb://root:toor@'$H':28014/?" . $o);
        try { $m->executeCommand("\$external", new MongoDB\Driver\Command(["createUser" => "CN=client,OU=drivers,O=mdrop-client", "roles" => [["role" => "root", "db" => "admin"]]])); }
        catch (MongoDB\Driver\Exception\CommandException $e) { if (strpos($e->getMessage(), "already exists") === false) throw $e; }' ;;
    compress)
      mongod_start compress 28018 --networkMessageCompressors snappy,zstd,zlib
      mongod_start compress-off 28019 --networkMessageCompressors disabled ;;
    all) start standalone; start replset; start sharded ;;
  esac
}
stop() {
  case $1 in
    standalone) mongod_stop standalone; [ -f "$RELAY_PID" ] && kill "$(cat "$RELAY_PID")" 2>/dev/null; rm -f "$RELAY_PID" ;;
    auth) mongod_stop auth ;;
    tls) mongod_stop tls; mongod_stop tls-ip ;;
    x509) mongod_stop x509 ;;
    compress) mongod_stop compress; mongod_stop compress-off ;;
    replset) for p in 28011 28012 28013; do mongod_stop rs0-$p; done ;;
    sharded)
      for p in 28030 28031; do mongos_stop $p; done
      mongod_stop sh1; mongod_stop sh2; mongod_stop cfg ;;
    all) stop sharded; stop replset; stop standalone ;;
  esac
}
"$1" "$2"
