#!/bin/bash
# Test topologies for the PHPT/PHPLIB suites: topo.sh start|stop standalone|replset|sharded|all
#   standalone  mongodb://127.0.0.1:28010
#   replset     mongodb://127.0.0.1:28011,127.0.0.1:28012,127.0.0.1:28013/?replicaSet=rs0
#   sharded     mongodb://127.0.0.1:28030,127.0.0.1:28031 (2 mongos; config rs cfg 28020, shards sh1 28021, sh2 28022)
# Env: MONGO_BIN (dir with mongod/mongos), TOPO_DATA (data dir), TOPO_PHP (a PHP with a mongodb extension, to initiate).
set -e
BIN=${MONGO_BIN:-/zf/async/mongo/bin}
DATA=${TOPO_DATA:-/zf/mdrop/data/topo}
PHPI=${TOPO_PHP:-php8.4 -n -d extension=mongodb}
H=127.0.0.1

mongod_start() { # name port extra...
  local name=$1 port=$2; shift 2
  mkdir -p "$DATA/$name"
  "$BIN/mongod" --dbpath "$DATA/$name" --port "$port" --bind_ip $H --fork --logpath "$DATA/$name.log" \
    --wiredTigerCacheSizeGB 0.5 --setParameter enableTestCommands=1 "$@" >/dev/null
}
mongod_stop() { [ -d "$DATA/$1" ] && "$BIN/mongod" --dbpath "$DATA/$1" --shutdown >/dev/null 2>&1 || true; }
initiate() { # port json
  $PHPI -r '$m = new MongoDB\Driver\Manager("mongodb://'$H:$1'/?directConnection=true");
    try { $m->executeCommand("admin", new MongoDB\Driver\Command(["replSetInitiate" => json_decode($argv[1])])); } catch (Exception $e) { if (strpos($e->getMessage(), "already") === false) throw $e; }
    for ($i = 0; $i < 120; $i++) { $r = $m->executeCommand("admin", new MongoDB\Driver\Command(["hello" => 1]))->toArray()[0]; if (!empty($r->isWritablePrimary)) exit(0); usleep(250000); }
    fwrite(STDERR, "no primary\n"); exit(1);' "$2"
}
members() { # set port...
  local set=$1; shift; local i=0 s=""
  for p in "$@"; do s="$s{\"_id\":$i,\"host\":\"$H:$p\"},"; i=$((i+1)); done
  echo "{\"_id\":\"$set\",\"members\":[${s%,}]}"
}

start() {
  case $1 in
    standalone) mongod_start standalone 28010 ;;
    replset) for p in 28011 28012 28013; do mongod_start rs0-$p $p --replSet rs0; done; initiate 28011 "$(members rs0 28011 28012 28013)" ;;
    sharded)
      mongod_start cfg 28020 --replSet cfg --configsvr; initiate 28020 '{"_id":"cfg","configsvr":true,"members":[{"_id":0,"host":"'$H':28020"}]}'
      mongod_start sh1 28021 --replSet sh1 --shardsvr; initiate 28021 "$(members sh1 28021)"
      mongod_start sh2 28022 --replSet sh2 --shardsvr; initiate 28022 "$(members sh2 28022)"
      for p in 28030 28031; do "$BIN/mongos" --configdb cfg/$H:28020 --port $p --bind_ip $H --fork --logpath "$DATA/mongos-$p.log" --setParameter enableTestCommands=1 >/dev/null; done
      $PHPI -r '$m = new MongoDB\Driver\Manager("mongodb://'$H':28030");
        foreach (["sh1/'$H':28021", "sh2/'$H':28022"] as $s) { try { $m->executeCommand("admin", new MongoDB\Driver\Command(["addShard" => $s])); } catch (Exception $e) { if (strpos($e->getMessage(), "already") === false) throw $e; } }' ;;
    all) start standalone; start replset; start sharded ;;
  esac
}
stop() {
  case $1 in
    standalone) mongod_stop standalone ;;
    replset) for p in 28011 28012 28013; do mongod_stop rs0-$p; done ;;
    sharded)
      for p in 28030 28031; do pkill -f "mongos --configdb cfg/$H:28020 --port $p" || true; done
      mongod_stop sh1; mongod_stop sh2; mongod_stop cfg ;;
    all) stop sharded; stop replset; stop standalone ;;
  esac
}
"$1" "$2"
