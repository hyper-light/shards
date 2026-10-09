// What CDI v1.1.0's ParseSpec (BuildKit v0.28.1's), through sigs.k8s.io/yaml v1.4.0 and
// its go-yaml v2, makes of each spec text of testdata/cdi-specs.json: the spec as JSON, or
// its error (D96). crates/shards/src/build/cdi.rs's tests read each again.
package main

import (
	"encoding/json"
	"os"

	"tags.cncf.io/container-device-interface/pkg/cdi"
)

func main() {
	in, err := os.ReadFile(os.Args[1])
	if err != nil {
		panic(err)
	}
	var texts []string
	if err := json.Unmarshal(in, &texts); err != nil {
		panic(err)
	}
	var out []map[string]any
	for _, t := range texts {
		spec, err := cdi.ParseSpec([]byte(t))
		if err != nil {
			out = append(out, map[string]any{"error": err.Error()})
			continue
		}
		out = append(out, map[string]any{"spec": spec})
	}
	b, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[2], append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}
