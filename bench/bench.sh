#!/bin/bash
# bench.sh [SECTIONS...] — run on a Proxmox VE or OG-Host server as root; prints "RESULT name value unit".
# Needs: /root/key (SSH key the guests trust), template "tpl"/9000 (Debian 13 cloud image, user bench),
# the nginx:alpine image pulled. Sections: footprint containers density vms vmdisk vmnet backup
set -u
. "$(dirname "$0")/lib.sh"
if command -v pveversion >/dev/null; then P=pve; NODE=$(hostname); else P=og; fi
GSSH="ssh -q -i /root/key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=3"
RUNS=${RUNS:-3}

# ---- platform primitives ----
pve_wait() { local u; u=$(grep -o 'UPID:[^ ]*' | tail -1); while pvesh get "/nodes/$NODE/tasks/$u/status" --output-format json | grep -q '"status":"running"'; do sleep 0.2; done; }
ctr_create_start() { # N -> prints IP when serving HTTP
  if [ $P = pve ]; then
    pct create $((300+$1)) local:vztmpl/nginx_alpine.tar --hostname c$1 --rootfs local-lvm:1 --memory 256 --swap 0 \
      --net0 name=eth0,bridge=vmbr0,ip=10.0.2.$((100+$1))/24,gw=10.0.2.2 --unprivileged 1 >/dev/null 2>&1
    pct start $((300+$1)); echo 10.0.2.$((100+$1))
  else
    og-hostd run --name c$1 --memory 256m nginx:alpine | sed -n 's/.* ip \([0-9.]*\).*/\1/p'
  fi
}
ctr_rm() { if [ $P = pve ]; then pct stop $((300+$1)) >/dev/null 2>&1; pct destroy $((300+$1)) --purge >/dev/null 2>&1; else og-hostd rm -f c$1 >/dev/null 2>&1; fi; }
vm_clone() { # N linked(1/0)
  if [ $P = pve ]; then
    local full=1; [ "$2" = 1 ] && full=0
    qm clone 9000 $((100+$1)) --name v$1 --full $full >/dev/null 2>&1
    qm set $((100+$1)) --ipconfig0 ip=10.0.2.$((50+$1))/24,gw=10.0.2.2 --nameserver 10.0.2.3 >/dev/null
  else
    local tk; tk=$(ogapi POST /v1/vms/tpl/clone "{\"name\":\"v$1\",\"linked\":$([ "$2" = 1 ] && echo true || echo false)}" | sed -n 's/.*"task":"\([^"]*\)".*/\1/p')
    ogtask "$tk" >/dev/null
  fi
}
vm_start() { if [ $P = pve ]; then qm start $((100+$1)); else ogapi POST /v1/vms/v$1/start >/dev/null; fi; }
vm_ip() { if [ $P = pve ]; then echo 10.0.2.$((50+$1)); else ogapi GET /v1/vms/v$1 | grep -o '"ip":"[0-9.]*"' | head -1 | cut -d'"' -f4; fi; }
vm_rm() { if [ $P = pve ]; then qm stop $((100+$1)) >/dev/null 2>&1; qm destroy $((100+$1)) --purge >/dev/null 2>&1; else ogapi POST /v1/vms/v$1/stop >/dev/null; sleep 1; ogapi DELETE /v1/vms/v$1 >/dev/null; fi; }
vm_ready() { local ip; ip=$(vm_ip $1); [ -n "$ip" ] && $GSSH bench@"$ip" true; }

cleanup() { # leftovers of an interrupted run
  if [ $P = pve ]; then
    for id in $(qm list | awk '$1>100 && $1<200{print $1}'); do qm stop $id >/dev/null 2>&1; qm destroy $id --purge >/dev/null 2>&1; done
    for id in $(pct list | awk '$1>=300 && $1<400{print $1}'); do pct stop $id >/dev/null 2>&1; pct destroy $id --purge >/dev/null 2>&1; done
  else
    for v in $(ogapi GET /v1/vms | grep -o '"name":"v[0-9]*"' | cut -d'"' -f4 | sort -u); do ogapi POST /v1/vms/$v/stop >/dev/null; sleep 1; ogapi DELETE /v1/vms/$v >/dev/null; done
    for c in $(og-hostd ps | awk 'NR>1 && $2 ~ /^(c[0-9]+|i[0-9]+|hog|cb-web-1)$/ {print $2}'); do og-hostd stop $c >/dev/null 2>&1; og-hostd rm $c >/dev/null 2>&1; done
    rm -rf /var/lib/og-host/bench-store # an earlier run's backups (they'd count as disk used, and dedup the next backup)
  fi
}

