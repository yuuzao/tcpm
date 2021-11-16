#!/usr/bin/sh

cargo b --release
ext=$?
if [[ $ext -ne 0 ]]; then
	exit $ext
fi
echo "=================="

sudo setcap cap_net_admin=eip $PWD/target/release/mytcp
$PWD/target/release/mytcp &

sudo ip addr add 192.168.0.1/24 dev mytcp
sudo ip link set up dev mytcp

echo "=================="

pid=$!
trap "kill $pid" INT TERM
wait $pid


