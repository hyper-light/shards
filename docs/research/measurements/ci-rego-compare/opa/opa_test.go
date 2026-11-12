// What OPA v1.14.1's own builtins do with numbers whose exponents are large (the cases in
// cases.json, CI's fuzzing artifacts first), one case a process: run.sh runs each under a
// time limit, as OPA writes some of these texts for hours. Each run prints one JSON line:
// the outcome (result, error or panic), the result's length and first 120 characters,
// and the seconds taken.
package opa

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"testing"
	"time"

	"github.com/open-policy-agent/opa/v1/ast"
	"github.com/open-policy-agent/opa/v1/topdown"
	opabuiltins "github.com/open-policy-agent/opa/v1/topdown/builtins"
)

func TestCase(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_CASES"))
	if err != nil {
		t.Fatal(err)
	}
	var cases []struct {
		Name string            `json:"name"`
		Args []json.RawMessage `json:"args"`
	}
	if err := json.Unmarshal(dt, &cases); err != nil {
		t.Fatal(err)
	}
	i, err := strconv.Atoi(os.Getenv("SHARDS_CASE"))
	if err != nil || i < 0 || i >= len(cases) {
		t.Fatalf("SHARDS_CASE %q", os.Getenv("SHARDS_CASE"))
	}
	c := cases[i]
	out := map[string]any{"case": i, "name": c.Name}
	start := time.Now()
	defer func() {
		if r := recover(); r != nil {
			out["outcome"] = "panic"
			out["detail"] = fmt.Sprint(r)
		}
		out["seconds"] = time.Since(start).Seconds()
		line, _ := json.Marshal(out)
		fmt.Println(string(line))
	}()
	f := topdown.GetBuiltin(c.Name)
	operands := make([]*ast.Term, 0, len(c.Args))
	for _, a := range c.Args {
		v, err := ast.ValueFromReader(bytes.NewReader(a))
		if err != nil {
			t.Fatalf("%s: %v", c.Name, err)
		}
		operands = append(operands, ast.NewTerm(v))
	}
	bctx := topdown.BuiltinContext{
		Context:  context.Background(),
		Time:     ast.NumberTerm("1700000000000000000"),
		Seed:     bytes.NewReader(make([]byte, 1024)),
		Cache:    opabuiltins.Cache{},
		Location: &ast.Location{File: "p.rego", Row: 1, Col: 1},
	}
	var got *ast.Term
	err = f(bctx, operands, func(t *ast.Term) error {
		got = t
		return nil
	})
	switch {
	case err != nil:
		out["outcome"] = "error"
		out["detail"] = err.Error()
	case got == nil:
		out["outcome"] = "undefined"
	default:
		s := got.Value.String()
		out["outcome"] = "result"
		out["length"] = len(s)
		if len(s) > 120 {
			s = s[:120]
		}
		out["detail"] = s
	}
}

// TestCompares prints OPA's ast.NumberCompare of each pair of number texts in
// SHARDS_COMPARES (texts JSON cannot hold, to_number's hex floats among them), or the
// panic it ends in.
func TestCompares(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_COMPARES"))
	if err != nil {
		t.Fatal(err)
	}
	var pairs [][2]string
	if err := json.Unmarshal(dt, &pairs); err != nil {
		t.Fatal(err)
	}
	for _, p := range pairs {
		out := map[string]any{"a": p[0], "b": p[1]}
		start := time.Now()
		func() {
			defer func() {
				if r := recover(); r != nil {
					out["panic"] = fmt.Sprint(r)
				}
			}()
			out["compare"] = ast.NumberCompare(ast.Number(p[0]), ast.Number(p[1]))
		}()
		out["seconds"] = time.Since(start).Seconds()
		line, _ := json.Marshal(out)
		fmt.Println(string(line))
	}
}