# what the host does for its VMs, for comparing the two (DIAG lines; not results)
diag_host() {
  echo "DIAG kernel $(uname -r)"
  echo "DIAG thp $(cat /sys/kernel/mm/transparent_hugepage/enabled) defrag $(cat /sys/kernel/mm/transparent_hugepage/defrag)"
  echo "DIAG ksm run=$(cat /sys/kernel/mm/ksm/run 2>/dev/null) pages_sharing=$(cat /sys/kernel/mm/ksm/pages_sharing 2>/dev/null)"
  for p in $(pgrep -f '^(/usr/bin/)?(qemu-system-x86_64|kvm) '); do
    echo "DIAG qemu $p $(awk '/^(Rss|Pss_Anon|Pss_File|AnonHugePages|Swap):/{printf "%s %d MB  ", $1, $2/1024}' /proc/$p/smaps_rollup)"
    echo "DIAG qemu $p cmdline $(tr '\0' ' ' < /proc/$p/cmdline | grep -oE -- '-(machine|cpu|m|netdev|device|vga|bios|drive) [^ ]+' | grep -vE 'pflash|smbios' | tr '\n' ' ')"
  done
}
# a two-node cluster with the other node off has no quorum (/etc/pve read-only): one vote is enough here
if [ $P = pve ] && [ -f /etc/pve/corosync.conf ]; then pvecm status 2>/dev/null | grep -q "Quorate:.*Yes" || pvecm expected 1; for i in $(seq 30); do pvecm status 2>/dev/null | grep -q "Quorate:.*Yes" && break; sleep 1; done; fi
[ $P = pve ] && systemctl stop proxmox-backup-proxy proxmox-backup 2>/dev/null
[ "${1:-}" = footprint ] || cleanup

# ---- sections ----
s_footprint() { footprint; }

s_containers() {
  local v=()
  for i in $(seq $RUNS); do
    local s; s=$(t); ip=$(ctr_create_start $i)
    waitfor 60 http_up "$ip" || echo "WARN container $i never served"
    v+=("$(since $s)"); ctr_rm $i
  done
  out container_create_to_http_s "$(printf '%s\n' "${v[@]}" | median)" s
}

s_density() { # 30 nginx containers: time, and memory per container
  local n=${DENSITY:-30} m0 s; m0=$(memsettled); s=$(t)
  for i in $(seq $n); do ip=$(ctr_create_start $((10+i))); waitfor 60 http_up "$ip" || echo "WARN c$((10+i))"; done
  out density_${n}_containers_s "$(since $s)" s
  sleep 20; out density_mem_per_container_mb "$(( ($(memsettled) - m0) / n ))" MB
  # per container: its cgroup (after dropping clean caches) + the process that supervises it
  sync; echo 3 > /proc/sys/vm/drop_caches; sleep 2
  local tot=0 c sup
  for i in $(seq $n); do
    if [ $P = pve ]; then
      c=$(cat /sys/fs/cgroup/lxc/$((310+i))/memory.current); sup=$(pgrep -f "^/usr/bin/lxc-start -F -n $((310+i))\$")
    else
      c=$(cat /sys/fs/cgroup/og-host/c$((10+i))/memory.current); sup=$(pgrep -f "og-shim run --id $(og-hostd ps | awk -v n=c$((10+i)) '$2==n{print $1}')")
    fi
    tot=$(( tot + c + $(awk '/^Pss:/{print $2*1024}' /proc/$sup/smaps_rollup 2>/dev/null || echo 0) ))
  done
  out ctr_mem_cgroup_plus_supervisor_mb "$(awk -v x=$tot -v n=$n 'BEGIN{printf "%.1f", x/n/1048576}')" MB
  s=$(t); for i in $(seq $n); do ctr_rm $((10+i)); done; out density_remove_all_s "$(since $s)" s
}

