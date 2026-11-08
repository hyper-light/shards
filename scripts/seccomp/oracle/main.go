// The seccomp oracle of scripts/seccomp/generate: what dockerd docker-v29.8.1 (moby
// profiles/seccomp v0.2.3) and runc v1.3.4 with libseccomp 2.5.4 make of each case, on
// this architecture.
//
//	oracle ARCH SYSCALLS_CSV CASES_JSON ORACLE_OUT SYSCALLS_OUT
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/moby/profiles/seccomp"
	"github.com/opencontainers/runc/libcontainer/configs"
	runcseccomp "github.com/opencontainers/runc/libcontainer/seccomp"
	"github.com/opencontainers/runc/libcontainer/seccomp/patchbpf"
	"github.com/opencontainers/runc/libcontainer/specconv"
	"github.com/opencontainers/runtime-spec/specs-go"
	libseccomp "github.com/seccomp/libseccomp-golang"
)

// A case: a profile (Docker's default where none is given) for a container whose
// bounding set is Caps.
type Case struct {
	Name    string          `json:"name"`
	Caps    []string        `json:"caps"`
	Profile json.RawMessage `json:"profile,omitempty"`
}

type Result struct {
	Name       string              `json:"name"`
	Error      string              `json:"error,omitempty"`
	Resolved   *specs.LinuxSeccomp `json:"resolved,omitempty"`
	Program    [][4]uint32         `json:"program,omitempty"`
	Flags      uint                `json:"flags"`
	NoNewPrivs bool                `json:"noNewPrivs"`
}

func run(c Case) Result {
	r := Result{Name: c.Name}
	spec := &specs.Spec{Process: &specs.Process{Capabilities: &specs.LinuxCapabilities{Bounding: c.Caps}}}
	var resolved *specs.LinuxSeccomp
	var err error
	if len(c.Profile) == 0 {
		resolved, err = seccomp.GetDefaultProfile(spec)
	} else {
		resolved, err = seccomp.LoadProfile(string(c.Profile), spec)
	}
	if err != nil {
		r.Error = err.Error()
		return r
	}
	r.Resolved = resolved
	if resolved == nil {
		return r
	}
	var cfg *configs.Seccomp
	if cfg, err = specconv.SetupSeccomp(resolved); err != nil {
		r.Error = err.Error()
		return r
	}
	filter, err := runcseccomp.BuildFilter(cfg)
	if err != nil {
		r.Error = err.Error()
		return r
	}
	defer filter.Release()
	prog, flags, nnp, err := patchbpf.Export(cfg, filter)
	if err != nil {
		r.Error = err.Error()
		return r
	}
	for _, i := range prog {
		r.Program = append(r.Program, [4]uint32{uint32(i.Code), uint32(i.Jt), uint32(i.Jf), i.K})
	}
	r.Flags, r.NoNewPrivs = flags, nnp
	return r
}

func must(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func main() {
	arch, csvPath, casesPath, oracleOut, syscallsOut := os.Args[1], os.Args[2], os.Args[3], os.Args[4], os.Args[5]
	var cases []Case
	b, err := os.ReadFile(casesPath)
	must(err)
	must(json.Unmarshal(b, &cases))
	results := []Result{}
	for _, c := range cases {
		results = append(results, run(c))
	}
	out, err := json.MarshalIndent(map[string]any{"arch": arch, "cases": results}, "", " ")
	must(err)
	must(os.WriteFile(oracleOut, append(out, '\n'), 0o644))

	// libseccomp's own tables, by which runc reads a profile's names: each name of
	// syscalls.csv on each of this architecture's ABIs that libseccomp resolves there.
	arches := map[string][]libseccomp.ScmpArch{
		"amd64": {libseccomp.ArchAMD64, libseccomp.ArchX86, libseccomp.ArchX32},
		"arm64": {libseccomp.ArchARM64, libseccomp.ArchARM},
	}[arch]
	f, err := os.Open(csvPath)
	must(err)
	var names []string
	s := bufio.NewScanner(f)
	for s.Scan() {
		line := s.Text()
		if strings.HasPrefix(line, "#") || line == "" {
			continue
		}
		names = append(names, strings.SplitN(line, ",", 2)[0])
	}
	sort.Strings(names)
	tables := map[string]map[string]int32{}
	for _, a := range arches {
		t := map[string]int32{}
		for _, n := range names {
			// A negative number is libseccomp's pseudo-syscall for one this ABI
			// lacks (__PNR_*), which it still takes rules for.
			if nr, err := libseccomp.GetSyscallFromNameByArch(n, a); err == nil {
				t[n] = int32(nr)
			}
		}
		tables[a.String()] = t
	}
	out, err = json.MarshalIndent(tables, "", " ")
	must(err)
	must(os.WriteFile(syscallsOut, append(out, '\n'), 0o644))
}
