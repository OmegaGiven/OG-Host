#!/bin/bash
# cl-up.sh pve|og — boot a node pair with a private cluster LAN (QEMU multicast socket):
# node 1 on cores 8-11, node 2 on 12-15; cluster addresses 10.10.<10|20>.1/.2
K=$1; cd ~/bench-vms
if [ $K = pve ]; then A=(pve 22201 28006 pve2 22203 28008); MC=12341; NET=10.10.10; else A=(og 22202 28007 og2 22204 28009); MC=12342; NET=10.10.20; fi
for i in 0 3; do
  n=${A[$i]}; ./isup.sh $n && continue
  idx=$(( i / 3 + 1 ))
  CORES=$([ $idx = 1 ] && echo 8-11 || echo 12-15) ./vm.sh $n ${A[$((i+1))]} ${A[$((i+2))]} \
    -netdev socket,id=n1,mcast=230.0.0.1:$MC -device virtio-net-pci,netdev=n1,mac=52:54:00:aa:$([ $K = pve ] && echo 01 || echo 02):0$idx \
    -daemonize -pidfile $n.pid
done
for i in 0 3; do until curl -sk -o /dev/null --max-time 1 https://127.0.0.1:${A[$((i+2))]}/; do sleep 1; done; done
for i in 0 3; do
  idx=$(( i / 3 + 1 )); ip=$NET.$idx
  ./pssh ${A[$((i+1))]} "nic=\$(ip -o link | awk -F': ' '/52:54:00:aa/{print \$2}' | head -1); ip addr show dev \$nic | grep -q $ip || { ip addr add $ip/24 dev \$nic; ip link set \$nic up; }; echo ${A[$i]} \$nic $ip"
done
