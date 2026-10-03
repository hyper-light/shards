package image

// The Docker CLI's own `images` output, for shards' to match (crates/shards/src/daemon/
// images.rs, docker-images.json). `generate` copies this file into docker/cli's
// cli/command/image and runs it there: the CLI's tree view (its default) of the images
// below, written to a pipe and to a pseudo-terminal of several widths, in colour and
// with NO_COLOR, folded and expanded (--tree); and its table (-q, --no-trunc,
// --digests), with and without truncation.

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/containerd/platforms"
	"github.com/creack/pty"
	"github.com/docker/go-units"
	"github.com/docker/cli/cli/command/formatter"
	"github.com/docker/cli/cli/streams"
	"github.com/docker/cli/internal/test"
	imagetypes "github.com/moby/moby/api/types/image"
	"github.com/opencontainers/go-digest"
	ocispec "github.com/opencontainers/image-spec/specs-go/v1"
)

// A manifest of an image, as dockerd's containerd store lists it.
type shardsManifest struct {
	ID        string `json:"id"`
	Kind      string `json:"kind"`
	Platform  string `json:"platform,omitempty"`
	For       string `json:"for,omitempty"`
	Available bool   `json:"available"`
	Content   int64  `json:"content"`
	Total     int64  `json:"total"`
	InUse     bool   `json:"in_use"`
}

// An image as dockerd lists it; Ago is seconds from its creation to the listing.
type shardsImage struct {
	ID         string           `json:"id"`
	Tags       []string         `json:"tags"`
	Digests    []string         `json:"digests"`
	Ago        int64            `json:"ago"`
	Size       int64            `json:"size"`
	Containers int64            `json:"containers"`
	Manifests  []shardsManifest `json:"manifests"`
}

type shardsTree struct {
	Images   []shardsImage `json:"images"`
	Expanded bool          `json:"expanded"`
	// 0: a pipe; else a terminal this wide.
	Width   uint   `json:"width"`
	NoColor bool   `json:"no_color"`
	Output  string `json:"output"`
}

type shardsTable struct {
	Images  []shardsImage `json:"images"`
	Trunc   bool          `json:"trunc"`
	Quiet   bool          `json:"quiet"`
	Digests bool          `json:"digests"`
	Output  string        `json:"output"`
}

const (
	idA = "sha256:5291449c3df7a8c0bd0a9e2aeb58fa40d1e1ba8cc4b6c01f12a4f8ec6b3e9b6d"
	idB = "sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc"
	idC = "sha256:bdf57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b9"
	idD = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
)

func manifests(inUse bool) []shardsManifest {
	return []shardsManifest{
		{"sha256:aaaa000000000000000000000000000000000000000000000000000000000001", "image", "linux/amd64", "", false, 4_210_000, 4_210_000, false},
		{"sha256:aaaa000000000000000000000000000000000000000000000000000000000002", "image", "linux/arm64/v8", "", true, 4_100_000, 13_400_000, inUse},
		{"sha256:aaaa000000000000000000000000000000000000000000000000000000000003", "attestation", "", "sha256:aaaa000000000000000000000000000000000000000000000000000000000002", true, 1_200, 1_200, false},
	}
}

var imageSets = [][]shardsImage{
	{
		{idA, []string{"alpine:3.22", "alpine:latest"}, []string{"alpine@" + idA}, 3600 * 30, 13_400_000, 1, manifests(true)},
		{idB, []string{"docker.io/team/a-rather-long-repository-name-for-a-column:v1.2.3-rc.1"}, nil, 120, 1_290_000_000, 0, manifests(false)},
		{idC, nil, []string{"busybox@" + idC}, 86400 * 9, 6_140_000, 0, manifests(false)[1:2]},
		{idD, []string{"localhost:5000/日本語/イメージ:最新"}, nil, 5, 999, 2, manifests(false)[1:2]},
	},
	{
		{idC, []string{"busybox:1.37"}, nil, 7, 6_140_000, 0, manifests(false)[1:2]},
	},
	// Pulled by digest: dockerd lists such a name among RepoTags.
	{
		{idC, []string{"busybox:1.37", "busybox@" + idC}, []string{"busybox@" + idC}, 7, 6_140_000, 0, manifests(false)[1:2]},
		{idA, []string{"alpine@" + idA}, []string{"alpine@" + idA}, 60, 13_400_000, 0, manifests(false)},
	},
	nil,
}

func summaries(images []shardsImage, now int64) []imagetypes.Summary {
	var out []imagetypes.Summary
	for _, img := range images {
		s := imagetypes.Summary{
			ID:          img.ID,
			RepoTags:    img.Tags,
			RepoDigests: img.Digests,
			Created:     now - img.Ago,
			Size:        img.Size,
			Containers:  img.Containers,
			SharedSize:  -1,
		}
		for _, m := range img.Manifests {
			ms := imagetypes.ManifestSummary{
				ID:         m.ID,
				Descriptor: ocispec.Descriptor{Digest: digest.Digest(m.ID)},
				Available:  m.Available,
				Kind:       imagetypes.ManifestKind(m.Kind),
			}
			ms.Size.Content = m.Content
			ms.Size.Total = m.Total
			if m.Kind == "image" {
				p, err := platforms.Parse(m.Platform)
				if err != nil {
					panic(err)
				}
				ms.ImageData = &imagetypes.ImageProperties{Platform: p}
				if m.InUse {
					ms.ImageData.Containers = []string{"c1"}
				}
			}
			if m.Kind == "attestation" {
				ms.AttestationData = &imagetypes.AttestationProperties{For: digest.Digest(m.For)}
			}
			s.Manifests = append(s.Manifests, ms)
		}
		out = append(out, s)
	}
	return out
}