s_vms() {
  local lc=() fc=() bt=()
  for i in $(seq $RUNS); do
    local s; s=$(t); vm_clone $i 1; lc+=("$(since $s)")
    s=$(t); vm_start $i; waitfor 180 vm_ready $i || echo "WARN v$i never ready"; bt+=("$(since $s)")
    vm_rm $i
    s=$(t); vm_clone $((10+i)) 0; fc+=("$(since $s)"); vm_rm $((10+i))
  done
  out vm_linked_clone_s "$(printf '%s\n' "${lc[@]}" | median)" s
  out vm_full_clone_8g_s "$(printf '%s\n' "${fc[@]}" | median)" s
  out vm_start_to_ssh_s "$(printf '%s\n' "${bt[@]}" | median)" s
  # memory per idle running VM (3 x 1 GB guests)
  local m0; m0=$(memsettled)
  for i in 21 22 23; do vm_clone $i 1; vm_start $i; done
  for i in 21 22 23; do waitfor 180 vm_ready $i || echo "WARN v$i"; done
  # idle VMs run for hours: measured after 3 minutes (also after 30 s, freshly booted)
  sleep 30; out vm_mem_per_fresh_1g_guest_mb "$(( ($(memsettled) - m0) / 3 ))" MB
  sleep 150; out vm_mem_per_idle_1g_guest_mb "$(( ($(memsettled) - m0) / 3 ))" MB
  diag_host
  for i in 21 22 23; do vm_rm $i; done
}

guest_tools() { # N: install fio + iperf3 in the guest
  # after cloud-init finishes (it holds the apt lock while it runs); a few tries for mirror hiccups
  $GSSH bench@"$(vm_ip $1)" 'cloud-init status --wait >/dev/null 2>&1; for i in 1 2 3; do sudo apt-get -qq update >/dev/null 2>&1; sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y fio iperf3 >/dev/null 2>&1 && break; sleep 5; done; command -v fio && command -v iperf3' >/dev/null
}

s_vmdisk() {
  vm_clone 31 1; vm_start 31; waitfor 180 vm_ready 31; guest_tools 31 || echo "WARN no fio"
  local ip; ip=$(vm_ip 31)
  # each test RUNS times on its own fresh 2 GB file, medians; terse v3: read iops 8, read KB/s 7, write iops 49, write KB/s 48
  for spec in "randread 4k 32 8" "randwrite 4k 32 49" "read 1M 8 7" "write 1M 8 48"; do
    set -- $spec
    local v=()
    for i in $(seq $RUNS); do
      r=$($GSSH bench@"$ip" "sudo rm -f /var/tmp/fio; sudo fio --name=b --filename=/var/tmp/fio --size=2G --direct=1 --ioengine=libaio --rw=$1 --bs=$2 --iodepth=$3 --runtime=20 --time_based --group_reporting --output-format=terse --terse-version=3" 2>/dev/null)
      v+=("$(echo "$r" | cut -d';' -f$4)")
    done
    local m; m=$(printf '%s\n' "${v[@]}" | median)
    case $1 in
      randread|randwrite) out vm_fio_${1}_4k_iops "$m" iops ;;
      *) out vm_fio_seq${1}_1m_mbs "$(awk -v x="$m" 'BEGIN{printf "%d", x/1024}')" MB/s ;;
    esac
    echo "DIAG fio $1 runs: ${v[*]}"
  done
  vm_rm 31
}

