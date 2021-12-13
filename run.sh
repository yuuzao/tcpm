#!/usr/bin/sh

nic="tcpm"
# target=$PWD/target/release/$nic
target=$PWD/target/debug/$nic
# cargo b --release
cargo b 
ext=$?
if [[ $ext -ne 0 ]]; then
	exit $ext
fi
echo "cargo done =================="

sudo setcap cap_net_admin=eip $target
(sleep 2 && sudo ip addr add 192.168.0.1/24 dev tcpm)&
(sleep 2 && sudo ip link set up dev tcpm)&
echo "ip settings done =================="
$target 
