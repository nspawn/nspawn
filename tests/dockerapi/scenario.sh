#!/bin/sh
# Runs inside the oracle (docker in docker, see oracle.sh): makes the containers the
# golden answers describe, records the answers of the API into $OUT/golden and into
# $OUT/requests what each tool asks of it (one request line per call, through
# logproxy.py) and the request of each lifecycle case (NAME.request).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/out}
S=/var/run/docker.sock
G=$OUT/golden
R=$OUT/requests
P=/tmp/proxies
rm -rf "$G" "$R" "$P"
mkdir -p "$G" "$R" "$P"
apk add --no-cache curl python3 >/dev/null

for i in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
docker rm -f $(docker ps -aq) >/dev/null 2>&1 || true
docker network rm front >/dev/null 2>&1 || true
docker volume rm data >/dev/null 2>&1 || true
docker volume rm kept loose >/dev/null 2>&1 || true
docker rmi labelled:1 >/dev/null 2>&1 || true

# save NAME [CURL ARGS...] PATH: headers, body and status of one call.
save() {
    name=$1
    shift
    curl -sS --unix-socket $S -D "$G/$name.headers" -o "$G/$name.body" -w '%{http_code}\n' "$@" > "$G/$name.status" || true
    # curl writes no file for an answer that cannot have a body (304).
    [ -e "$G/$name.body" ] || : > "$G/$name.body"
}