s_vmnet() {
  vm_clone 41 1; vm_clone 42 1; vm_start 41; vm_start 42; waitfor 180 vm_ready 41; waitfor 180 vm_ready 42
  guest_tools 41; guest_tools 42
  local a b; a=$(vm_ip 41); b=$(vm_ip 42)
  $GSSH bench@"$a" 'iperf3 -s -D'; sleep 1
  local g=()
  for i in $(seq $RUNS); do
    r=$($GSSH bench@"$b" "iperf3 -c $a -t 10 -P 2 -J" | grep -A8 '"sum_received"' | grep bits_per_second | head -1 | tr -dc '0-9.')
    g+=("$(awk -v x="$r" 'BEGIN{printf "%.2f", x/1e9}')")
  done
  out vm_to_vm_gbit "$(printf '%s\n' "${g[@]}" | median)" Gbit/s
  r=$($GSSH bench@"$b" "ping -c 50 -i 0.05 -q $a" | awk -F/ '/rtt/{print $5}')
  out vm_to_vm_ping_ms "$r" ms
  echo "DIAG guest $($GSSH bench@"$b" 'echo nproc $(nproc); ethtool -g eth0 2>/dev/null | tr -s "\t\n" " "; ethtool -l eth0 2>/dev/null | tr -s "\t\n" " "')"
  # host to guest
  if command -v iperf3 >/dev/null; then
    g=()
    for i in $(seq $RUNS); do
      r=$(iperf3 -c "$a" -t 10 -J | grep -A8 '"sum_received"' | grep bits_per_second | head -1 | tr -dc '0-9.')
      g+=("$(awk -v x="$r" 'BEGIN{printf "%.2f", x/1e9}')")
    done
    out host_to_vm_gbit "$(printf '%s\n' "${g[@]}" | median)" Gbit/s
  fi
  vm_rm 41; vm_rm 42
}

