//go:build linux

// Copied by buildkit.sh into BuildKit v0.33.0's proxyprovider package: what its own
// newCA and certForHost cost (RSA-2048), as certificate_costs measures shards' (P-256):
// a CA, a host's certificate and its TLS key pair, and one kept, n each, interleaved, wall
// and the thread's CPU time, in microseconds.
package proxyprovider

import (
	"container/list"
	"fmt"
	"os"
	"runtime"
	"sort"
	"strconv"
	"syscall"
	"testing"
	"time"
)

func threadCPU() time.Duration {
	var ru syscall.Rusage
	_ = syscall.Getrusage(1, &ru) // RUSAGE_THREAD
	return time.Duration(ru.Utime.Nano() + ru.Stime.Nano())
}

func quantiles(took []time.Duration) string {
	sort.Slice(took, func(i, j int) bool { return took[i] < took[j] })
	q := func(p int) int64 {
		i := len(took) * p / 100
		if i >= len(took) {
			i = len(took) - 1
		}
		return took[i].Microseconds()
	}
	return fmt.Sprintf("p50 %d p90 %d p99 %d max %d", q(50), q(90), q(99), took[len(took)-1].Microseconds())
}

func TestShardsCertificateCosts(t *testing.T) {
	n := 200
	if s := os.Getenv("SHARDS_N"); s != "" {
		n, _ = strconv.Atoi(s)
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	caPEM, ca, key, err := newCA()
	if err != nil {
		t.Fatal(err)
	}
	p := &provider{caPEM: caPEM, ca: ca, caKey: key, certs: map[string]*certCacheEntry{}, lru: list.New()}
	if _, err := p.certForHost("kept.example"); err != nil {
		t.Fatal(err)
	}
	names := []string{"CA, RSA-2048 (BuildKit)", "host certificate and TLS, RSA-2048 (BuildKit)", "host certificate kept (BuildKit)"}
	wall := make([][]time.Duration, len(names))
	cpu := make([][]time.Duration, len(names))
	for i := 0; i < n; i++ {
		host := fmt.Sprintf("h%d.example", i)
		work := []func(){
			func() {
				if _, _, _, err := newCA(); err != nil {
					t.Fatal(err)
				}
			},
			func() {
				if _, err := p.certForHost(host); err != nil {
					t.Fatal(err)
				}
			},
			func() {
				if _, err := p.certForHost("kept.example"); err != nil {
					t.Fatal(err)
				}
			},
		}
		for k, f := range work {
			w, c := time.Now(), threadCPU()
			f()
			wall[k] = append(wall[k], time.Since(w))
			cpu[k] = append(cpu[k], threadCPU()-c)
		}
	}
	for k, name := range names {
		fmt.Printf("%s: n=%d wall %s\n", name, n, quantiles(wall[k]))
		fmt.Printf("%s: n=%d cpu %s\n", name, n, quantiles(cpu[k]))
	}
}
