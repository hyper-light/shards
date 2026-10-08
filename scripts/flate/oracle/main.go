// What Go's compress/gzip writer, as BuildKit's util/compression drives it
// (gzip.NewWriterLevel, written whole, closed), makes of a corpus that
// crates/flate/tests/oracle.rs generates again byte for byte: each case's input
// digest, and its output's length and digest (the output itself when small).
package main

import (
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"runtime"
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

func main() {
	kinds := []string{"zeros", "random", "text", "runs", "mixed"}
	sizes := []int{0, 1, 2, 3, 4, 5, 15, 16, 17, 64, 100, 127, 128, 129, 258, 259, 1000, 4096,
		32768, 32769, 65534, 65535, 65536, 65537, 100000, 262144, 1 << 20}
	var cases []map[string]any
	seed := uint64(0)
	add := func(kind string, n int, levels []int) {
		seed++
		in := input(kind, n, seed)
		inSum := sha256.Sum256(in)
		for _, level := range levels {
			var out bytes.Buffer
			w, err := gzip.NewWriterLevel(&out, level)
			if err != nil {
				panic(err)
			}
			if _, err := w.Write(in); err != nil {
				panic(err)
			}
			if err := w.Close(); err != nil {
				panic(err)
			}
			sum := sha256.Sum256(out.Bytes())
			c := map[string]any{
				"kind": kind, "size": n, "seed": seed, "level": level,
				"input": hex.EncodeToString(inSum[:]),
				"length": out.Len(), "digest": hex.EncodeToString(sum[:]),
			}
			if out.Len() <= 512 {
				c["output"] = hex.EncodeToString(out.Bytes())
			}
			cases = append(cases, c)
		}
	}
	all := []int{-2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9}
	for _, k := range kinds {
		for _, n := range sizes {
			add(k, n, all)
		}
	}
	// Past the hash offset's rebase (1<<24) and many window shifts.
	for _, k := range []string{"mixed", "text"} {
		add(k, 20<<20+1000, []int{-1, 1, 9})
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