s_backup() { # back up a stopped VM with 1 GB of random data written into it, then restore it
  vm_clone 51 0; vm_start 51; waitfor 180 vm_ready 51
  $GSSH bench@"$(vm_ip 51)" 'sudo dd if=/dev/urandom of=/var/tmp/blob bs=1M count=1024 status=none; sync'
  vm_rm_keep() { if [ $P = pve ]; then qm shutdown 151 --timeout 60 >/dev/null 2>&1; else ogapi POST /v1/vms/v51/shutdown >/dev/null; waitfor 60 sh -c "ogapi GET /v1/vms/v51 | grep -q '\"state\":\"stopped\"'"; fi; }
  vm_rm_keep; sleep 2
  local s; s=$(t)
  if [ $P = pve ]; then
    vzdump 151 --storage local --mode stop --compress zstd >/tmp/vzdump.log 2>&1
    out backup_vm_s "$(since $s)" s
    f=$(ls -t /var/lib/vz/dump/vzdump-qemu-151-*.vma.zst | head -1); out backup_vm_size_mb "$(( $(stat -c %s "$f") / 1048576 ))" MB
    qm destroy 151 --purge >/dev/null 2>&1
    s=$(t); qmrestore "$f" 151 --storage local-lvm >/dev/null 2>&1; out restore_vm_s "$(since $s)" s
    # second backup of the unchanged VM (incremental with PBS only; vzdump is always full)
    s=$(t); vzdump 151 --storage local --mode stop --compress zstd >/dev/null 2>&1; out backup_vm_again_s "$(since $s)" s
    qm destroy 151 --purge >/dev/null 2>&1; rm -f /var/lib/vz/dump/vzdump-qemu-151-*
  else
    ogapi GET /v1/storage | grep -q '"id":"bench"' || ogapi POST /v1/storage '{"id":"bench","type":"dir","path":"/var/lib/og-host/bench-store"}' >/dev/null
    tk=$(ogapi POST /v1/backup '{"store":"bench","targets":[{"kind":"vm","name":"v51"}]}' | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null
    out backup_vm_s "$(since $s)" s
    out backup_vm_size_mb "$(( $(du -sb /var/lib/og-host/bench-store | cut -f1) / 1048576 ))" MB
    snap=$(ogapi GET /v1/storage/bench/backups | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
    ogapi POST /v1/vms/v51/stop >/dev/null; ogapi DELETE /v1/vms/v51 >/dev/null
    s=$(t); tk=$(ogapi POST /v1/restore "{\"store\":\"bench\",\"snapshot\":\"$snap\"}" | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null; out restore_vm_s "$(since $s)" s
    s=$(t); tk=$(ogapi POST /v1/backup '{"store":"bench","targets":[{"kind":"vm","name":"v51"}]}' | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null; out backup_vm_again_s "$(since $s)" s
    ogapi DELETE /v1/vms/v51 >/dev/null
    rm -rf /var/lib/og-host/bench-store
  fi
}

# ---- more sections ----

s_security() {
  command -v lynis >/dev/null || DEBIAN_FRONTEND=noninteractive apt-get -qq install -y lynis >/dev/null 2>&1
  out lynis_hardening_index "$(lynis audit system --quick --no-colors 2>/dev/null | sed -n 's/.*Hardening index : \([0-9]*\).*/\1/p')" /100
  out tcp_ports_list "$(ss -Htln | awk '{print $4}' | grep -vE '^(127\.|\[::1\])' | sed 's/.*://' | sort -un | tr '\n' ',')"
  out suid_files "$(find / -xdev -perm -4000 -type f 2>/dev/null | wc -l)"
  out kernel_lockdown "$(cat /sys/kernel/security/lockdown 2>/dev/null | tr -d '[]' | tr ' ' '/' )"
}

api_get() { # one "list the VMs" call over HTTPS with an API token
  if [ $P = pve ]; then curl -sk -o /dev/null -H "Authorization: PVEAPIToken=$PVETOKEN" "https://127.0.0.1:8006/api2/json/nodes/$NODE/qemu"
  else curl -sk -o /dev/null -H "Authorization: Bearer $(cat /var/lib/og-host/api-token)" https://127.0.0.1:8006/v1/vms; fi
}
s_api() {
  if [ $P = pve ]; then
    pveum user token remove root@pam bench >/dev/null 2>&1
    PVETOKEN="root@pam!bench=$(pveum user token add root@pam bench --privsep 0 --output-format json | sed -n 's/.*"value":"\([^"]*\)".*/\1/p')"
  fi
  local v=(); for i in $(seq 50); do local s; s=$(t); api_get; v+=("$(since $s)"); done
  out api_list_vms_ms "$(printf '%s\n' "${v[@]}" | median | awk '{printf "%.1f", $1*1000}')" ms
  out api_list_vms_max_ms "$(printf '%s\n' "${v[@]}" | sort -n | tail -1 | awk '{printf "%.1f", $1*1000}')" ms
}

# an iperf3 server container N; prints its IP
iperf_ctr() {
  if [ $P = pve ]; then
    [ -f /var/lib/vz/template/cache/iperf3_latest.tar ] || pvesh create /nodes/$NODE/storage/local/oci-registry-pull --reference docker.io/networkstatic/iperf3:latest | pve_wait
    pct create $((300+$1)) local:vztmpl/iperf3_latest.tar --hostname i$1 --rootfs local-lvm:1 --memory 512 --swap 0 \
      --net0 name=eth0,bridge=vmbr0,ip=10.0.2.$((100+$1))/24,gw=10.0.2.2 --unprivileged 1 >/dev/null 2>&1
    pct set $((300+$1)) --entrypoint 'iperf3 -s' >/dev/null 2>&1
    pct start $((300+$1)); echo 10.0.2.$((100+$1))
  else
    og-hostd run --name i$1 networkstatic/iperf3 -- -s | sed -n 's/.* ip \([0-9.]*\).*/\1/p'
  fi
}
iperf_rm() { if [ $P = pve ]; then pct stop $((300+$1)) >/dev/null 2>&1; pct destroy $((300+$1)) --purge >/dev/null 2>&1; else og-hostd stop i$1 >/dev/null 2>&1; og-hostd rm i$1 >/dev/null 2>&1; fi; }
ctr_iperf_client() { # N SERVER-IP: iperf3 from inside container N, Gbit/s
  if [ $P = pve ]; then pct exec $((300+$1)) -- iperf3 -c "$2" -t 8 -P 2 -J
  else
    local id; id=$(og-hostd ps | awk -v n=i$1 '$2==n{print $1}')
    local e; e=$(curl -s --unix-socket /run/og-host/docker.sock -H content-type:application/json -d "{\"AttachStdout\":true,\"Cmd\":[\"sh\",\"-c\",\"iperf3 -c $2 -t 8 -P 2 -J > /tmp/r.json\"]}" http://x/containers/$id/exec | sed -n 's/.*"Id":"\([^"]*\)".*/\1/p')
    curl -s --unix-socket /run/og-host/docker.sock -H content-type:application/json -d '{"Detach":false}' http://x/exec/$e/start >/dev/null
    curl -s --unix-socket /run/og-host/docker.sock -H content-type:application/json -d '{"AttachStdout":true,"Cmd":["cat","/tmp/r.json"]}' http://x/containers/$id/exec | sed -n 's/.*"Id":"\([^"]*\)".*/\1/p' | { read -r e2; curl -s --unix-socket /run/og-host/docker.sock -H content-type:application/json -d '{"Detach":false}' http://x/exec/$e2/start | tr -d '\000-\010'; }
  fi | grep -A8 '"sum_received"' | grep bits_per_second | head -1 | tr -dc '0-9.' | awk '{printf "%.2f", $1/1e9}'
}
s_ctrnet() {
  local a b; a=$(iperf_ctr 61); b=$(iperf_ctr 62); waitfor 30 bash -c "timeout 1 bash -c '</dev/tcp/$a/5201'" ; waitfor 30 bash -c "timeout 1 bash -c '</dev/tcp/$b/5201'"
  local g=() h=()
  for i in $(seq $RUNS); do g+=("$(ctr_iperf_client 62 "$a")"); sleep 1; done
  out ctr_to_ctr_gbit "$(printf '%s\n' "${g[@]}" | median)" Gbit/s
  for i in $(seq $RUNS); do h+=("$(iperf3 -c "$a" -t 8 -P 2 -J | grep -A8 '"sum_received"' | grep bits_per_second | head -1 | tr -dc '0-9.' | awk '{printf "%.2f", $1/1e9}')"); sleep 1; done
  out host_to_ctr_gbit "$(printf '%s\n' "${h[@]}" | median)" Gbit/s
  iperf_rm 61; iperf_rm 62
}

s_ctrbackup() { # a running web container with 500 MB of data: back up, remove, restore, serving again
  local ip s tk snap
  if [ $P = pve ]; then
    ip=$(ctr_create_start 71); waitfor 60 http_up "$ip"
    pct exec 371 -- sh -c 'head -c 524288000 /dev/urandom > /data.bin; sync'
    s=$(t); vzdump 371 --storage local --mode snapshot --compress zstd >/dev/null 2>&1; out ctr_backup_s "$(since $s)" s
    local f; f=$(ls -t /var/lib/vz/dump/vzdump-lxc-371-*.tar.zst | head -1); out ctr_backup_size_mb "$(( $(stat -c %s "$f") / 1048576 ))" MB
    s=$(t); vzdump 371 --storage local --mode snapshot --compress zstd >/dev/null 2>&1; out ctr_backup_again_s "$(since $s)" s
    ctr_rm 71; s=$(t); pct restore 371 "$f" --storage local-lvm >/dev/null 2>&1; pct start 371; waitfor 60 http_up "$ip" || echo "WARN restored ct not serving"; out ctr_restore_to_http_s "$(since $s)" s
    rm -f /var/lib/vz/dump/vzdump-lxc-371-*; ctr_rm 71
  else
    mkdir -p /root/cb && printf 'services:\n  web:\n    image: nginx:alpine\n    volumes:\n      - data:/data\nvolumes:\n  data: {}\n' > /root/cb/compose.yml
    og-hostd up -f /root/cb/compose.yml -p cb >/dev/null; ip=$(og-hostd ps | awk '$2=="cb-web-1"{print $5}'); waitfor 60 http_up "$ip"
    head -c 500M /dev/urandom > /var/lib/og-host/volumes/cb_data/data.bin; sync
    ogapi GET /v1/storage | grep -q '"id":"bench"' || ogapi POST /v1/storage '{"id":"bench","type":"dir","path":"/var/lib/og-host/bench-store"}' >/dev/null
    s=$(t); tk=$(ogapi POST /v1/backup '{"store":"bench","targets":[{"kind":"stack","name":"cb"}]}' | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null; out ctr_backup_s "$(since $s)" s
    out ctr_backup_size_mb "$(( $(du -sb /var/lib/og-host/bench-store | cut -f1) / 1048576 ))" MB
    s=$(t); tk=$(ogapi POST /v1/backup '{"store":"bench","targets":[{"kind":"stack","name":"cb"}]}' | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null; out ctr_backup_again_s "$(since $s)" s
    snap=$(ogapi GET /v1/storage/bench/backups | grep -o '"id":"stack/cb/[^"]*"' | head -1 | cut -d'"' -f4)
    og-hostd down -f /root/cb/compose.yml -p cb --volumes >/dev/null
    s=$(t); tk=$(ogapi POST /v1/restore "{\"store\":\"bench\",\"snapshot\":\"$snap\",\"start\":true}" | sed -n 's/.*"task":"\([^"]*\)".*/\1/p'); ogtask "$tk" >/dev/null
    ip=$(og-hostd ps | awk '$2=="cb-web-1"{print $5}'); waitfor 60 http_up "$ip" || echo "WARN restored ct not serving"; out ctr_restore_to_http_s "$(since $s)" s
    [ -s /var/lib/og-host/volumes/cb_data/data.bin ] || echo "WARN restored volume empty"
    og-hostd down -f /root/cb/compose.yml -p cb --volumes >/dev/null
    rm -rf /var/lib/og-host/bench-store; ogapi DELETE /v1/storage/bench >/dev/null
  fi
}

# a container that eats 10 GB (4 x 2.5 GB, more than the host has) ($2 = memory limit in MB, 0 = the platform default)
hog_start() {
  if [ $P = pve ]; then
    [ -f /var/lib/vz/template/cache/stress_latest.tar ] || pvesh create /nodes/$NODE/storage/local/oci-registry-pull --reference docker.io/polinux/stress:latest | pve_wait
    local mem=""; [ "$2" != 0 ] && mem="--memory $2"
    pct create $((300+$1)) local:vztmpl/stress_latest.tar --hostname hog --rootfs local-lvm:1 $mem --net0 name=eth0,bridge=vmbr0,ip=10.0.2.$((100+$1))/24 --unprivileged 1 >/dev/null 2>&1
    pct set $((300+$1)) --entrypoint 'stress --vm 4 --vm-bytes 2500M --vm-keep' >/dev/null 2>&1
    pct start $((300+$1))
  else
    local mem=""; [ "$2" != 0 ] && mem="--memory ${2}m"
    og-hostd run --name hog $mem --entrypoint stress polinux/stress -- --vm 4 --vm-bytes 2500M --vm-keep >/dev/null
  fi
}
hog_rm() { if [ $P = pve ]; then pct stop $((300+$1)) >/dev/null 2>&1; pct destroy $((300+$1)) --purge >/dev/null 2>&1; else og-hostd stop hog >/dev/null 2>&1; og-hostd rm hog >/dev/null 2>&1; fi; }
s_pressure() { # a protected web service while another container eats memory: defaults, then a 16 GB limit
  local ip; ip=$(ctr_create_start 81); waitfor 60 http_up "$ip"
  for lim in 0 16384; do
    local before; before=$(dmesg | grep -c "Killed process")
    hog_start 82 $lim
    local fails=0 max=0 lat
    for i in $(seq 300); do # 60 s
      lat=$(curl -s -o /dev/null -w '%{time_total}' --max-time 2 "http://$ip/" || echo fail)
      if [ "$lat" = fail ]; then fails=$((fails+1)); else max=$(awk -v a=$max -v b=$lat 'BEGIN{print (b>a)?b:a}'); fi
      sleep 0.2
    done
    local tag=default; [ $lim != 0 ] && tag=16g
    out pressure_${tag}_web_failures "$fails" "/300"
    out pressure_${tag}_web_max_latency_ms "$(awk -v x=$max 'BEGIN{printf "%.0f", x*1000}')" ms
    out pressure_${tag}_killed "$(dmesg | grep "Killed process" | tail -n +$((before+1)) | sed -n 's/.*Killed process [0-9]* (\([^)]*\)).*/\1/p' | sort | uniq -c | awk '{printf "%s:%s ", $2, $1}')"
    hog_rm 82; sleep 5
  done
  ctr_rm 81
}

s_vmdensity() { # 1 GB guests started until one doesn't come up within 3 minutes, or 16
  local n=0 s; s=$(t)
  for i in $(seq 61 76); do
    vm_clone $i 1; vm_start $i
    if waitfor 180 vm_ready $i; then n=$((n+1)); else echo "WARN v$i not ready"; break; fi
  done
  sleep 180
  out vm_density_1g_guests "$n" "(max 16)"
  out vm_density_host_used_mb "$(memsettled)" MB
  out vm_density_ksm_saved_mb "$(( $(cat /sys/kernel/mm/ksm/pages_sharing) * 4 / 1024 ))" MB
  for i in $(seq 61 76); do vm_rm $i; done
}

s_pbs() { # Proxmox only: the same VM backup into Proxmox Backup Server (deduplicated, incremental)
  [ $P = pve ] || return 0
  systemctl start proxmox-backup proxmox-backup-proxy; sleep 3
  vm_clone 52 0; vm_start 52; waitfor 180 vm_ready 52
  $GSSH bench@"$(vm_ip 52)" 'sudo dd if=/dev/urandom of=/var/tmp/blob bs=1M count=1024 status=none; sync'
  qm shutdown 152 --timeout 60 >/dev/null 2>&1; sleep 2
  local s; s=$(t); vzdump 152 --storage pbs --mode stop >/dev/null 2>&1; out pbs_backup_vm_s "$(since $s)" s
  out pbs_store_mb "$(( $(du -sb /var/lib/pbs-store | cut -f1) / 1048576 ))" MB
  s=$(t); vzdump 152 --storage pbs --mode stop >/dev/null 2>&1; out pbs_backup_vm_again_s "$(since $s)" s
  local vol; vol=$(pvesm list pbs --vmid 152 | awk 'NR>1{print $1}' | tail -1)
  qm destroy 152 --purge >/dev/null 2>&1
  s=$(t); qmrestore "$vol" 152 --storage local-lvm >/dev/null 2>&1; out pbs_restore_vm_s "$(since $s)" s
  qm destroy 152 --purge >/dev/null 2>&1
  for v in $(pvesm list pbs | awk 'NR>1{print $1}'); do pvesm free "$v" >/dev/null 2>&1; done
  proxmox-backup-manager garbage-collection start store >/dev/null 2>&1
  systemctl stop proxmox-backup-proxy proxmox-backup
}

s_dockerlxc() { # Proxmox only: Docker inside a Debian LXC container (privileges: nesting, keyctl)
  [ $P = pve ] || return 0
  if ! pct status 250 >/dev/null 2>&1; then
    ls /var/lib/vz/template/cache/debian-13-standard_* >/dev/null 2>&1 || pveam download local debian-13-standard_13.6-1_amd64.tar.zst >/dev/null
    pct create 250 local:vztmpl/debian-13-standard_13.6-1_amd64.tar.zst --hostname dockerhost --cores 4 --memory 6144 --swap 0 \
      --rootfs local-lvm:16 --net0 name=eth0,bridge=vmbr0,ip=10.0.2.250/24,gw=10.0.2.2 --nameserver 10.0.2.3 \
      --unprivileged 1 --features nesting=1,keyctl=1 >/dev/null
    pct start 250; sleep 5
    pct exec 250 -- sh -c 'apt-get -qq update >/dev/null 2>&1; DEBIAN_FRONTEND=noninteractive apt-get -qq install -y docker.io curl >/dev/null 2>&1'
  fi
  pct status 250 | grep -q running || { pct start 250; sleep 5; }
  pct push 250 "$(dirname "$0")/dockerlxc.sh" /root/dockerlxc.sh
  sleep 10; sync; echo 3 > /proc/sys/vm/drop_caches; sleep 2
  out dockerlxc_platform_mem_mb "$(( $(cat /sys/fs/cgroup/lxc/250/memory.current) / 1048576 ))" MB
  pct exec 250 -- env RUNS=$RUNS DENSITY=${DENSITY:-30} sh /root/dockerlxc.sh | sed 's/^RESULT /RESULT dockerlxc_/'
  pct stop 250
}

for sec in "${@:-footprint security api containers density ctrnet ctrbackup pressure dockerlxc vms vmdensity vmdisk vmnet backup pbs}"; do for x in $sec; do echo "== $x"; s_$x; done; done
