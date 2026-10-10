package commands

// buildx's own answers to `du`, for shards-cmdline's tests to match
// (crates/cmdline/tests/buildx_du.rs). `generate` copies this file into buildx's commands
// package and runs it there: each case runs runDiskUsage itself, against a builder of the
// remote driver whose BuildKit is a fake one here, which answers DiskUsage with the case's
// records, as BuildKit would once it had filtered them. What it printed, its error, and
// the filters it asked BuildKit for are each case's answer.

import (
	"bytes"
	"context"
	"encoding/json"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	_ "github.com/docker/buildx/driver/remote"
	"github.com/docker/buildx/store"
	"github.com/docker/buildx/store/storeutil"
	"github.com/docker/cli/cli/command"
	cliflags "github.com/docker/cli/cli/flags"
	"github.com/docker/cli/opts"
	controlapi "github.com/moby/buildkit/api/services/control"
	"google.golang.org/grpc"
	"google.golang.org/protobuf/types/known/timestamppb"
)

// A record BuildKit's DiskUsage answers with.
type shardsDuRecord struct {
	ID          string   `json:"id"`
	Parents     []string `json:"parents"`
	CreatedAt   string   `json:"created_at"`
	Mutable     bool     `json:"mutable"`
	InUse       bool     `json:"in_use"`
	Shared      bool     `json:"shared"`
	Size        int64    `json:"size"`
	Description string   `json:"description"`
	UsageCount  int64    `json:"usage_count"`
	// Seconds before the case ran that it was last used, each far enough inside its unit of
	// go-units' HumanDuration that a case's setup does not change what it says; none where
	// it never was.
	LastUsedAgo *int64 `json:"last_used_ago"`
	Type        string `json:"type"`
}

func shardsAgo(s int64) *int64 { return &s }

var shardsDuRecords = []shardsDuRecord{
	{ID: "kx8kqtz5xgo2ixlw9els774wk", CreatedAt: "2026-10-10T10:50:52.283772793Z", Size: 12345678,
		Description: "mount / from exec /bin/sh -c echo hi", UsageCount: 1, LastUsedAgo: shardsAgo(90),
		Type: "regular", Parents: []string{"qe221zojgn5tbnabgk7hcggrj"}},
	{ID: "rx92qyq0sh89maoiyvvf44kbe", CreatedAt: "2026-10-10T10:51:00Z", Mutable: true, Size: 4096,
		Description: `cached mount /c from exec /bin/sh -c ls -A /c with id "//c"`, UsageCount: 3,
		LastUsedAgo: shardsAgo(3 * 60), Type: "exec.cachemount"},
	{ID: "v8cz0q2a8ectlvskuomzsvjfv", CreatedAt: "2026-10-09T23:59:59.5Z", Mutable: true, Size: 1048576,
		Description: "local source for context", UsageCount: 2, LastUsedAgo: shardsAgo(2 * 3600), Type: "source.local"},
	{ID: "ugszqghmzn7ce8t1eqc5pvim8", CreatedAt: "2026-09-01T00:00:00.000000001Z", Shared: true, Size: 5000000000,
		UsageCount: 7, LastUsedAgo: shardsAgo(3 * 86400), Type: "regular",
		Parents: []string{"w573g82hyxry17yity2rfiqxz", "yobxdjeelt5ayncm49zbj2b6i"}},
	{ID: "qit9yatvygfksd43cf1dtw5k5", CreatedAt: "2026-10-10T11:00:00Z", Mutable: true, InUse: true,
		Type: "internal"},
	{ID: "gglyeigvuwm9jfwnylxid6r5f", CreatedAt: "2026-01-02T03:04:05Z", Size: 999,
		Description: "pulled from docker.io/library/alpine:3.22", UsageCount: 12, LastUsedAgo: shardsAgo(45 * 86400),
		Type: "frontend"},
}

type shardsDuCase struct {
	Format  string   `json:"format"`
	Verbose bool     `json:"verbose"`
	Filters []string `json:"filters"`
	// The records BuildKit answers with, by their place in shardsDuRecords.
	Records []int `json:"records"`
	Stdout  string `json:"stdout"`
	// runDiskUsage's error, which main prints after "ERROR: ".
	Error string `json:"error"`
	// The filters buildx asked BuildKit for, each one, sorted: buildx joins them in a
	// map's order.
	Asked []string `json:"asked"`
}

var shardsAll = []int{0, 1, 2, 3, 4, 5}

