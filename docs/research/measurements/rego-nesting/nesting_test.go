package oracle

// M160: OPA v1.14.1's time and peak memory on a shape of crates/rego/benches/nesting.json
// at a depth, run as buildx runs a policy check (oracle_test.go's run), NESTING_N times,
// whatever the outcome. A warm-up run comes first only where more than one run is asked:
// a deep shape takes OPA minutes a run.

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"
)

// nestingPolicy is crates/rego/benches/nesting.rs's policy: Open depth times around
// Inner, closed by as many Close, between Head and Tail; or Item for i from 1 to depth
// ({i} the index, {j} the one before), {n} in Tail the depth.
func nestingPolicy(s map[string]string, depth int) string {
	var b strings.Builder
	b.WriteString(s["head"])
	if item, ok := s["item"]; ok {
		for i := 1; i <= depth; i++ {
			x := strings.ReplaceAll(item, "{i}", strconv.Itoa(i))
			b.WriteString(strings.ReplaceAll(x, "{j}", strconv.Itoa(i-1)))
		}
	} else {
		b.WriteString(strings.Repeat(s["open"], depth))
		b.WriteString(s["inner"])
		b.WriteString(strings.Repeat(s["close"], depth))
	}
	b.WriteString(strings.ReplaceAll(s["tail"], "{n}", strconv.Itoa(depth)))
	return b.String()
}

func TestNesting(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("NESTING_SHAPES"))
	if err != nil {
		t.Skip("NESTING_SHAPES names crates/rego/benches/nesting.json")
	}
	var shapes []map[string]string
	if err := json.Unmarshal(dt, &shapes); err != nil {
		t.Fatal(err)
	}
	name := os.Getenv("NESTING_SHAPE")
	depth, err := strconv.Atoi(os.Getenv("NESTING_DEPTH"))
	if err != nil {
		t.Fatal(err)
	}
	n, err := strconv.Atoi(os.Getenv("NESTING_N"))
	if err != nil || n < 1 {
		n = 1
	}
	var shape map[string]string
	for _, s := range shapes {
		if s["name"] == name {
			shape = s
		}
	}
	if shape == nil {
		t.Fatalf("no shape %q", name)
	}
	c := Case{Name: name, Modules: [][2]string{{"policy.rego", nestingPolicy(shape, depth)}}, Query: "data.docker.decision"}
	if n > 1 {
		run(c)
	}
	var ds []time.Duration
	var r Result
	for range n {
		start := time.Now()
		r = run(c)
		ds = append(ds, time.Since(start))
	}
	outcome := "error " + r.Error
	if r.Error == "" {
		rs, _ := json.Marshal(r.Results)
		outcome = "results " + string(rs)
	}
	if len(outcome) > 300 {
		outcome = fmt.Sprintf("%s... (%d bytes)", outcome[:300], len(outcome))
	}
	line, _ := json.Marshal(map[string]any{
		"bench":        "opa-nesting",
		"case":         fmt.Sprintf("%s-%d", name, depth),
		"whole":        summarize(ds),
		"outcome":      outcome,
		"peak_rss_mib": peakRSS() >> 20,
	})
	fmt.Println(string(line))
}
