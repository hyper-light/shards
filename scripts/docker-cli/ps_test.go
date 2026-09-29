package formatter

// The Docker CLI's own `ps` tables, for shards' to match (crates/shards/src/daemon/
// commands.rs, docker-ps.json). `generate` copies this file into docker/cli's
// cli/command/formatter and runs it there: the CLI's formatter writes tables of the
// containers below as `docker ps` would, with and without --no-trunc, and in and out of
// an East Asian locale; and go-units' durations for the times it prints.

import (
	"bytes"
	"encoding/json"
	"os"
	"testing"
	"time"

	units "github.com/docker/go-units"
	"github.com/mattn/go-runewidth"
	"github.com/moby/moby/api/types/container"
)

type listed struct {
	ID      string `json:"id"`
	Image   string `json:"image"`
	Command string `json:"command"`
	// Seconds from its creation to the listing.
	Ago    int64  `json:"ago"`
	Status string `json:"status"`
	Name   string `json:"name"`
}

type table struct {
	Containers []listed `json:"containers"`
	Trunc      bool     `json:"trunc"`
	Quiet      bool     `json:"quiet"`
	EastAsian  bool     `json:"east_asian"`
	Output     string   `json:"output"`
}

type duration struct {
	Seconds int64  `json:"seconds"`
	Text    string `json:"text"`
}

var containers = []listed{
	{"4bed76d3ad428b889c56c1ecc2bf2ed95cb08256db22dc5ef5863e1d03252a19", "nginx:alpine", "/docker-entrypoint.sh nginx -g 'daemon off;'", 1, "Up Less than a second", "test"},
	{"b0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000001", "docker.io/library/busybox:latest", "sh", 4, "Exited (0) 3 seconds ago", "ecstatic_beaver"},
	{"c0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000002", "alpine@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1", "echo 12345678901234567890", 7200, "Created", "x"},
	{"d0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000003", "localhost:5000/team/app:v1.2@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1", "printf 'a\tb\nc'", 120, "Exited (137) About a minute ago", "tabs"},
	{"e0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000004", "ghcr.io/org/image", "echo 日本語の文字列です", 3600, "Up About an hour", "cjk"},
	{"f0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000005", "docker.io/team/tool:1", "echo é   ‮ x", 90000, "Up 25 hours", "marks"},
	{"a1318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000006", "busybox", "echo …… ambiguous width", 172800, "Exited (1) 2 days ago", "ambiguous"},
	{"a2318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000007", "busybox:1.36", "sh -c 'echo \"quoted\" \\ back'", 1209600, "Exited (2) 2 weeks ago", "quotes"},
	{"a3318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000008", "busybox", "echo 😀 done\x01\x7f", 5184000, "Exited (0) 8 weeks ago", "a-rather-long-container-name_1"},
}

// The durations `docker ps` prints, around each of go-units' boundaries.
var seconds = []int64{
	0, 1, 2, 59, 60, 119, 120, 3599, 3600, 5399, 5400, 47*3600 + 1799, 47*3600 + 1800,
	48 * 3600, 13*24*3600 + 23*3600, 14 * 24 * 3600, 59 * 24 * 3600, 60 * 24 * 3600,
	729 * 24 * 3600, 730 * 24 * 3600, 17519 * 3600, 17520 * 3600,
}

func TestShardsPs(t *testing.T) {
	out := os.Getenv("SHARDS_PS_OUT")
	if out == "" {
		t.Skip("SHARDS_PS_OUT names the file to write")
	}
	var tables []table
	for _, eastAsian := range []bool{false, true} {
		runewidth.DefaultCondition.EastAsianWidth = eastAsian
		for _, trunc := range []bool{true, false} {
			for _, quiet := range []bool{false, true} {
				for _, list := range [][]listed{containers, nil} {
					now := time.Now().Unix()
					var summaries []container.Summary
					for _, c := range list {
						summaries = append(summaries, container.Summary{
							ID:      c.ID,
							Names:   []string{"/" + c.Name},
							Image:   c.Image,
							Command: c.Command,
							Created: now - c.Ago,
							Status:  c.Status,
						})
					}
					var buf bytes.Buffer
					ctx := Context{Output: &buf, Format: NewContainerFormat("table", quiet, false), Trunc: trunc}
					if err := ContainerWrite(ctx, summaries); err != nil {
						t.Fatal(err)
					}
					tables = append(tables, table{list, trunc, quiet, eastAsian, buf.String()})
				}
			}
		}
	}
	runewidth.DefaultCondition.EastAsianWidth = false
	var durations []duration
	for _, s := range seconds {
		durations = append(durations, duration{s, units.HumanDuration(time.Duration(s) * time.Second)})
	}
	data, err := json.MarshalIndent(map[string]any{"tables": tables, "durations": durations}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