var shardsDuCases = []shardsDuCase{
	{Records: shardsAll},
	{Verbose: true, Records: shardsAll},
	{Format: "table", Records: shardsAll},
	{Format: "pretty", Records: shardsAll},
	{Format: "json", Records: shardsAll},
	{Format: "raw", Records: []int{0}},
	{Format: "{{.ID}} {{.Type}} {{.Mutable}} {{.Reclaimable}} {{.Shared}}", Records: shardsAll},
	{Format: "table {{.ID}}\t{{.Parents}}\t{{.Size}}\t{{.Shared}}", Records: shardsAll},
	{Format: "table {{.CreatedAt}}\t{{.UsageCount}}\t{{.LastUsedAt}}\t{{.Description}}", Records: shardsAll},
	{Format: "{{json .}}", Records: []int{0, 4}},
	{Format: "{{.Parents}}|{{len .Parents}}", Records: []int{0, 1, 3}},
	{Format: "  table {{.ID}}  ", Records: []int{1}},
	{Format: `{{.ID}}\t{{.Size}}\n`, Records: []int{0, 1}},
	{Filters: []string{"type=regular"}, Records: []int{0, 3}},
	{Verbose: true, Filters: []string{"until=24h"}, Records: shardsAll},
	{Filters: []string{"id=kx8k", "description~=mount", "mutable=true"}, Records: []int{1}},
	{Format: "pretty", Filters: []string{"type=exec.cachemount"}, Records: []int{1}},
	{Records: []int{}},
	{Verbose: true, Records: []int{}},
	{Format: "json", Records: []int{}},
	{Records: []int{1, 2, 4}},
	{Format: "json", Verbose: true, Records: shardsAll},
	{Format: "pretty", Verbose: true, Records: shardsAll},
	{Filters: []string{"until=bogus"}, Records: shardsAll},
	{Filters: []string{"until=1h", "unused-for=2h"}, Records: shardsAll},
	{Filters: []string{"unused-for=1h", "unused-for=2h"}, Records: shardsAll},
	{Filters: []string{"type=regular", "type=source.local"}, Records: shardsAll},
	{Format: "{{.Bogus}}", Records: []int{0}},
	{Format: "table {{.Bogus}}", Records: []int{0}},
	{Format: "{{.ID", Records: []int{0}},
}

// shardsFakeControl is BuildKit's Control service, all but DiskUsage left unimplemented.
type shardsFakeControl struct {
	controlapi.UnimplementedControlServer
	records []*controlapi.UsageRecord
	asked   []string
}

func (f *shardsFakeControl) DiskUsage(_ context.Context, r *controlapi.DiskUsageRequest) (*controlapi.DiskUsageResponse, error) {
	f.asked = r.Filter
	return &controlapi.DiskUsageResponse{Record: f.records}, nil
}

func TestShardsDuOracle(t *testing.T) {
	out := os.Getenv("SHARDS_DU_ORACLE_OUT")
	if out == "" {
		t.Skip("SHARDS_DU_ORACLE_OUT names the file to write")
	}
	os.Setenv("DOCKER_CONFIG", t.TempDir())
	// A short path: a unix socket's must fit sockaddr_un.
	dir, err := os.MkdirTemp("/tmp", "sdu")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(dir)
	sock := filepath.Join(dir, "b.sock")
	l, err := net.Listen("unix", sock)
	if err != nil {
		t.Fatal(err)
	}
	fake := &shardsFakeControl{}
	srv := grpc.NewServer()
	controlapi.RegisterControlServer(srv, fake)
	go srv.Serve(l)
	defer srv.Stop()

	var stdout, stderr bytes.Buffer
	dockerCli, err := command.NewDockerCli(command.WithOutputStream(&stdout), command.WithErrorStream(&stderr))
	if err != nil {
		t.Fatal(err)
	}
	if err := dockerCli.Initialize(cliflags.NewClientOptions()); err != nil {
		t.Fatal(err)
	}
	txn, release, err := storeutil.GetStore(dockerCli)
	if err != nil {
		t.Fatal(err)
	}
	ng := &store.NodeGroup{Name: "shards-oracle", Driver: "remote", Nodes: []store.Node{{Name: "shards-oracle0", Endpoint: "unix://" + sock}}}
	if err := txn.Save(ng); err != nil {
		t.Fatal(err)
	}
	release()

	answers := make([]shardsDuCase, 0, len(shardsDuCases))
	for _, c := range shardsDuCases {
		now := time.Now()
		fake.records, fake.asked = nil, nil
		for _, i := range c.Records {
			r := shardsDuRecords[i]
			created, err := time.Parse(time.RFC3339Nano, r.CreatedAt)
			if err != nil {
				t.Fatal(err)
			}
			rec := &controlapi.UsageRecord{
				ID: r.ID, Mutable: r.Mutable, InUse: r.InUse, Size: r.Size, Parents: r.Parents,
				CreatedAt: timestamppb.New(created), UsageCount: r.UsageCount,
				Description: r.Description, RecordType: r.Type, Shared: r.Shared,
			}
			if r.LastUsedAgo != nil {
				rec.LastUsedAt = timestamppb.New(now.Add(-time.Duration(*r.LastUsedAgo) * time.Second))
			}
			fake.records = append(fake.records, rec)
		}
		filter := opts.NewFilterOpt()
		for _, f := range c.Filters {
			if err := filter.Set(f); err != nil {
				t.Fatal(err)
			}
		}
		stdout.Reset()
		stderr.Reset()
		err := runDiskUsage(context.Background(), dockerCli, duOptions{
			builder: "shards-oracle", filter: filter, verbose: c.Verbose, format: c.Format, timeout: 20 * time.Second,
		})
		a := c
		a.Stdout = stdout.String()
		if err != nil {
			a.Error = err.Error()
		}
		for _, f := range fake.asked {
			for _, one := range strings.Split(f, ",") {
				if one != "" {
					a.Asked = append(a.Asked, one)
				}
			}
		}
		sort.Strings(a.Asked)
		if stderr.Len() > 0 {
			t.Fatalf("%v: stderr %q", c, stderr.String())
		}
		answers = append(answers, a)
	}
	data, err := json.MarshalIndent(map[string]any{"records": shardsDuRecords, "cases": answers}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
