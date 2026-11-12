// What OPA v1.14.1's builtins.FloatToNumber(NumberToFloat(s)) costs Go: SetString at 64
// bits, then Text('f' for an integer, else 'g', -1), timed, with the text's length, ends
// and SHA-256 (as crates/rego/testdata/numbers-oracle.json keeps them) and Go's
// allocation. For PM M524:
//
//	go build -o gotext . && /usr/bin/time -l ./gotext 80000000000e00029174680
package main

import (
	"crypto/sha256"
	"fmt"
	"math/big"
	"os"
	"runtime"
	"time"
)

func main() {
	s := os.Args[1]
	t0 := time.Now()
	f, ok := new(big.Float).SetString(s)
	if !ok {
		fmt.Println("refused")
		return
	}
	t1 := time.Now()
	format := byte('g')
	if f.IsInt() {
		format = 'f'
	}
	out := f.Text(format, -1)
	t2 := time.Now()
	var ms runtime.MemStats
	runtime.ReadMemStats(&ms)
	head, tail := out, out
	if len(out) > 64 {
		head, tail = out[:64], out[len(out)-64:]
	}
	fmt.Printf("prec=%d exp=%d isint=%v format=%c\nlen=%d\nhead=%s\ntail=%s\nsha256=%x\nparse=%v text=%v totalalloc=%dMB sys=%dMB\n",
		f.Prec(), f.MantExp(nil), f.IsInt(), format, len(out), head, tail, sha256.Sum256([]byte(out)), t1.Sub(t0), t2.Sub(t1), ms.TotalAlloc>>20, ms.Sys>>20)
}
