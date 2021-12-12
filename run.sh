#!/usr/bin/sh

nic="tcpm"
target=$PWD/target/release/$nic
cargo b --release

ext=$?
if [[ $ext -ne 0 ]]; then
	exit $ext
fi
echo "cargo done =================="

sudo setcap cap_net_admin=eip $target
$target &

pid=$!
echo $pid

sudo ip addr add 192.168.0.1/24 dev $nic
sudo ip link set up dev $nic 

echo "ip settings done =================="

trap "kill $pid" INT TERM
wait $pid

