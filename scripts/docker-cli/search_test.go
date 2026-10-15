package registry

// The Docker CLI's own `search` output, for shards' to match (crates/cmdline/src/format/
// search.rs, tests/docker-search.json). `generate` copies this file into docker/cli's
// cli/command/registry and runs it there: the CLI's formatter writes the results below as
// `docker search` would, in its table and others, with and without --no-trunc, as JSON,
// and through templates that read a result before and after marshalling it.

import (
	"bytes"
	"encoding/json"
	"os"
	"testing"

	"github.com/docker/cli/cli/command/formatter"
	registrytypes "github.com/moby/moby/api/types/registry"
	"github.com/moby/moby/client"
)

type shardsFound struct {
	Name        string `json:"name"`
	Description string `json:"description"`
	Stars       int    `json:"star_count"`
	Official    bool   `json:"is_official"`
}

type shardsSearch struct {
	Results []shardsFound `json:"results"`
	Format  string        `json:"format"`
	Trunc   bool          `json:"trunc"`
	Output  string        `json:"output"`
	Error   string        `json:"error,omitempty"`
}

var shardsResults = [][]shardsFound{
	{
		{"alpine", "A minimal Docker image based on Alpine Linux with a complete package index and only 5 MB in size!", 11563, true},
		{"alpine/git", "A  simple git container running in alpine linux, especially for tiny linux distro.", 252, false},
		{"team/日本語", "日本語の説明は四十五列を越えると切られます。日本語の説明は四十五列を越えると切られます。", 0, false},
		{"lines", "first line\nsecond line\r\nthird", 3, false},
		{"marks", "<b>&amp;</b> \"quoted\" 'single' \t tab", 1, false},
		{"exactly45", "123456789012345678901234567890123456789012345", 0, true},
		{"exactly46", "1234567890123456789012345678901234567890123456", 0, false},
		{"short", "", 0, true},
	},
	{},
}

var shardsFormats = []string{
	"",
	"table",
	"json",
	"raw",
	"{{.Name}}",
	"table {{.Name}}\t{{.IsOfficial}}",
	"{{json .}} {{.IsOfficial}}",
	"{{.IsOfficial}} {{json .}}",
	"{{.Description}}|",
	"table {{.StarCount}}",
	"{{.Nope}}",
}

func TestShardsSearch(t *testing.T) {
	var out []shardsSearch
	for _, results := range shardsResults {
		var items []registrytypes.SearchResult
		for _, r := range results {
			items = append(items, registrytypes.SearchResult{
				Name:        r.Name,
				Description: r.Description,
				StarCount:   r.Stars,
				IsOfficial:  r.Official,
			})
		}
		for _, f := range shardsFormats {
			for _, trunc := range []bool{true, false} {
				var buf bytes.Buffer
				ctx := formatter.Context{Output: &buf, Format: newFormat(f), Trunc: trunc}
				err := formatWrite(ctx, client.ImageSearchResult{Items: items})
				s := shardsSearch{Results: results, Format: f, Trunc: trunc, Output: buf.String()}
				if err != nil {
					s.Error = err.Error()
				}
				out = append(out, s)
			}
		}
	}
	b, err := json.MarshalIndent(out, "", "\t")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_SEARCH_OUT"), append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
