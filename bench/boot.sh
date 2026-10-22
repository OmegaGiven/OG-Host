#!/bin/bash
# boot.sh NAME SSHPORT UIPORT RUNS — power-on to web UI answering (https :8006), and to SSH
N=$1; SP=$2; UP=$3; R=${4:-3}
for i in $(seq $R); do
  s=$(date +%s.%N)
  ./vm.sh $N $SP $UP -daemonize -pidfile $N.pid
  until curl -sk -o /dev/null --max-time 1 https://127.0.0.1:$UP/; do sleep 0.2; done
  ui=$(awk -v a=$s -v b=$(date +%s.%N) 'BEGIN{printf "%.1f", b-a}')
  echo "$N boot_to_ui_s $ui"
  ssh -q -i key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p $SP root@127.0.0.1 'systemctl poweroff' 2>/dev/null
  while kill -0 $(cat $N.pid) 2>/dev/null; do sleep 0.5; done
done
