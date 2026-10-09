package oracle

// OPA's side of shards-rego's benchmarks (crates/rego/benches/policy.rs): each policy of
// SHARDS_BENCH_POLICIES (a JSON list of {name, file, input?, unknowns?}) run as buildx
// v0.37.1 runs one per source it checks (policy/validate.go: rego.New with its options,
// then Eval, or Partial over unknowns), SHARDS_BENCH_N times after one warm-up, and
// evaluated again on a prepared query, its compile set aside; the times' n, p50, p90, p99
// and max, and the process's peak resident memory, one JSON line a policy on stdout.
//
// `scripts/rego/bench-opa POLICIES.json [N]`

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"syscall"
	"testing"
	"time"

	"github.com/open-policy-agent/opa/v1/ast"
	"github.com/open-policy-agent/opa/v1/rego"
)

type benchCase struct {
	Name     string   `json:"name"`
	File     string   `json:"file"`
	Input    string   `json:"input,omitempty"`
	Unknowns []string `json:"unknowns,omitempty"`
}

type dist struct {
	N   int     `json:"n"`
	P50 float64 `json:"p50_us"`
	P90 float64 `json:"p90_us"`
	P99 float64 `json:"p99_us"`
	Max float64 `json:"max_us"`
}

func summarize(ds []time.Duration) dist {
	us := make([]float64, len(ds))
	for i, d := range ds {
		us[i] = float64(d.Nanoseconds()) / 1e3
	}
	slices.Sort(us)
	// Nearest rank, as crates/rego/benches/policy.rs ranks.
	at := func(q float64) float64 {
		if len(us) == 0 {
			return 0
		}
		rank := max(int(math.Ceil(q*float64(len(us)))), 1)
		return us[min(rank, len(us))-1]
	}
	return dist{N: len(us), P50: at(0.50), P90: at(0.90), P99: at(0.99), Max: at(1)}
}

func peakRSS() int64 {
	var ru syscall.Rusage
	_ = syscall.Getrusage(syscall.RUSAGE_SELF, &ru)
	// Bytes on macOS, kibibytes on Linux.
	if runtime.GOOS == "darwin" {
		return ru.Maxrss
	}
	return ru.Maxrss * 1024
}

func options(c benchCase, src string, in any) []func(*rego.Rego) {
	caps := &ast.Capabilities{Builtins: builtins(), Features: slices.Clone(ast.Features)}
	for _, h := range hostDecls {
		caps.Builtins = append(caps.Builtins, &ast.Builtin{Name: h.name, Decl: h.decl})
	}
	comp := ast.NewCompiler().WithCapabilities(caps).WithKeepModules(true).WithEnablePrintStatements(true)
	opts := []func(*rego.Rego){
		rego.SetRegoVersion(ast.RegoV1),
		rego.Query("data.docker.decision"),
		rego.SkipPartialNamespace(true),
		rego.Compiler(comp),
		rego.Module("builtin/buildx_defaults.rego", builtinsRego),
		rego.Module(c.File, src),
		rego.EnablePrintStatements(true),
		rego.PrintHook(&hook{}),
	}
	for _, hd := range hostDecls {
		f := &rego.Function{Name: hd.name, Decl: hd.decl}
		opts = append(opts, rego.FunctionDyn(f, func(_ rego.BuiltinContext, _ []*ast.Term) (*ast.Term, error) {
			return nil, nil
		}))
	}
	if in != nil {
		opts = append(opts, rego.Input(in))
	}
	if c.Unknowns != nil {
		opts = append(opts, rego.Unknowns(c.Unknowns))
	}
	return opts
}

func once(c benchCase, src string, in any) error {
	ctx := context.Background()
	r := rego.New(options(c, src, in)...)
	if c.Unknowns != nil {
		_, err := r.Partial(ctx)
		return err
	}
	_, err := r.Eval(ctx)
	return err
}

func TestShardsBench(t *testing.T) {
	list := os.Getenv("SHARDS_BENCH_POLICIES")
	if list == "" {
		t.Skip("SHARDS_BENCH_POLICIES names the policies")
	}
	n, err := strconv.Atoi(os.Getenv("SHARDS_BENCH_N"))
	if err != nil || n < 1 {
		n = 100
	}
	dt, err := os.ReadFile(list)
	if err != nil {
		t.Fatal(err)
	}
	var cases []benchCase
	if err := json.Unmarshal(dt, &cases); err != nil {
		t.Fatal(err)
	}
	base := filepath.Dir(list)
	for _, c := range cases {
		src, err := os.ReadFile(filepath.Join(base, c.File))
		if err != nil {
			t.Fatal(err)
		}
		var in any
		if c.Input != "" {
			raw, err := os.ReadFile(filepath.Join(base, c.Input))
			if err != nil {
				t.Fatal(err)
			}
			if err := json.Unmarshal(raw, &in); err != nil {
				t.Fatal(err)
			}
		}
		if err := once(c, string(src), in); err != nil {
			fmt.Printf("{\"name\":%q,\"error\":%q}\n", c.Name, err.Error())
			continue
		}
		// What buildx does per check: options, compile and evaluation together.
		var whole []time.Duration
		for i := 0; i < n; i++ {
			start := time.Now()
			if err := once(c, string(src), in); err != nil {
				t.Fatal(err)
			}
			whole = append(whole, time.Since(start))
		}
		// Evaluation alone, on a query prepared once.
		ctx := context.Background()
		var evals []time.Duration
		if c.Unknowns == nil {
			pq, err := rego.New(options(c, string(src), in)...).PrepareForEval(ctx)
			if err != nil {
				t.Fatal(err)
			}
			for i := 0; i < n; i++ {
				start := time.Now()
				if in != nil {
					_, err = pq.Eval(ctx, rego.EvalInput(in))
				} else {
					_, err = pq.Eval(ctx)
				}
				if err != nil {
					t.Fatal(err)
				}
				evals = append(evals, time.Since(start))
			}
		}
		line, _ := json.Marshal(map[string]any{
			"name":         c.Name,
			"whole":        summarize(whole),
			"eval":         summarize(evals),
			"peak_rss_mib": peakRSS() >> 20,
		})
		fmt.Println(string(line))
	}
}
