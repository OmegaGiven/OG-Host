#!/bin/bash
# cluster-bench.sh pve|og [RUNS] — on the workstation: live migration of a running VM (8 GiB
# local thin disk, 1 GiB RAM) between the two nodes and back, RUNS times; for OG-Host also an
# HA failover after node 1 is cut off (killed). Needs cl-up.sh's pair, clustered.
K=$1; R=${2:-3}; cd ~/bench-vms
GS='GSSH="ssh -q -i /root/key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=3"'
LIB='cd /root/bench; . ./lib.sh; source <(sed -n "/^# ---- platform primitives/,/^cleanup()/p" bench.sh | sed "\$d")'
./cl-up.sh $K >/dev/null
med() { sort -n | awk '{a[NR]=$1}END{if(NR%2)print a[(NR+1)/2]; else printf "%.2f\n",(a[NR/2]+a[NR/2+1])/2}'; }
tot=(); down=()
if [ $K = pve ]; then
  ./pssh 22201 "$LIB; P=pve; NODE=\$(hostname); $GS; qm status 160 >/dev/null 2>&1 || { vm_clone 60 0; }; qm start 160 2>/dev/null; waitfor 180 vm_ready 60"
  for i in $(seq $R); do
    for dir in "22201 pve-bench2" "22203 pve-bench"; do set -- $dir
      o=$(./pssh $1 'cd /root/bench; . ./lib.sh; s=$(t); qm migrate 160 '$2' --online --with-local-disks --targetstorage local-lvm --migration_network 10.10.10.0/24 > /tmp/mig.log 2>&1; echo "rc=$? total=$(since $s) down=$(sed -n "s/.*downtime \([0-9]*\) ms.*/\1/p" /tmp/mig.log | tail -1)"')
      echo "DIAG pve migrate to $2: $o"; tot+=("$(echo "$o" | sed -n 's/.*total=\([0-9.]*\).*/\1/p')"); down+=("$(echo "$o" | sed -n 's/.*down=\([0-9]*\).*/\1/p')")
    done
  done
  echo "RESULT migrate_live_s $(printf '%s\n' "${tot[@]}" | med) s"
  echo "RESULT migrate_downtime_ms $(printf '%s\n' "${down[@]}" | med) ms"
  echo "RESULT ha_failover_s n/a (Proxmox HA needs shared storage or ZFS replication, and a third vote, for VMs on local disks)"
else
  ./pssh 22202 "$LIB; P=og; $GS; ogapi GET /v1/vms/v60 | grep -q '\"name\"' || vm_clone 60 0; ogapi POST /v1/vms/v60/start >/dev/null; waitfor 180 vm_ready 60"
  for i in $(seq $R); do
    for dir in "22202 og-bench2" "22204 og-bench"; do set -- $dir
      o=$(./pssh $1 'cd /root/bench; . ./lib.sh; s=$(t); T=$(ogapi POST /v1/vms/v60/migrate "{\"node\":\"'$2'\"}" | sed -n "s/.*\"task\":\"\([^\"]*\)\".*/\1/p"); st=$(ogtask $T | grep -o "\"status\":\"[a-z]*\""); echo "st=$st total=$(since $s) down=$(ogapi GET /v1/tasks/$T | sed -n "s/.*downtime \([0-9]*\) ms.*/\1/p")"')
      echo "DIAG og migrate to $2: $o"; tot+=("$(echo "$o" | sed -n 's/.*total=\([0-9.]*\).*/\1/p')"); down+=("$(echo "$o" | sed -n 's/.*down=\([0-9]*\).*/\1/p')")
    done
  done
  echo "RESULT migrate_live_s $(printf '%s\n' "${tot[@]}" | med) s"
  echo "RESULT migrate_downtime_ms $(printf '%s\n' "${down[@]}" | med) ms"
  # HA: replicate every minute to node 2, then cut node 1 off
  ./pssh 22202 'ogapi PUT /v1/ha/witness "{\"witness\":\"10.0.2.2\"}" >/dev/null; ogapi GET /v1/storage | grep -q "\"id\":\"ha\"" || ogapi POST /v1/storage "{\"id\":\"ha\",\"type\":\"dir\",\"path\":\"/var/lib/og-host/ha-store\"}" >/dev/null; ogapi PUT /v1/ha/v60 "{\"enabled\":true,\"owner\":\"og-bench\",\"standby\":\"og-bench2\",\"interval_min\":1,\"store\":\"ha\"}" >/dev/null; for i in $(seq 120); do sleep 5; ogapi GET /v1/ha | grep -q "\"last_replica_s\":[0-9]" && break; done'
  ./pssh 22204 'ogapi GET /v1/storage | grep -q "\"id\":\"ha\"" || ogapi POST /v1/storage "{\"id\":\"ha\",\"type\":\"dir\",\"path\":\"/var/lib/og-host/ha-store\"}" >/dev/null'
  kill -9 "$(cat og.pid)"
  o=$(./pssh 22204 'bash /root/bench/hawatch.sh')
  echo "RESULT ha_failover_running_s $(echo "$o" | sed -n 's/running_after_s //p') s"
  echo "RESULT ha_failover_ssh_s $(echo "$o" | sed -n 's/ssh_after_s //p') s"
fi
