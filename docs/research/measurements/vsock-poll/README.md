# What the vsock device's fresh kqueue per wait costs a run (PM M50)

`count.patch` makes each macOS wait print its process, its interests, and the time from
entry to the blocking `kevent`: the kqueue, its registrations and the arrays, which a
persistent kqueue would not pay. Apply it, build, sign `shardsd` and `shards-vm` with
`resources/hvf.entitlements`, and run `shards run --pull never --rm alpine true` in a
home of their own; the lines land in the home's `daemon.log`:

    grep '^vsock-poll' HOME/daemon.log | awk '{n[$2]++; ns+=$4; c++}
      END {print length(n), "VMs,", c/length(n), "waits each,", ns/c/1000, "µs a wait"}'

Warm VMs are ended when their run is done, so a process's last line may be cut short.
