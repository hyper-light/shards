package image

// docker/cli's own `history` output for shards-cmdline's format module, completing what
// container_test.go wrote to SHARDS_FORMAT_PART: `generate` copies this file into
// docker/cli's cli/command/image and runs it there, so that newHistoryFormat and
// historyWrite write the layers below for each format of the corpus, at the moment and in
// the zone container_test.go's cases run at. It writes everything to SHARDS_FORMAT_OUT.

import (
	"bytes"
	"encoding/json"
	"os"
	"strconv"
	"testing"
	"testing/synctest"
	"time"

	"github.com/docker/cli/cli/command/formatter"
	"github.com/moby/moby/api/types/image"
	"github.com/moby/moby/client"
)

type shardsLayer struct {
	ID        string `json:"id"`
	Created   int64  `json:"created"`
	CreatedBy string `json:"created_by"`
	Size      int64  `json:"size"`
	Comment   string `json:"comment"`
}

type shardsHistoryCase struct {
	Command string `json:"command"`
	Set     string `json:"set"`
	Format  string `json:"format"`
	Quiet   bool   `json:"quiet,omitempty"`
	NoTrunc bool   `json:"no_trunc,omitempty"`
	Human   bool   `json:"human,omitempty"`
	Output  string `json:"output"`
	Error   string `json:"error,omitempty"`
}

type shardsZone struct {
	Offset int    `json:"offset"`
	Name   string `json:"name"`
}

const shardsNow = 1749988800

var shardsLayers = []shardsLayer{
	{"sha256:5291449c3df7a8c0bd0a9e2aeb58fa40d1e1ba8cc4b6c01f12a4f8ec6b3e9b6d", shardsNow - 3600*30, "CMD [\"/bin/sh\"]", 0, ""},
	{"<missing>", shardsNow - 3600*30 - 1, "ADD alpine-minirootfs-3.22.0-aarch64.tar.gz / # buildkit", 8_421_000, "buildkit.dockerfile.v0"},
	{"<missing>", 946684800, "/bin/sh -c #(nop) \tLABEL\tx=y", 0, ""},
	{"<missing>", 946684801, "RUN /bin/sh -c apk add --no-cache 日本語の文字列です和 && echo done # buildkit", 1_290_000_000, "<comment> & more"},
	{"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 0, "", -1, "imported from -"},
	{"sha256:aa", shardsNow + 50, "COPY . /app # buildkit", 999_999, ""},
}

var shardsHistoryFormats = []string{
	"", "table", "json", "raw", "{{json .}}", "table {{json .}}", "{{.ID}}", "table {{.ID}}",
	"{{.Missing}}", "table {{.Missing}}", "{{.ID", "table {{.ID}}\\t{{.Missing.X}}",
	"table {{.FullHeader}}\\t{{.ID}}", "  table {{.ID}}  ", "table {{upper .ID}}\\t{{json .CreatedBy}}",
	"table {{.ID}}\\t{{.CreatedAt}}\\t{{.CreatedSince}}", "{{.CreatedBy}}|{{.Size}}|{{.Comment}}",
	"table {{.CreatedBy}}\\t{{.Size}}", "{{json .Comment}}", `{{.Label "x"}}`,
}

func shardsHistory(c *shardsHistoryCase, items []image.HistoryResponseItem) {
	var buf bytes.Buffer
	format := c.Format
	if format == "" {
		format = formatter.TableFormatKey
	}
	ctx := formatter.Context{Output: &buf, Format: newHistoryFormat(format, c.Quiet, c.Human), Trunc: !c.NoTrunc}
	if err := historyWrite(ctx, c.Human, client.ImageHistoryResult{Items: items}); err != nil {
		c.Error = err.Error()
	}
	c.Output = buf.String()
}

func TestShardsFormatHistory(t *testing.T) {
	part, out := os.Getenv("SHARDS_FORMAT_PART"), os.Getenv("SHARDS_FORMAT_OUT")
	if part == "" || out == "" {
		t.Skip("SHARDS_FORMAT_PART names the file container_test.go wrote, SHARDS_FORMAT_OUT the one to write")
	}
	data, err := os.ReadFile(part)
	if err != nil {
		t.Fatal(err)
	}
	var oracle map[string]json.RawMessage
	if err := json.Unmarshal(data, &oracle); err != nil {
		t.Fatal(err)
	}
	var cases []json.RawMessage
	var zones map[string]shardsZone
	if err := json.Unmarshal(oracle["cases"], &cases); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(oracle["zones"], &zones); err != nil {
		t.Fatal(err)
	}
	var items []image.HistoryResponseItem
	for _, l := range shardsLayers {
		items = append(items, image.HistoryResponseItem{ID: l.ID, Created: l.Created, CreatedBy: l.CreatedBy, Size: l.Size, Comment: l.Comment})
		name, offset := time.Unix(l.Created, 0).Zone()
		zones[strconv.FormatInt(l.Created, 10)] = shardsZone{offset, name}
	}
	for _, set := range []string{"all", "empty"} {
		for _, f := range shardsHistoryFormats {
			for _, flags := range [][3]bool{{false, false, true}, {true, false, true}, {false, true, true}, {false, false, false}} {
				c := shardsHistoryCase{Command: "history", Set: set, Format: f, Quiet: flags[0], NoTrunc: flags[1], Human: flags[2]}
				synctest.Test(t, func(t *testing.T) {
					time.Sleep(time.Unix(shardsNow, 0).Sub(time.Now()))
					if set == "all" {
						shardsHistory(&c, items)
					} else {
						shardsHistory(&c, nil)
					}
				})
				raw, err := json.Marshal(c)
				if err != nil {
					t.Fatal(err)
				}
				cases = append(cases, raw)
			}
		}
	}
	for k, v := range map[string]any{"cases": cases, "zones": zones, "history": shardsLayers} {
		if oracle[k], err = json.Marshal(v); err != nil {
			t.Fatal(err)
		}
	}
	data, err = json.MarshalIndent(oracle, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
