#!/bin/sh
# Runs inside a Debian LXC container with Docker (Proxmox + Docker in LXC, the common setup):
# the same container tests as bench.sh. Prints RESULT lines.
t() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(date +%s.%N)" 'BEGIN{printf "%.2f", b-a}'; }
up() { curl -s -o /dev/null --max-time 1 "http://$1/"; }
ipof() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"; }
waitup() { i=0; until up "$1"; do sleep 0.1; i=$((i+1)); [ $i -gt 600 ] && return 1; done; }
RUNS=${RUNS:-3}; N=${DENSITY:-30}
docker pull -q nginx:alpine >/dev/null
v=""
for r in $(seq $RUNS); do s=$(t); docker run -d -q --name c$r -m 256m nginx:alpine >/dev/null; waitup "$(ipof c$r)"; v="$v $(since $s)"; docker rm -f c$r >/dev/null; done
echo "RESULT container_create_to_http_s $(echo $v | tr ' ' '\n' | sort -n | awk '{a[NR]=$1}END{print a[int((NR+1)/2)]}') s"
s=$(t); for i in $(seq $N); do docker run -d -q --name d$i -m 256m nginx:alpine >/dev/null; waitup "$(ipof d$i)"; done
echo "RESULT density_${N}_containers_s $(since $s) s"
s=$(t); docker rm -f $(seq -f 'd%g' 1 $N) >/dev/null; echo "RESULT density_remove_all_s $(since $s) s"
echo "DIAG docker storage $(docker info -f '{{.Driver}}')"
