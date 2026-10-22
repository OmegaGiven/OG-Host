#!/bin/bash
# setup-og.sh SSHPORT — a fresh OG-Host bench node: tools, bench scripts, key, nginx image, the "tpl" VM template
P=$1; cd ~/bench-vms; S="scp -q -i key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
./pssh $P 'mkdir -p /var/lib/og-host/iso'
$S -P $P dl/debian-13-genericcloud-amd64.qcow2 root@127.0.0.1:/var/lib/og-host/iso/debian.qcow2
$S -P $P key key.pub root@127.0.0.1:/root/
$S -P $P -r /home/omegagiven/projects/og-host/bench root@127.0.0.1:/root/
./pssh $P 'apt-get -qq update >/dev/null 2>&1; DEBIAN_FRONTEND=noninteractive apt-get -qq install -y iperf3 lynis >/dev/null 2>&1
cat > /usr/local/bin/ogapi <<"X"
#!/bin/sh
M=$1; P=$2; B=${3:-}
if [ -n "$B" ]; then curl -s -X $M -H "Authorization: Bearer $(cat /var/lib/og-host/api-token)" -H content-type:application/json -d "$B" http://127.0.0.1:8411$P
else curl -s -X $M -H "Authorization: Bearer $(cat /var/lib/og-host/api-token)" http://127.0.0.1:8411$P; fi
X
cat > /usr/local/bin/ogtask <<"X"
#!/bin/sh
while :; do s=$(ogapi GET /v1/tasks/$1); echo "$s" | grep -q "\"status\":\"running\"" || break; sleep 0.2; done; echo "$s" | head -c 300
X
chmod +x /usr/local/bin/ogapi /usr/local/bin/ogtask
for i in nginx:alpine networkstatic/iperf3 alpine; do og-hostd pull docker.io/library/$i >/dev/null 2>&1 || og-hostd pull docker.io/$i >/dev/null; done
T=$(ogapi POST /v1/pools/local-thin/import "{\"file\":\"debian.qcow2\",\"name\":\"tpl-disk0\",\"grow\":8}" | sed -n "s/.*\"task\":\"\([^\"]*\)\".*/\1/p"); ogtask $T >/dev/null
K=$(cat /root/key.pub)
ogapi POST /v1/vms "{\"vm\":{\"name\":\"tpl\",\"cores\":1,\"memory\":1024,\"disks\":[{\"id\":\"disk0\",\"bus\":\"virtio\",\"volume\":\"local-thin:tpl-disk0\",\"discard\":true}],\"nics\":[{\"id\":\"net0\",\"model\":\"virtio\",\"network\":\"default\"}]}}" >/dev/null
ogapi POST /v1/vms/tpl/cloudinit "{\"cloudinit\":{\"user\":\"bench\",\"ssh_keys\":[\"$K\"],\"ip\":\"dhcp\",\"address\":\"\",\"gateway\":\"\",\"dns\":[],\"extra\":\"\",\"upgrade\":false},\"password\":\"\"}" >/dev/null
ogapi POST /v1/vms/tpl/template >/dev/null
echo "$(hostname): $(og-hostd --version), kernel $(uname -r), template: $(ogapi GET /v1/vms/tpl | grep -o "\"template\":true")"'