// The tree view of `images` written to a pipe (width 0) or a terminal of `width`.
func tree(t *testing.T, images []shardsImage, expanded bool, width uint, noColor bool) string {
	if noColor {
		t.Setenv("NO_COLOR", "1")
	} else {
		os.Unsetenv("NO_COLOR")
	}
	cli := test.NewFakeCli(nil)
	var got bytes.Buffer
	if width == 0 {
		cli.SetOut(streams.NewOut(&got))
	} else {
		ptmx, tty, err := pty.Open()
		if err != nil {
			t.Fatal(err)
		}
		defer ptmx.Close()
		if err := pty.Setsize(tty, &pty.Winsize{Rows: 50, Cols: uint16(width)}); err != nil {
			t.Fatal(err)
		}
		// What the CLI writes comes back on the master; the line discipline's output
		// processing (ONLCR) would rewrite newlines, so the tty is raw.
		if _, err := pty.GetsizeFull(tty); err != nil {
			t.Fatal(err)
		}
		out := streams.NewOut(tty)
		if err := out.SetRawTerminal(); err != nil {
			t.Fatal(err)
		}
		cli.SetOut(out)
		done := make(chan struct{})
		go func() {
			_, _ = io.Copy(&got, ptmx)
			close(done)
		}()
		if _, err := runTree(context.Background(), cli, treeOptions{
			images:   summaries(images, time.Now().Unix()),
			expanded: expanded,
		}); err != nil {
			t.Fatal(err)
		}
		out.RestoreTerminal()
		tty.Close()
		ptmx.SetReadDeadline(time.Now().Add(2 * time.Second))
		<-done
		return strings.ReplaceAll(got.String(), "\r\n", "\n")
	}
	if _, err := runTree(context.Background(), cli, treeOptions{
		images:   summaries(images, time.Now().Unix()),
		expanded: expanded,
	}); err != nil {
		t.Fatal(err)
	}
	return got.String()
}

func TestShardsImages(t *testing.T) {
	out := os.Getenv("SHARDS_IMAGES_OUT")
	if out == "" {
		t.Skip("SHARDS_IMAGES_OUT names the file to write")
	}
	var trees []shardsTree
	for _, images := range imageSets {
		for _, expanded := range []bool{false, true} {
			for _, width := range []uint{0, 120, 80, 50, 30, 10} {
				for _, noColor := range []bool{false, true} {
					if width == 0 && noColor {
						continue
					}
					trees = append(trees, shardsTree{images, expanded, width, noColor, tree(t, images, expanded, width, noColor)})
				}
			}
		}
	}
	var tables []shardsTable
	for _, images := range imageSets {
		for _, trunc := range []bool{true, false} {
			for _, quiet := range []bool{false, true} {
				for _, digests := range []bool{false, true} {
					var buf bytes.Buffer
					ctx := formatter.ImageContext{
						Context: formatter.Context{
							Output: &buf,
							Format: formatter.NewImageFormat(formatter.TableFormatKey, quiet, digests),
							Trunc:  trunc,
						},
						Digest: digests,
					}
					if err := formatter.ImageWrite(ctx, summaries(images, time.Now().Unix())); err != nil {
						t.Fatal(err)
					}
					tables = append(tables, shardsTable{images, trunc, quiet, digests, buf.String()})
				}
			}
		}
	}
	// HumanSizeWithPrecision(n, 3), as both views print sizes, around its roundings.
	var sizes []map[string]any
	for _, n := range []int64{
		0, 1, 9, 10, 99, 100, 999, 1000, 1001, 1005, 1015, 1025, 1125, 1135, 1995, 1999, 9995,
		9999, 99949, 99950, 99999, 999499, 999500, 999999, 1_000_000, 4_105_000, 4_115_000,
		13_350_000, 1_290_000_000, 1_234_567_890_123, 999_999_999_999_999, 9_223_372_036_854_775_807,
	} {
		sizes = append(sizes, map[string]any{"n": n, "text": units.HumanSizeWithPrecision(float64(n), 3)})
	}
	// encoding/json's Indent, as IndentedInspector lays out `inspect`'s documents.
	var indents []map[string]string
	for _, in := range []string{
		`[]`, `[{}]`, `{"a":[],"b":{}}`, `[{"Id":"x","L":[1,2],"S":"a,b:{c}[\"d\"]","E":"\u003c\u0026"},{"N":null,"T":true}]`,
		`{"a":{"b":{"c":[[],[{}],[1]]}}}`, `"x"`, `[1,-2.5e+10,"é😀"]`,
	} {
		var out bytes.Buffer
		if err := json.Indent(&out, []byte(in), "", "    "); err != nil {
			t.Fatal(fmt.Errorf("%s: %w", in, err))
		}
		indents = append(indents, map[string]string{"in": in, "out": out.String()})
	}
	data, err := json.MarshalIndent(map[string]any{"trees": trees, "tables": tables, "sizes": sizes, "indents": indents}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
