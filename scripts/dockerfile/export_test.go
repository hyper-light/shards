package containerimage

// BuildKit's image exporter's own answers, for crates/dockerfile's export.rs to match.
// `generate` copies this file into BuildKit's exporter/containerimage, where the
// exporter's functions live, and runs it there. For each image plan.json records, it
// exports the config with layers made up for it, fewer, as many and more than its history
// claims, with and without SOURCE_DATE_EPOCH and the base image's history, as
// commitDistributionManifest does: normalizeLayersAndHistory, patchImageConfig, and the
// manifest marshalled as it marshals it.

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"testing"
	"time"

	"github.com/moby/buildkit/solver"
	dockerspec "github.com/moby/docker-image-spec/specs-go/v1"
	digest "github.com/opencontainers/go-digest"
	specs "github.com/opencontainers/image-spec/specs-go"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
)

type exportCase struct {
	File   string `json:"file"`
	Layers int    `json:"layers"`
	Epoch  bool   `json:"epoch"`
	Base   bool   `json:"base"`
	Config string `json:"config,omitempty"`
	Error  string `json:"error,omitempty"`
	Manifest string `json:"manifest,omitempty"`
}

func TestShardsExport(t *testing.T) {
	in := os.Getenv("SHARDS_PLANS")
	out := os.Getenv("SHARDS_EXPORT_OUT")
	if in == "" || out == "" {
		t.Skip("SHARDS_PLANS and SHARDS_EXPORT_OUT name the files")
	}
	data, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	var plans []map[string]any
	if err := json.Unmarshal(data, &plans); err != nil {
		t.Fatal(err)
	}
	epoch := time.Unix(1700000000, 0).UTC()
	var cases []exportCase
	for _, p := range plans {
		img, ok := p["image"].(string)
		if !ok {
			continue
		}
		var image dockerspec.DockerOCIImage
		if err := json.Unmarshal([]byte(img), &image); err != nil {
			t.Fatal(err)
		}
		claimed := 0
		for _, h := range image.History {
			if !h.EmptyLayer {
				claimed++
			}
		}
		counts := []int{claimed, claimed + 1}
		if claimed > 0 {
			counts = append(counts, claimed-1)
		}
		for _, n := range counts {
			for _, withEpoch := range []bool{false, true} {
				for _, withBase := range []bool{false, true} {
					c := exportCase{File: p["file"].(string), Layers: n, Epoch: withEpoch, Base: withBase}
					var descs []ocispecs.Descriptor
					for i := 0; i < n; i++ {
						descs = append(descs, ocispecs.Descriptor{
							MediaType: "application/vnd.docker.image.rootfs.diff.tar.gzip",
							Digest:    digest.FromString(fmt.Sprintf("blob %d", i)),
							Size:      int64(100 + i),
							Annotations: map[string]string{
								"containerd.io/uncompressed": digest.FromString(fmt.Sprintf("diff %d", i)).String(),
								"buildkit/createdat":         "x",
								"org.example.kept":           fmt.Sprintf("v%d <&>", i),
							},
						})
					}
					var e *time.Time
					if withEpoch {
						e = &epoch
					}
					var base *dockerspec.DockerOCIImage
					if withBase {
						base = &image
					}
					history, err := parseHistoryFromConfig([]byte(img))
					if err != nil {
						t.Fatal(err)
					}
					remote := &solver.Remote{Descriptors: descs}
					remote, history = normalizeLayersAndHistory(context.Background(), remote, history, nil, true)
					config, err := patchImageConfig([]byte(img), remote.Descriptors, history, nil, e, base)
					if err != nil {
						c.Error = err.Error()
						cases = append(cases, c)
						continue
					}
					c.Config = string(config)
					mfst := ocispecs.Manifest{
						MediaType: ocispecs.MediaTypeImageManifest,
						Versioned: specs.Versioned{SchemaVersion: 2},
						Config: ocispecs.Descriptor{
							Digest:    digest.FromBytes(config),
							Size:      int64(len(config)),
							MediaType: ocispecs.MediaTypeImageConfig,
						},
					}
					for _, desc := range remote.Descriptors {
						desc.Annotations = RemoveInternalLayerAnnotations(desc.Annotations, true)
						mfst.Layers = append(mfst.Layers, desc)
					}
					m, err := json.MarshalIndent(mfst, "", "  ")
					if err != nil {
						t.Fatal(err)
					}
					c.Manifest = string(m)
					cases = append(cases, c)
				}
			}
		}
	}
	b, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
