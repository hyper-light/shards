package topdown

// Copied as a test-only diagnostic into an isolated OPA v1.14.1 source copy.
// Original regex.go/regex_template.go bytes are checked before and after the run.
import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"testing"

	"github.com/open-policy-agent/opa/v1/ast"
	"github.com/open-policy-agent/opa/v1/topdown/builtins"
)

type shardsCacheCall struct {
	Name string   `json:"name"`
	Args []string `json:"args"`
}
type shardsCacheCase struct {
	Name     string            `json:"name"`
	Calls    []shardsCacheCall `json:"calls"`
	Expected []bool            `json:"expected_pure"`
}

func shardsCacheClear() {
	regexpCacheLock.Lock()
	clear(regexpCache)
	regexpCacheLock.Unlock()
}
func shardsCacheCount() int {
	regexpCacheLock.RLock()
	n := len(regexpCache)
	regexpCacheLock.RUnlock()
	return n
}
func shardsCacheCallBuiltin(t *testing.T, name string, args []string) bool {
	t.Helper()
	f := GetBuiltin(name)
	if f == nil {
		t.Fatalf("missing builtin %s", name)
	}
	operands := make([]*ast.Term, 0, len(args))
	for _, s := range args {
		operands = append(operands, ast.StringTerm(s))
	}
	// A fresh context every call proves retention outlives an evaluation context.
	bctx := BuiltinContext{Context: context.Background(), Cache: builtins.Cache{}}
	var got *ast.Term
	err := f(bctx, operands, func(value *ast.Term) error { got = value; return nil })
	if err != nil || got == nil {
		t.Fatalf("%s: value=%v error=%v", name, got, err)
	}
	value, err := ast.JSON(got.Value)
	if err != nil {
		t.Fatal(err)
	}
	answer, ok := value.(bool)
	if !ok {
		t.Fatalf("non-bool %v", value)
	}
	return answer
}
func shardsCacheEmit(t *testing.T, prefix string, value any) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	fmt.Printf("%s %s\n", prefix, data)
}
func TestShardsCacheProbe(t *testing.T) {
	defer shardsCacheClear()
	data, err := os.ReadFile(os.Getenv("SHARDS_REGO_CACHE_CASES"))
	if err != nil {
		t.Fatal(err)
	}
	var cases []shardsCacheCase
	if err = json.Unmarshal(data, &cases); err != nil {
		t.Fatal(err)
	}
	pure := true
	for _, c := range cases {
		shardsCacheClear()
		answers := make([]bool, 0, len(c.Calls))
		retained := make([]int, 0, len(c.Calls))
		for _, call := range c.Calls {
			answers = append(answers, shardsCacheCallBuiltin(t, call.Name, call.Args))
			retained = append(retained, shardsCacheCount())
		}
		for i, a := range answers {
			pure = pure && a == c.Expected[i]
		}
		shardsCacheEmit(t, "REGOCACHE_CASE", map[string]any{"implementation": "opa-v1.14.1", "name": c.Name, "answers": answers, "expected_pure": c.Expected, "entries_after_context_drop": retained})
	}
	for _, kind := range []string{"ordinary", "template"} {
		for _, multiple := range []int{1, 2, 4} {
			shardsCacheClear()
			runtime.GC()
			var before, after runtime.MemStats
			runtime.ReadMemStats(&before)
			n := regexCacheMaxSize * multiple
			for i := range n {
				pattern := fmt.Sprintf("cache-probe-%d", i)
				name := "regex.match"
				args := []string{pattern, pattern}
				if kind == "template" {
					name = "regex.template_match"
					args = append(args, "<", ">")
				}
				if !shardsCacheCallBuiltin(t, name, args) {
					t.Fatal("literal no longer matches itself")
				}
			}
			runtime.GC()
			runtime.ReadMemStats(&after)
			retained := shardsCacheCount()
			shardsCacheCallBuiltin(t, "regex.match", []string{"cache-probe-extra", "cache-probe-extra"})
			shardsCacheEmit(t, "REGOCACHE_RETAIN", map[string]any{"implementation": "opa-v1.14.1", "kind": kind, "calls": n, "declared_max": regexCacheMaxSize, "entries_after_context_drop": retained, "entries_after_ordinary_extra": shardsCacheCount(), "heap_alloc_before": before.HeapAlloc, "heap_alloc_after_gc": after.HeapAlloc, "total_alloc_delta": after.TotalAlloc - before.TotalAlloc, "mallocs_delta": after.Mallocs - before.Mallocs})
		}
	}
	if os.Getenv("SHARDS_REGO_CACHE_STRICT") == "1" && !pure {
		t.Fatal("regex results depend on prior compilation kind or delimiter choice")
	}
}
