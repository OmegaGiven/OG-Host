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
pve_wait() { local u; u=$(cat); while pvesh get "/nodes/$NODE/tasks/$u/status" --output-format json | grep -q '"status":"running"'; do sleep 0.2; done; }
ctr_create_start() { # N -> prints IP when serving HTTP
  if [ $P = pve ]; then
    pct create $((300+$1)) local:vztmpl/nginx_alpine.tar --hostname c$1 --rootfs local-lvm:1 --memory 256 --swap 0 \
      --net0 name=eth0,bridge=vmbr0,ip=10.0.2.$((100+$1))/24,gw=10.0.2.2 --unprivileged 1 >/dev/null 2>&1
    pct start $((300+$1)); echo 10.0.2.$((100+$1))
  else
    og-hostd run --name c$1 --memory 256m nginx:alpine | sed -n 's/.* ip \([0-9.]*\).*/\1/p'
  fi
}
ctr_rm() { if [ $P = pve ]; then pct stop $((300+$1)) >/dev/null 2>&1; pct destroy $((300+$1)) --purge >/dev/null 2>&1; else og-hostd stop c$1 >/dev/null 2>&1; og-hostd rm c$1 >/dev/null 2>&1; fi; }
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
    for c in $(og-hostd ps | awk 'NR>1 && $2 ~ /^c[0-9]+$/ {print $2}'); do og-hostd stop $c >/dev/null 2>&1; og-hostd rm $c >/dev/null 2>&1; done
  fi
}
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
  sleep 30; out vm_mem_per_idle_1g_guest_mb "$(( ($(memsettled) - m0) / 3 ))" MB
  for i in 21 22 23; do vm_rm $i; done
}

guest_tools() { # N: install fio + iperf3 in the guest
  # after cloud-init finishes (it holds the apt lock while it runs); a few tries for mirror hiccups
  $GSSH bench@"$(vm_ip $1)" 'cloud-init status --wait >/dev/null 2>&1; for i in 1 2 3; do sudo apt-get -qq update >/dev/null 2>&1; sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y fio iperf3 >/dev/null 2>&1 && break; sleep 5; done; command -v fio && command -v iperf3' >/dev/null
}

s_vmdisk() {
  vm_clone 31 1; vm_start 31; waitfor 180 vm_ready 31; guest_tools 31 || echo "WARN no fio"
  local ip; ip=$(vm_ip 31)
  for spec in "randread 4k 32" "randwrite 4k 32" "read 1M 8" "write 1M 8"; do
    set -- $spec
    r=$($GSSH bench@"$ip" "sudo fio --name=b --filename=/var/tmp/fio --size=2G --direct=1 --ioengine=libaio --rw=$1 --bs=$2 --iodepth=$3 --runtime=20 --time_based --group_reporting --output-format=terse --terse-version=3" 2>/dev/null)
    # terse v3: read iops field 8, read KB/s 7; write iops 49, write KB/s 48
    case $1 in
      randread) out vm_fio_randread_4k_iops "$(echo "$r" | cut -d';' -f8)" iops ;;
      randwrite) out vm_fio_randwrite_4k_iops "$(echo "$r" | cut -d';' -f49)" iops ;;
      read) out vm_fio_seqread_1m_mbs "$(echo "$r" | cut -d';' -f7 | awk '{printf "%d", $1/1024}')" MB/s ;;
      write) out vm_fio_seqwrite_1m_mbs "$(echo "$r" | cut -d';' -f48 | awk '{printf "%d", $1/1024}')" MB/s ;;
    esac
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
  fi
}

for sec in "${@:-footprint containers density vms vmdisk vmnet backup}"; do for x in $sec; do echo "== $x"; s_$x; done; done
