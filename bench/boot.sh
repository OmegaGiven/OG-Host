#!/bin/bash
# boot.sh NAME SSHPORT UIPORT RUNS — power-on to management ready (see ready.sh)
N=$1; SP=$2; UP=$3; R=${4:-3}; cd ~/bench-vms
for i in $(seq $R); do
  s=$(date +%s.%N)
  ./vm.sh $N $SP $UP -daemonize -pidfile $N.pid
  until ./ready.sh $N $SP $UP; do sleep 0.2; done
  echo "$N boot_to_ready_s $(awk -v a=$s -v b=$(date +%s.%N) 'BEGIN{printf "%.1f", b-a}')"
  ./pssh $SP 'systemctl poweroff' 2>/dev/null; while ./isup.sh $N; do sleep 0.5; done
done
