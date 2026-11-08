# Which preemption model gives PID 1 its CPU back at boot (PM M65)

In about 30% of cold boots on arm64, PID 1 waits a tick, 10 ms at `CONFIG_HZ=100`, for
the crypto self-tests' kthreads to give up the one vCPU (M39); the kernel is
`PREEMPT_NONE`. `.github/workflows/boot-preempt.yml` builds the pinned kernel in the
pinned builder with `preempt.config` added to `resources/kernel/shards.config`, which
makes the model a boot parameter, and uploads it as an artifact: nothing is published.
Then, on the Mac:

    gh run download RUN -n kernel-preempt-aarch64 -D target/kernel-preempt
    K=target/kernel-preempt/Image-6.18.48-aarch64
    docs/research/measurements/kernel-ab/ab.py --bin target/e2e/shards-… \
        --init target/guest/…/shards-init --guest target/guest/…/shards-testguest \
        --a $K --b $K --a-cmdline preempt=none --b-cmdline preempt=voluntary
