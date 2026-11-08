// Copied into opencontainers/cgroups v0.0.4's devices package by generate: runc's own
// deviceFilter run on each case's rules, the program as cilium/ebpf marshals it for
// BPF_PROG_LOAD (its jumps resolved), or its error.
package devices

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"os"
	"testing"

	devices "github.com/opencontainers/cgroups/devices/config"
)

type rule struct {
	Type  string `json:"type"`
	Major int64  `json:"major"`
	Minor int64  `json:"minor"`
	Perms string `json:"perms"`
	Allow bool   `json:"allow"`
}

type answer struct {
	Name    string `json:"name"`
	Program string `json:"program,omitempty"`
	Error   string `json:"error,omitempty"`
}

func TestShardsOracle(t *testing.T) {
	in, err := os.ReadFile(os.Getenv("CASES"))
	if err != nil {
		t.Fatal(err)
	}
	var cases []struct {
		Name  string `json:"name"`
		Rules []rule `json:"rules"`
	}
	if err := json.Unmarshal(in, &cases); err != nil {
		t.Fatal(err)
	}
	var out []answer
	for _, c := range cases {
		var rules []*devices.Rule
		for _, r := range c.Rules {
			rules = append(rules, &devices.Rule{
				Type:        devices.Type(r.Type[0]),
				Major:       r.Major,
				Minor:       r.Minor,
				Permissions: devices.Permissions(r.Perms),
				Allow:       r.Allow,
			})
		}
		a := answer{Name: c.Name}
		insts, _, err := deviceFilter(rules)
		if err == nil {
			var b bytes.Buffer
			err = insts.Marshal(&b, binary.LittleEndian)
			a.Program = hex.EncodeToString(b.Bytes())
		}
		if err != nil {
			a.Program = ""
			a.Error = err.Error()
		}
		out = append(out, a)
	}
	j, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("OUT"), append(j, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
