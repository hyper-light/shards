// D113's spy: runs docker/dockerfile's own frontend as a child, passes BuildKit's stdio
// (the gateway's gRPC connection) through untouched, and logs every chunk each way to
// stderr, which buildkitd logs, with its arguments and environment first.
package main

import (
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"os/exec"
	"sync"
	"time"
)

func main() {
	var mu sync.Mutex
	say := func(format string, a ...any) {
		mu.Lock()
		fmt.Fprintf(os.Stderr, "SHARDS-D113-SPY "+format+"\n", a...)
		mu.Unlock()
	}
	say("ARGS %q", os.Args)
	for _, e := range os.Environ() {
		say("ENV %q", e)
	}
	cmd := exec.Command("/bin/dockerfile-frontend")
	cmd.Env = os.Environ()
	cmd.Stderr = os.Stderr
	in, err := cmd.StdinPipe()
	if err != nil {
		say("ERR stdin %v", err)
		os.Exit(1)
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		say("ERR stdout %v", err)
		os.Exit(1)
	}
	if err := cmd.Start(); err != nil {
		say("ERR start %v", err)
		os.Exit(1)
	}
	relay := func(dir string, from io.Reader, to io.Writer, done func()) {
		buf := make([]byte, 1<<16)
		for {
			n, err := from.Read(buf)
			if n > 0 {
				say("%s %d %s", dir, time.Now().UnixNano(), hex.EncodeToString(buf[:n]))
				if _, werr := to.Write(buf[:n]); werr != nil {
					say("ERR %s write %v", dir, werr)
					break
				}
			}
			if err != nil {
				say("EOF %s %v", dir, err)
				break
			}
		}
		done()
	}
	go relay("IN", os.Stdin, in, func() { in.Close() })
	relay("OUT", out, os.Stdout, func() {})
	err = cmd.Wait()
	say("EXIT %v", err)
}
