package cel_test

import (
	"encoding/json"
	"fmt"
	"runtime"
	"testing"
	"time"

	"github.com/google/cel-go/cel"
	"github.com/google/cel-go/common/types"
	"github.com/google/cel-go/interpreter"
)

// This is an ownership alternative probe, not a cross-language speed comparison.
func TestShardsCompilationOwnership(t *testing.T) {
	env, err := cel.NewEnv(cel.Variable("value", cel.StringType), cel.Variable("pattern", cel.StringType), cel.Variable("enabled", cel.BoolType))
	if err != nil {
		t.Fatal(err)
	}
	for _, mode := range []string{"baseline", "planned"} {
		for _, workload := range []string{"constant_hot", "dynamic_regex"} {
			source := `value.matches("^[a-z]+[0-9]+$")`
			if workload == "dynamic_regex" {
				source = `value.matches(pattern)`
			}
			ast, issues := env.Compile(source)
			if issues.Err() != nil {
				t.Fatal(issues.Err())
			}
			factoryCalls := 0
			options := []cel.ProgramOption{}
			if mode == "planned" {
				candidate := *interpreter.MatchesRegexOptimization
				candidate.Factory = func(call interpreter.InterpretableCall, pattern string) (interpreter.InterpretableCall, error) {
					factoryCalls++
					return interpreter.MatchesRegexOptimization.Factory(call, pattern)
				}
				options = append(options, cel.OptimizeRegex(&candidate))
			}
			var before runtime.MemStats
			runtime.GC()
			runtime.ReadMemStats(&before)
			began := time.Now()
			program, err := env.Program(ast, options...)
			setupNS := time.Since(began).Nanoseconds()
			if err != nil {
				t.Fatal(err)
			}
			ast = nil // The executable plan, rather than this AST variable, owns compiled state.
			inputs := make([]map[string]any, 2000)
			for i := range inputs {
				inputs[i] = map[string]any{"value": "alpha42", "pattern": "unused"}
				if workload == "dynamic_regex" {
					pattern := fmt.Sprintf("cache-probe-%d", i)
					inputs[i] = map[string]any{"value": pattern, "pattern": pattern}
				}
			}
			rows := make([]map[string]any, 0, 20)
			for sample := 0; sample < 20; sample++ {
				var a, b runtime.MemStats
				runtime.ReadMemStats(&a)
				began = time.Now()
				for i := 0; i < 100; i++ {
					value, _, err := program.Eval(inputs[sample*100+i])
					if err != nil || value != types.True {
						t.Fatalf("%s/%s value=%v error=%v", mode, workload, value, err)
					}
				}
				elapsed := time.Since(began).Nanoseconds()
				runtime.ReadMemStats(&b)
				rows = append(rows, map[string]any{"elapsed_ns": elapsed, "iterations": 100, "allocated_bytes": b.TotalAlloc - a.TotalAlloc, "allocations": b.Mallocs - a.Mallocs})
			}
			expected := 0
			if mode == "planned" && workload == "constant_hot" {
				expected = 1
			}
			if factoryCalls != expected {
				t.Fatalf("planned factories=%d wanted=%d", factoryCalls, expected)
			}
			var held, after runtime.MemStats
			runtime.GC()
			runtime.ReadMemStats(&held)
			runtime.KeepAlive(program)
			program = nil
			inputs = nil
			runtime.GC()
			runtime.ReadMemStats(&after)
			record := map[string]any{"mode": mode, "workload": workload, "factory_calls": factoryCalls, "setup_ns": setupNS, "measurements": rows, "heap_before": before.HeapAlloc, "heap_owner_held": held.HeapAlloc, "heap_after_drop": after.HeapAlloc}
			encoded, err := json.Marshal(record)
			if err != nil {
				t.Fatal(err)
			}
			fmt.Println("REGOCACHE_CEL", string(encoded))
		}
	}
	ast, issues := env.Compile(`enabled ? value.matches("[") : false`)
	if issues.Err() != nil {
		t.Fatal(issues.Err())
	}
	ordinary, err := env.Program(ast)
	if err != nil {
		t.Fatal(err)
	}
	value, _, err := ordinary.Eval(map[string]any{"enabled": false, "value": "x"})
	if err != nil || value != types.False {
		t.Fatalf("unexecuted invalid regexp: value=%v err=%v", value, err)
	}
	_, plannedError := env.Program(ast, cel.OptimizeRegex(interpreter.MatchesRegexOptimization))
	if plannedError == nil {
		t.Fatal("expected eager constant compilation failure")
	}
	encoded, _ := json.Marshal(map[string]any{"baseline_unexecuted_value": false, "planned_creation_error": plannedError.Error()})
	fmt.Println("REGOCACHE_CEL_ERROR_PHASE", string(encoded))
}
