package topdown_test

// Test-only pinned OPA comparison through its actual prepared Rego evaluation API.
import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"runtime"
	"syscall"
	"testing"
	"time"

	"github.com/open-policy-agent/opa/v1/rego"
	"github.com/open-policy-agent/opa/v1/topdown"
)

type shardsPerfWorkload struct {
	Name            string           `json:"name"`
	Source          string           `json:"source"`
	Inputs          []map[string]any `json:"inputs"`
	Expected        any              `json:"expected"`
	ExpectedByInput []bool           `json:"expected_by_input"`
	Comparison      string           `json:"comparison"`
}
type shardsPerfSuite struct {
	Samples    int                  `json:"samples"`
	Iterations int                  `json:"iterations"`
	Workloads  []shardsPerfWorkload `json:"workloads"`
}

func shardsPerfEmit(t *testing.T, prefix string, value any) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	fmt.Printf("%s %s\n", prefix, data)
}
func shardsPerfAllocation(before, after runtime.MemStats) map[string]any {
	return map[string]any{"allocated_bytes": after.TotalAlloc - before.TotalAlloc, "allocations": after.Mallocs - before.Mallocs, "freed_objects": after.Frees - before.Frees, "retained_delta": int64(after.HeapAlloc) - int64(before.HeapAlloc)}
}
func shardsPerfSnapshot() runtime.MemStats {
	var stats runtime.MemStats
	runtime.ReadMemStats(&stats)
	return stats
}
func shardsPerfUsage(t *testing.T) syscall.Rusage {
	t.Helper()
	var stats syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &stats); err != nil {
		t.Fatal(err)
	}
	return stats
}
func shardsPerfUsageDelta(before, after syscall.Rusage) map[string]any {
	cpu := func(r syscall.Rusage) int64 { return (r.Utime.Sec+r.Stime.Sec)*1_000_000 + r.Utime.Usec + r.Stime.Usec }
	return map[string]any{"process_cpu_us": cpu(after) - cpu(before), "minor_faults": after.Minflt - before.Minflt, "major_faults": after.Majflt - before.Majflt, "voluntary_switches": after.Nvcsw - before.Nvcsw, "involuntary_switches": after.Nivcsw - before.Nivcsw}
}
func TestShardsCachePerformance(t *testing.T) {
	data, err := os.ReadFile(os.Getenv("SHARDS_REGO_EXPERIMENT_INPUT"))
	if err != nil {
		t.Fatal(err)
	}
	var suite shardsPerfSuite
	if err = json.Unmarshal(data, &suite); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	for _, workload := range suite.Workloads {
		if selected := os.Getenv("SHARDS_REGO_EXPERIMENT_WORKLOAD"); selected != "" && selected != workload.Name {
			continue
		}
		runtime.GC()
		before := shardsPerfSnapshot()
		began := time.Now()
		var builtinErrors []topdown.Error
		prepared, err := rego.New(rego.Query("data.bench.decision"), rego.Module("probe.rego", workload.Source), rego.BuiltinErrorList(&builtinErrors)).PrepareForEval(ctx)
		if err != nil {
			t.Fatal(err)
		}
		setupNS := time.Since(began).Nanoseconds()
		afterSetup := shardsPerfSnapshot()
		beforeFirst := shardsPerfSnapshot()
		began = time.Now()
		first, err := prepared.Eval(ctx, rego.EvalInput(workload.Inputs[0]))
		if err != nil {
			t.Fatal(err)
		}
		firstNS := time.Since(began).Nanoseconds()
		afterFirst := shardsPerfSnapshot()
		var firstValue any
		if len(first) > 0 {
			firstValue = first[0].Expressions[0].Value
		}
		if workload.Comparison == "equivalent" && !reflect.DeepEqual(firstValue, workload.Expected) {
			t.Fatalf("first %s: %v", workload.Name, firstValue)
		}
		firstErrors := make([]string, 0, len(builtinErrors))
		for _, failure := range builtinErrors {
			firstErrors = append(firstErrors, failure.Error())
		}
		measures := make([]map[string]any, 0, suite.Samples)
		mismatches, errorCount := 0, 0
		firstError := ""
		for sample := range suite.Samples {
			beforeSample := shardsPerfSnapshot()
			usageBefore := shardsPerfUsage(t)
			began = time.Now()
			for i := range suite.Iterations {
				index := sample*suite.Iterations + i
				builtinErrors = builtinErrors[:0]
				values, err := prepared.Eval(ctx, rego.EvalInput(workload.Inputs[index]))
				if err != nil {
					t.Fatal(err)
				}
				var value any
				if len(values) > 0 {
					value = values[0].Expressions[0].Value
				}
				if workload.Comparison == "equivalent" && (!reflect.DeepEqual(value, workload.Expected) || len(builtinErrors) > 0) {
					t.Fatalf("%s: %v / %v", workload.Name, value, builtinErrors)
				}
				if workload.Comparison == "upstream_history_deviation" && value != workload.ExpectedByInput[index] {
					mismatches++
				}
				errorCount += len(builtinErrors)
				if len(builtinErrors) > 0 && firstError == "" {
					firstError = builtinErrors[0].Error()
				}
			}
			elapsed := time.Since(began).Nanoseconds()
			usageAfter := shardsPerfUsage(t)
			afterSample := shardsPerfSnapshot()
			measures = append(measures, map[string]any{"sample": sample, "elapsed_ns": elapsed, "iterations": suite.Iterations, "allocation": shardsPerfAllocation(beforeSample, afterSample), "usage": shardsPerfUsageDelta(usageBefore, usageAfter)})
		}
		beforeDrop := shardsPerfSnapshot()
		began = time.Now()
		runtime.KeepAlive(prepared)
		prepared = rego.PreparedEvalQuery{}
		runtime.GC()
		dropNS := time.Since(began).Nanoseconds()
		afterDrop := shardsPerfSnapshot()
		shardsPerfEmit(t, "REGOCACHE_EXPERIMENT", map[string]any{"implementation": "opa-v1.14.1", "mode": "opa", "workload": workload.Name, "comparison": workload.Comparison, "setup_ns": setupNS, "setup_allocation": shardsPerfAllocation(before, afterSetup), "first_ns": firstNS, "first_allocation": shardsPerfAllocation(beforeFirst, afterFirst), "first_value": firstValue, "first_errors": firstErrors, "measurements": measures, "semantic_mismatches": mismatches, "error_count": errorCount, "first_error": firstError, "drop_ns": dropNS, "drop_allocation": shardsPerfAllocation(beforeDrop, afterDrop), "drop_method": "last prepared reference removed followed by runtime.GC; global regex cache remains"})
	}
}
func TestShardsCacheMixedPolicy(t *testing.T) {
	source := "package bench\nordinary := regex.match(\"a.b\",input.value)\ntemplate := regex.template_match(\"a.b\",input.value,\"<\",\">\")\nangle := regex.template_match(\"a<b>\",input.value,\"<\",\">\")\nsquare := regex.template_match(\"a<b>\",input.value,\"[\",\"]\")\n"
	scenarios := map[string][]string{"ordinary_template": {"ordinary", "template", "axb"}, "template_ordinary": {"template", "ordinary", "axb"}, "angle_square": {"angle", "square", "ab"}, "square_angle": {"square", "angle", "ab"}}
	selected := os.Getenv("SHARDS_REGO_EXPERIMENT_MIXED")
	scenario, ok := scenarios[selected]
	if !ok {
		t.Fatal("choose exactly one fresh-process scenario")
	}
	ctx := context.Background()
	first, err := rego.New(rego.Query("data.bench."+scenario[0]), rego.Module("probe.rego", source)).PrepareForEval(ctx)
	if err != nil {
		t.Fatal(err)
	}
	second, err := rego.New(rego.Query("data.bench."+scenario[1]), rego.Module("probe.rego", source)).PrepareForEval(ctx)
	if err != nil {
		t.Fatal(err)
	}
	input := map[string]any{"value": scenario[2]}
	a, err := first.Eval(ctx, rego.EvalInput(input))
	if err != nil {
		t.Fatal(err)
	}
	b, err := second.Eval(ctx, rego.EvalInput(input))
	if err != nil {
		t.Fatal(err)
	}
	shardsPerfEmit(t, "REGOCACHE_OPA_MIXED_POLICY", map[string]any{"first": scenario[0], "second": scenario[1], "values": []any{a[0].Expressions[0].Value, b[0].Expressions[0].Value}})
}

func TestShardsCacheErrorOrder(t *testing.T) {
	source := "package bench\ndecision := [regex.match(\"[\",input.value), regex.template_match(\"a<\",input.value,\"<\",\">\")]\nsecond := regex.template_match(\"a<\",input.value,\"<\",\">\")\n"
	var ordered []string
	for _, query := range []string{"data.bench.decision", "data.bench.second"} {
		var failures []topdown.Error
		prepared, err := rego.New(rego.Query(query), rego.Module("probe.rego", source), rego.BuiltinErrorList(&failures)).PrepareForEval(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		values, err := prepared.Eval(context.Background(), rego.EvalInput(map[string]any{"value": "x"}))
		if err != nil || len(values) != 0 || len(failures) != 1 {
			t.Fatalf("%s: %v / %v / %v", query, values, failures, err)
		}
		ordered = append(ordered, failures[0].Error())
	}
	shardsPerfEmit(t, "REGOCACHE_OPA_ERROR_ORDER", map[string]any{"errors": ordered})
}
