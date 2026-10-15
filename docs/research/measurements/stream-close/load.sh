#!/bin/sh
# In the guest (run.sh): the stream test 8 at once, 25 runs each, then the kernel's TCP aborts and resets.
loop() {
  n=0
  while [ $n -lt 25 ]; do
    n=$((n+1))
    /t/shards-tests a_stream_closed_delivers_all_it_wrote --test-threads=1 -q > /scratch/out-$1 2>&1 || { echo "FAILED in loop $1 run $n"; grep PROBE /scratch/out-$1; }
  done
  echo "loop $1 done"
}
for i in 1 2 3 4 5 6 7 8; do loop $i & done
wait
awk '$1=="TcpExt:" { if (!h) { for (i=2;i<=NF;i++) k[i]=$i; h=1 } else { for (i=2;i<=NF;i++) if ($i!=0 && k[i] ~ /Abort|Reset|Rst|Prune|Collapse|Drop/) print k[i], $i } }' /proc/net/netstat
awk '$1=="Tcp:" { if (!h) { for (i=2;i<=NF;i++) k[i]=$i; h=1 } else { for (i=2;i<=NF;i++) if (k[i] ~ /Rst|Fails|Resets/) print k[i], $i } }' /proc/net/snmp
