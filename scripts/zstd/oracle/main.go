// What klauspost/compress v1.19.2's zstd writer, as BuildKit's util/compression drives
// it (zstd.NewWriter, with zstd.WithEncoderLevel(zstd.EncoderLevelFromZstd(level)) when
// a level is given, written in pieces as a layer's tar is, closed), makes of a corpus
// that crates/zstd/tests/oracle.rs generates again byte for byte: each case's input
// digest, and its output's length and digest (the output itself when small). The
// writer's concurrency is BuildKit's default (GOMAXPROCS, above 1), under which blocks
// after the first start from no recent offsets; one written with a concurrency of 1 is
// recorded too, as `sync_same`, to say where the two differ.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"runtime"

	"github.com/klauspost/compress/zstd"
)

type lcg struct{ s uint64 }

func (g *lcg) next() byte {
	g.s = g.s*6364136223846793005 + 1442695040888963407
	return byte(g.s >> 56)
}

var words = []string{"the", "flate", "layer", "docker", "shards", "image", "build", "tar",
	"usr", "bin", "lib", "etc", "a", "of", "and", "to"}

func text(g *lcg, n int) []byte {
	var b []byte
	for len(b) < n {
		b = append(b, words[g.next()%16]...)
		if g.next()%8 == 0 {
			b = append(b, '\n')
		} else {
			b = append(b, ' ')
		}
	}
	return b[:n]
}

func runs(g *lcg, n int) []byte {
	var b []byte
	for len(b) < n {
		v := g.next()
		l := 1 + int(g.next()%64)
		for i := 0; i < l; i++ {
			b = append(b, v)
		}
	}
	return b[:n]
}

func random(g *lcg, n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = g.next()
	}
	return b
}

func input(kind string, n int, seed uint64) []byte {
	g := &lcg{seed}
	switch kind {
	case "zeros":
		return make([]byte, n)
	case "random":
		return random(g, n)
	case "text":
		return text(g, n)
	case "runs":
		return runs(g, n)
	}
	// mixed: 997-byte chunks of text, random and runs in turn.
	var b []byte
	for i := 0; len(b) < n; i++ {
		switch i % 3 {
		case 0:
			b = append(b, text(g, 997)...)
		case 1:
			b = append(b, random(g, 997)...)
		default:
			b = append(b, runs(g, 997)...)
		}
	}
	return b[:n]
}

// compress is BuildKit's zstd compressor: no level (-1) or zstd's level, written in
// pieces of 32 KiB as io.Copy writes a layer, then closed.
func compress(in []byte, level int, concurrency int) []byte {
	var opts []zstd.EOption
	if level >= 0 {
		opts = append(opts, zstd.WithEncoderLevel(zstd.EncoderLevelFromZstd(level)))
	}
	if concurrency > 0 {
		opts = append(opts, zstd.WithEncoderConcurrency(concurrency))
	}
	var out bytes.Buffer
	w, err := zstd.NewWriter(&out, opts...)
	if err != nil {
		panic(err)
	}
	for len(in) > 0 {
		n := min(len(in), 32<<10)
		if _, err := w.Write(in[:n]); err != nil {
			panic(err)
		}
		in = in[n:]
	}
	if err := w.Close(); err != nil {
		panic(err)
	}
	return out.Bytes()
}

func main() {
	if runtime.GOMAXPROCS(0) < 2 {
		panic("BuildKit's default concurrency is GOMAXPROCS; run with more than one CPU")
	}
	kinds := []string{"zeros", "random", "text", "runs", "mixed"}
	sizes := []int{0, 1, 2, 3, 15, 16, 17, 100, 1000, 1024, 1025, 4096, 65535, 65536, 65537,
		131071, 131072, 131073, 262144, 1 << 20, 9 << 20}
	// No level (BuildKit's default), then zstd's levels across the four encoders:
	// fastest (1), default (3, 5), better (7) and best (11, 19, 22).
	levels := []int{-1, 1, 3, 5, 7, 11, 19, 22}
	var cases []map[string]any
	seed := uint64(0)
	add := func(kind string, n int, levels []int) {
		seed++
		in := input(kind, n, seed)
		inSum := sha256.Sum256(in)
		for _, level := range levels {
			out := compress(in, level, 0)
			sum := sha256.Sum256(out)
			sync := compress(in, level, 1)
			c := map[string]any{
				"kind": kind, "size": n, "seed": seed, "level": level,
				"input":     hex.EncodeToString(inSum[:]),
				"length":    len(out),
				"digest":    hex.EncodeToString(sum[:]),
				"sync_same": bytes.Equal(out, sync),
			}
			if len(out) <= 512 {
				c["output"] = hex.EncodeToString(out)
			}
			cases = append(cases, c)
		}
	}
	for _, k := range kinds {
		for _, n := range sizes {
			add(k, n, levels)
		}
	}
	// Past the history's capacity, where it moves down (16 MiB; 8 MiB at the fastest).
	for _, k := range []string{"mixed", "text"} {
		add(k, 20<<20+1000, levels)
	}
	out, err := json.MarshalIndent(map[string]any{"go": runtime.Version(), "cases": cases}, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(out, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Println(len(cases), "cases")
}
