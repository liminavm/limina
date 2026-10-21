#!/bin/bash
# ab.sh <rounds> <secs> — run in the guest (~/mc). Each round runs every arm once, in a shuffled
# order: `bench <secs>` on all vCPUs, with mmio_cost.ko loaded in the arm's mode (or nothing).
# Prints one line per run: round arm total_Miter, plus the module's per-CPU fire/read counts.
set -u
R=${1:-8}; S=${2:-10}
cd ~/mc
ARMS=(base tick-ram tick-mmio kthread-ram kthread-mmio tick-mmio-x10)
args() {
	case $1 in
	tick-ram) echo "mode=tick ram=1 nreads=2" ;;
	tick-mmio) echo "mode=tick nreads=2" ;;
	kthread-ram) echo "mode=kthread ram=1 nreads=2" ;;
	kthread-mmio) echo "mode=kthread nreads=2" ;;
	tick-mmio-x10) echo "mode=tick nreads=20" ;;
	esac
}
for r in $(seq 1 "$R"); do
	for a in $(printf '%s\n' "${ARMS[@]}" | shuf); do
		sudo dmesg -C
		[ "$a" != base ] && sudo insmod mmio_cost.ko $(args "$a")
		sleep 1
		out=$(./bench "$S")
		[ "$a" != base ] && sudo rmmod mmio_cost
		cnt=$(sudo dmesg | grep -o 'cpu=[0-9] fires=[0-9]* reads=[0-9]*' | tr '\n' ' ')
		echo "round=$r arm=$a $out | $cnt"
	done
done
