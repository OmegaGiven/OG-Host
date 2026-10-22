#!/bin/bash
# hawatch.sh (on the standby): time until VM v60 runs here and answers SSH
cd /root/bench; . ./lib.sh; GSSH="ssh -q -i /root/key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=2"
s=$(t); r=""
for i in $(seq 1500); do
  st=$(ogapi GET /v1/vms/v60 2>/dev/null | grep -o "\"state\":\"[a-z]*\"" | head -1)
  if [ -z "$r" ] && echo "$st" | grep -q running; then r=$(since $s); echo "running_after_s $r"; fi
  if [ -n "$r" ]; then ip=$(ogapi GET /v1/vms/v60 | grep -o "\"ip\":\"[0-9.]*\"" | head -1 | cut -d\" -f4); [ -n "$ip" ] && $GSSH bench@$ip true 2>/dev/null && { echo "ssh_after_s $(since $s)"; break; }; fi
  sleep 0.2
done
