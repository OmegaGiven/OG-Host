# shared helpers for the in-host benchmark scripts (sourced)
t() { date +%s.%N; }
since() { awk -v a="$1" -v b="$(date +%s.%N)" 'BEGIN{printf "%.2f", b-a}'; }
out() { echo "RESULT $1 $2 ${3:-}"; }
# memory in use, not counting page cache and reclaimable slab (like free's "used")
memused() { awk '/^MemTotal/{t=$2}/^MemFree/{f=$2}/^Buffers/{b=$2}/^Cached/{c=$2}/^SReclaimable/{r=$2}END{printf "%d", (t-f-b-c-r)/1024}' /proc/meminfo; }
# the same after dropping clean caches (steadier for before/after deltas)
memsettled() { sync; echo 3 > /proc/sys/vm/drop_caches; sleep 3; memused; }
# wait until a command succeeds (max $1 s); returns 1 on timeout
waitfor() { local max=$1; shift; local s; s=$(date +%s); until "$@" >/dev/null 2>&1; do sleep 0.1; [ $(( $(date +%s) - s )) -ge "$max" ] && return 1; done; return 0; }
http_up() { curl -s -o /dev/null --max-time 1 "http://$1/"; }
ssh_up() { timeout 2 bash -c "head -c3 </dev/tcp/$1/22" 2>/dev/null | grep -q SSH; }
median() { sort -n | awk '{a[NR]=$1}END{if(NR%2)print a[(NR+1)/2]; else printf "%.2f\n",(a[NR/2]+a[NR/2+1])/2}'; }
idle_cpu() { # % CPU busy over $1 seconds
  read -r _ a b c d e f g h _ < /proc/stat; local t1=$((a+b+c+d+e+f+g+h)) i1=$((d+e))
  sleep "$1"
  read -r _ a b c d e f g h _ < /proc/stat; local t2=$((a+b+c+d+e+f+g+h)) i2=$((d+e))
  awk -v t=$((t2-t1)) -v i=$((i2-i1)) 'BEGIN{printf "%.2f", 100*(t-i)/t}'
}
footprint() {
  out disk_used_mb "$(df -BM --output=used / | tail -1 | tr -dc 0-9)" MB
  out packages "$(dpkg-query -f '.\n' -W | wc -l)"
  out mem_used_idle_mb "$(memused)" MB
  out cpu_busy_idle_pct "$(idle_cpu 60)" %
  out tcp_ports_external "$(ss -Htln | awk '{print $4}' | grep -vE '^(127\.|\[::1\])' | sort -u | wc -l)"
  out root_processes "$(ps -eo user=,ppid=,pid= | awk '$1=="root" && $2!=2 && $3!=2' | wc -l)"
  out processes "$(ps -eo ppid=,pid= | awk '$1!=2 && $2!=2' | wc -l)"
}