# call NAME METHOD PATH [CONTENT_TYPE BODY]: save of a lifecycle case, which also keeps
# the request in $R/NAME.request: its request line and, when it has a body, the
# Content-Type header, an empty line and the body as sent.
call() {
    printf '%s %s\n' "$2" "$3" > "$R/$1.request"
    if [ $# -ge 5 ]; then
        printf 'Content-Type: %s\n\n%s' "$4" "$5" >> "$R/$1.request"
        save "$1" -X "$2" -H "Content-Type: $4" --data-binary "$5" "http://d$3"
    else
        save "$1" -X "$2" "http://d$3"
    fi
}

busybox=busybox:1.37
docker pull -q $busybox >/dev/null
# since counts whole seconds: the pull's event stays out of the events case, the
# network's and the volume's are in.
sleep 1
t0=$(date +%s)
docker network create --label e2e=yes front >/dev/null
docker volume create data >/dev/null
docker run -d --name web --network front --network-alias www -p 8080:80 -e GREETING=hello \
    -l caddy=http://web.test -l 'caddy.reverse_proxy={{upstreams 80}}' -l traefik.enable=true \
    -l 'traefik.http.routers.web.rule=Host(`web.test`)' -e VIRTUAL_HOST=web.test \
    -v data:/data --health-cmd true --health-interval 2s --restart unless-stopped \
    $busybox httpd -f -p 80 >/dev/null
docker run -d --name side --network container:web $busybox sleep 3600 >/dev/null
docker run --name done $busybox sh -c 'echo bye; echo oops >&2; exit 3' >/dev/null 2>&1 || true
docker create --name fresh $busybox true >/dev/null
sleep 6
t1=$(date +%s)

web=$(docker inspect -f '{{.Id}}' web)
front=$(docker network inspect -f '{{.Id}}' front)
save ping http://d/_ping
save ping-head -I http://d/_ping
save version http://d/version
save version-v124 http://d/v1.24/version
save version-old http://d/v1.12/version
save version-new http://d/v1.99/version
save info http://d/info
save containers http://d/containers/json
save containers-all 'http://d/containers/json?all=1'
save containers-label 'http://d/containers/json?filters=%7B%22label%22%3A%5B%22caddy%22%5D%7D'
save containers-label-legacy 'http://d/containers/json?filters=%7B%22label%22%3A%7B%22caddy%22%3Atrue%7D%7D'
save containers-exited 'http://d/containers/json?all=1&filters=%7B%22status%22%3A%5B%22exited%22%5D%7D'
save containers-badfilter 'http://d/containers/json?filters=%7B%22bogus%22%3A%5B%22x%22%5D%7D'
save containers-badstatus 'http://d/containers/json?filters=%7B%22status%22%3A%5B%22sleeping%22%5D%7D'
save inspect-web http://d/containers/web/json
save inspect-web-id "http://d/containers/$web/json"
save inspect-web-short "http://d/containers/$(echo $web | cut -c1-12)/json"
save inspect-side http://d/containers/side/json
save inspect-done http://d/containers/done/json
save inspect-fresh http://d/containers/fresh/json
save inspect-missing http://d/containers/nope/json
save top-web http://d/containers/web/top
# web writes to its disk, so that its stats have block I/O whatever the page cache holds.
docker exec web sh -c 'dd if=/dev/zero of=/tmp/io bs=4k count=256 conv=fsync' >/dev/null 2>&1
save stats-web 'http://d/containers/web/stats?stream=false'
save stats-web-oneshot 'http://d/containers/web/stats?stream=false&one-shot=true'
save logs-done 'http://d/containers/done/logs?stdout=1&stderr=1&timestamps=1'
save logs-done-tail 'http://d/containers/done/logs?stdout=1&stderr=1&tail=1'
save networks http://d/networks
save network-front http://d/networks/front
save network-front-id "http://d/networks/$front"
save network-bridge http://d/networks/bridge
save network-host http://d/networks/host
save network-missing http://d/networks/nope
save events "http://d/events?since=$t0&until=$t1"
save events-filtered "http://d/events?since=$t0&until=$t1&filters=%7B%22type%22%3A%5B%22container%22%5D%2C%22event%22%3A%5B%22die%22%5D%7D"
save kill-done -X POST http://d/containers/done/kill
save kill-web-hup -X POST 'http://d/containers/side/kill?signal=SIGHUP'
save unknown http://d/nothing/here

# tool NAME: starts the logging proxy whose socket the tool mounts as docker's.
proxy() {
    python3 "$here/logproxy.py" "$P/$1.sock" $S "$R/$1.log" &
    for i in $(seq 1 50); do [ -S "$P/$1.sock" ] && break; sleep 0.2; done
    sleep 1
}
backend() {
    docker run -d --name "$1" --network front -l caddy="http://$1.test" -l 'caddy.reverse_proxy={{upstreams 80}}' \
        -l traefik.enable=true -e VIRTUAL_HOST="$1.test" $busybox httpd -f -p 80 >/dev/null
}

proxy caddy
docker run -d --name caddy --network front -v $P/caddy.sock:/var/run/docker.sock lucaslorentz/caddy-docker-proxy:2.13-alpine >/dev/null
sleep 15
backend web2
sleep 15
docker rm -f caddy web2 >/dev/null

proxy traefik
docker run -d --name traefik --network front -v $P/traefik.sock:/var/run/docker.sock traefik:v3.5 \
    --providers.docker=true --providers.docker.exposedbydefault=false >/dev/null
sleep 15
backend web3
sleep 10
docker rm -f traefik web3 >/dev/null

proxy nginx-proxy
docker run -d --name nginx-proxy --network front -l com.github.nginx-proxy.nginx=true \
    -v $P/nginx-proxy.sock:/tmp/docker.sock:ro nginxproxy/nginx-proxy:latest >/dev/null
sleep 10
proxy acme
docker run -d --name acme --network front --volumes-from nginx-proxy \
    -v $P/acme.sock:/var/run/docker.sock:ro -e DEFAULT_EMAIL=e2e@example.test \
    -e ACME_CA_URI=https://127.0.0.1:1/directory nginxproxy/acme-companion:latest >/dev/null
sleep 10
backend web4
sleep 20
docker rm -f nginx-proxy acme web4 >/dev/null

proxy homepage
mkdir -p /tmp/homepage
printf 'local:\n  socket: /var/run/docker.sock\n' > /tmp/homepage/docker.yaml
printf -- '- Group:\n    - Web:\n        href: http://web\n        server: local\n        container: web\n' > /tmp/homepage/services.yaml
: > /tmp/homepage/settings.yaml
docker run -d --name homepage -p 3000:3000 -e HOMEPAGE_ALLOWED_HOSTS='*' -v /tmp/homepage:/app/config \
    -v $P/homepage.sock:/var/run/docker.sock ghcr.io/gethomepage/homepage:latest >/dev/null
for i in $(seq 1 60); do curl -fs -o /dev/null http://127.0.0.1:3000/ && break; sleep 1; done
curl -s -o /dev/null http://127.0.0.1:3000/api/docker/status/web/local || true
curl -s -o /dev/null http://127.0.0.1:3000/api/docker/stats/web/local || true
docker rm -f homepage >/dev/null

proxy dozzle
docker run -d --name dozzle -p 8081:8080 -v $P/dozzle.sock:/var/run/docker.sock amir20/dozzle:latest >/dev/null
for i in $(seq 1 30); do curl -fs -o /dev/null http://127.0.0.1:8081/healthcheck && break; sleep 1; done
curl -s -N -m 6 http://127.0.0.1:8081/api/events/stream > /tmp/dozzle.events || true
host=$(grep -o '"host":"[^"]*"' /tmp/dozzle.events | head -1 | cut -d'"' -f4)
short=$(echo $web | cut -c1-12)
curl -s -N -m 4 -o /dev/null "http://127.0.0.1:8081/api/hosts/$host/containers/$short/logs/stream?stdout=1&stderr=1" || true
docker rm -f dozzle >/dev/null

kill $(jobs -p) 2>/dev/null || true
docker images --digests --format '{{.Repository}}:{{.Tag}} {{.Digest}}' > "$OUT/images.txt"
docker version --format '{{.Server.Version}} {{.Server.APIVersion}}' > "$OUT/docker-version.txt"

# Images, volumes and disk usage, read on busybox alone with one container made from it,
# so that the answers do not follow the tools' tags: their containers, volumes and
# images go first. The container keeps one volume with labels; the other is dangling.
docker rm -f $(docker ps -aq) >/dev/null
docker volume prune -af >/dev/null
save volumes-empty http://d/volumes
docker volume create --label e2e=yes --label tier=data kept >/dev/null
docker volume create loose >/dev/null
docker create --name keep -v kept:/kept $busybox true >/dev/null
docker image prune -af >/dev/null
docker commit -c 'LABEL e2e=yes' keep labelled:1 >/dev/null
image=$(docker image inspect -f '{{.Id}}' $busybox)
save images http://d/images/json
save images-all 'http://d/images/json?all=1'
save images-reference 'http://d/images/json?filters=%7B%22reference%22%3A%5B%22busybox%22%5D%7D'
save images-dangling 'http://d/images/json?filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D'
save images-label 'http://d/images/json?filters=%7B%22label%22%3A%5B%22e2e%22%5D%7D'
save image-busybox http://d/images/$busybox/json
save image-busybox-id "http://d/images/$image/json"
save image-busybox-short "http://d/images/$(echo $image | cut -c8-19)/json"
save image-missing http://d/images/nope/json
save history-busybox http://d/images/$busybox/history
save volumes http://d/volumes
save volumes-dangling 'http://d/volumes?filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D'
save volume-kept http://d/volumes/kept
save volume-missing http://d/volumes/nope
save system-df http://d/system/df
# What a daemon outside a swarm answers of the swarm and plugin reads.
for what in plugins swarm services secrets configs nodes tasks; do
    save $what http://d/$what
done

# The lifecycle of containers through the API's writes. keep goes first, so that the
# containers listed, waited for and pruned are these alone: life runs sleep as PID 1,
# which ignores SIGTERM, so that a stop takes its timeout and ends in SIGKILL; created
# is made as `docker create -it --rm` makes it and never starts; exited ends with 3
# while a wait for it blocks. rm removes life, the prune the other two.
docker rm -f keep >/dev/null
life='{"Image":"busybox:1.37","Cmd":["sleep","3600"],"Labels":{"e2e":"lifecycle"}}'
call create POST '/containers/create?name=life' application/json "$life"
# The engine reads the body, then checks the settings, the image and the name in that
# order: each of these fails at one step alone.
call create-badname POST '/containers/create?name=-life' application/json "$life"
call create-conflict POST '/containers/create?name=life' application/json "$life"
call create-noimage POST '/containers/create?name=never' application/json '{"Image":"docker.io/library/nope"}'
call create-badjson POST '/containers/create?name=never' application/json '{"Image":"busybox:1.37"'
call create-contenttype POST '/containers/create?name=never' text/plain "$life"
call create-autoremove-restart POST '/containers/create?name=never' application/json \
    '{"Image":"busybox:1.37","HostConfig":{"AutoRemove":true,"RestartPolicy":{"Name":"always"}}}'
call start POST /containers/life/start
call start-again POST /containers/life/start
docker create --name created -it --rm -l e2e=lifecycle $busybox sh >/dev/null
call inspect-created GET /containers/created/json
call containers-created GET '/containers/json?all=1&filters=%7B%22status%22%3A%5B%22created%22%5D%7D'
call wait-created POST /containers/created/wait
call update-autoremove POST /containers/created/update application/json '{"RestartPolicy":{"Name":"always"}}'
call pause POST /containers/life/pause
call pause-again POST /containers/life/pause
call unpause POST /containers/life/unpause
call unpause-again POST /containers/life/unpause
call stop POST '/containers/life/stop?t=1'
call stop-again POST '/containers/life/stop?t=1'
call pause-stopped POST /containers/life/pause
call restart POST '/containers/life/restart?t=1'
call update POST /containers/life/update application/json \
    '{"Memory":67108864,"MemorySwap":134217728,"NanoCpus":500000000,"PidsLimit":-1,"RestartPolicy":{"Name":"on-failure","MaximumRetryCount":3}}'
call rm-running DELETE /containers/life
call rm DELETE '/containers/life?force=1'
call rm-missing DELETE /containers/life
docker run -d --name exited -l e2e=lifecycle $busybox sh -c 'head -c 4096 /dev/zero > /written; sleep 2; exit 3' >/dev/null
call wait-exited POST /containers/exited/wait
call wait-badcondition POST '/containers/exited/wait?condition=bogus'
call wait-missing POST /containers/nope/wait
call prune POST '/containers/prune?filters=%7B%22label%22%3A%5B%22e2e%3Dlifecycle%22%5D%7D'
echo "recorded into $OUT"
