#!/bin/bash
# run-host.sh NAME SSHPORT UIPORT — other bench VMs off, fresh boot, 2 min idle, the suite; results/NAME${TAG}.txt
N=$1; SP=$2; UP=$3; cd ~/bench-vms
for o in "pve 22201" "og 22202" "pve2 22203" "og2 22204" "deb 22205" "incus 22206"; do set -- $o; ./pssh $2 'systemctl poweroff' 2>/dev/null; while ./isup.sh $1; do sleep 1; done; done
./vm.sh $N $SP $UP -daemonize -pidfile $N.pid
until ./ready.sh $N $SP $UP; do sleep 1; done
sleep 120
S="scp -q -i key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"; $S -P $SP -r /home/omegagiven/projects/og-host/bench root@127.0.0.1:/root/
./pssh $SP "RUNS=3 DENSITY=30 /root/bench/bench.sh ${SECTIONS:-}" > results/$N${TAG:-}.txt 2>&1
echo "$N done"
