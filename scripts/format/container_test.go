package container

// docker/cli's own formatter output for shards-cmdline's format module
// (crates/cmdline/tests/format.rs). `generate` copies this file into docker/cli's
// cli/command/container and runs it there, so that `ps` (runPs's steps: list.go's
// buildContainerListOptions, NewContainerFormat, ContainerWrite), `stats`
// (statsFormatWrite), `images` (ImageWrite) and `system df` (DiskUsageContext) write the
// fixtures below through the CLI's code, for each format of the corpus. It writes the
// fixtures, the cases and what they gave to SHARDS_FORMAT_PART, which history_test.go
// completes.
//
// The formatter reads time.Now and time.Local. Every case runs in a synctest bubble,
// whose clock stands still at `now`; generate sets TZ, and the zone Go gives each time
// printed is recorded for format.rs to give the same.

import (
	"bytes"
	"encoding/json"
	"net/netip"
	"os"
	"strconv"
	"testing"
	"testing/synctest"
	"time"

	"github.com/docker/cli/cli/command/formatter"
	"github.com/docker/cli/opts"
	"github.com/mattn/go-runewidth"
	"github.com/moby/moby/api/types/build"
	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/image"
	"github.com/moby/moby/api/types/network"
	"github.com/moby/moby/api/types/volume"
	"github.com/moby/moby/client"
	ocispec "github.com/opencontainers/image-spec/specs-go/v1"
)

// The moment every case runs at: 2025-06-15T12:00:00Z.
const shardsNow = 1749988800

type shardsPlatform struct {
	OS           string `json:"os"`
	Architecture string `json:"architecture"`
	Variant      string `json:"variant"`
}

type shardsPort struct {
	IP      string `json:"ip"`
	Private uint16 `json:"private"`
	Public  uint16 `json:"public"`
	Type    string `json:"type"`
}

type shardsMount struct {
	Name   string `json:"name"`
	Source string `json:"source"`
	Driver string `json:"driver"`
}

// A container as format.rs's Container holds it.
type shardsContainer struct {
	ID         string            `json:"id"`
	Names      []string          `json:"names"`
	Image      string            `json:"image"`
	ImageID    string            `json:"image_id"`
	Platform   *shardsPlatform   `json:"platform"`
	Command    string            `json:"command"`
	Created    int64             `json:"created"`
	Ports      []shardsPort      `json:"ports"`
	SizeRw     int64             `json:"size_rw"`
	SizeRootFs int64             `json:"size_root_fs"`
	Labels     map[string]string `json:"labels"`
	State      string            `json:"state"`
	Status     string            `json:"status"`
	Health     string            `json:"health"`
	Networks   []string          `json:"networks"`
	Mounts     []shardsMount     `json:"mounts"`
}

type shardsImage struct {
	ID          string   `json:"id"`
	RepoTags    []string `json:"repo_tags"`
	RepoDigests []string `json:"repo_digests"`
	Created     int64    `json:"created"`
	Size        int64    `json:"size"`
	SharedSize  int64    `json:"shared_size"`
	Containers  int64    `json:"containers"`
}

type shardsStats struct {
	Container        string  `json:"container"`
	Name             string  `json:"name"`
	ID               string  `json:"id"`
	CPUPercentage    float64 `json:"cpu_percentage"`
	Memory           float64 `json:"memory"`
	MemoryLimit      float64 `json:"memory_limit"`
	MemoryPercentage float64 `json:"memory_percentage"`
	NetworkRx        float64 `json:"network_rx"`
	NetworkTx        float64 `json:"network_tx"`
	BlockRead        float64 `json:"block_read"`
	BlockWrite       float64 `json:"block_write"`
	PidsCurrent      uint64  `json:"pids"`
	IsInvalid        bool    `json:"invalid"`
}

type shardsVolume struct {
	Name       string            `json:"name"`
	Driver     string            `json:"driver"`
	Scope      string            `json:"scope"`
	Mountpoint string            `json:"mountpoint"`
	Labels     map[string]string `json:"labels"`
	// [RefCount, Size], or none.
	Usage []int64 `json:"usage"`
}

// Times in nanoseconds since the epoch.
type shardsCache struct {
	ID          string   `json:"id"`
	Parents     []string `json:"parents"`
	Type        string   `json:"kind"`
	Description string   `json:"description"`
	InUse       bool     `json:"in_use"`
	Shared      bool     `json:"shared"`
	Size        int64    `json:"size"`
	CreatedAt   int64    `json:"created_at"`
	LastUsedAt  *int64   `json:"last_used_at"`
	UsageCount  int      `json:"usage_count"`
}

type shardsCounts struct {
	Total       int64 `json:"total_count"`
	Active      int64 `json:"active_count"`
	Size        int64 `json:"total_size"`
	Reclaimable int64 `json:"reclaimable"`
}

// A case: a command, its fixtures (the set's name, or "empty"), its flags, and what the
// CLI wrote or the error it returned.
type shardsCase struct {
	Command   string `json:"command"`
	Set       string `json:"set"`
	Format    string `json:"format"`
	Quiet     bool   `json:"quiet,omitempty"`
	Size      bool   `json:"size,omitempty"`
	NoTrunc   bool   `json:"no_trunc,omitempty"`
	Digests   bool   `json:"digests,omitempty"`
	Human     bool   `json:"human,omitempty"`
	Verbose   bool   `json:"verbose,omitempty"`
	EastAsian bool   `json:"east_asian,omitempty"`
	Output    string `json:"output"`
	Error     string `json:"error,omitempty"`
}

type shardsOracle struct {
	Now        int64                   `json:"now"`
	Zones      map[string]shardsZone   `json:"zones"`
	Containers []shardsContainer       `json:"containers"`
	Images     []shardsImage           `json:"images"`
	Stats      []shardsStats           `json:"stats"`
	Volumes    []shardsVolume          `json:"volumes"`
	Cache      []shardsCache           `json:"cache"`
	Counts     map[string]shardsCounts `json:"counts"`
	Cases      []shardsCase            `json:"cases"`
}

type shardsZone struct {
	Offset int    `json:"offset"`
	Name   string `json:"name"`
}

const (
	dig1 = "sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1"
	dig2 = "sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc"
	idA  = "sha256:5291449c3df7a8c0bd0a9e2aeb58fa40d1e1ba8cc4b6c01f12a4f8ec6b3e9b6d"
	idB  = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
)

var shardsContainers = []shardsContainer{
	{
		ID: "4bed76d3ad428b889c56c1ecc2bf2ed95cb08256db22dc5ef5863e1d03252a19", Names: []string{"/web"},
		Image: "nginx:alpine", ImageID: idA, Platform: &shardsPlatform{"linux", "amd64", ""},
		Command: "/docker-entrypoint.sh nginx -g 'daemon off;'", Created: shardsNow - 90,
		Ports:  []shardsPort{{"0.0.0.0", 80, 18080, "tcp"}, {"::", 80, 18080, "tcp"}, {"", 443, 0, "tcp"}},
		SizeRw: 1234, SizeRootFs: 5_000_000, Labels: map[string]string{"com.example.team": "core", "a": "1"},
		State: "running", Status: "Up About a minute (healthy)", Networks: []string{"bridge"},
		Mounts: []shardsMount{{"data-volume-with-a-long-name", "/var/lib/docker/volumes/x", "local"}, {"", "/host/path/x", ""}},
	},
	{
		ID: "b0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000001", Names: []string{"/x", "/web/link-alias"},
		Image: "alpine@" + dig1, ImageID: idB, Platform: &shardsPlatform{"linux", "arm64", "v8"},
		Command: "sh", Created: shardsNow - 4, State: "exited", Status: "Exited (0) 3 seconds ago",
		Health: "starting", Mounts: []shardsMount{{"日本語のボリューム名前です", "", "local"}},
	},
	{
		ID: "c0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000002", Names: []string{"/日本語の名前"},
		Image: "localhost:5000/team/app:v1.2@" + dig1, ImageID: idB,
		Command: "printf 'a\tb\nc' \"quoted\" \\ back", Created: shardsNow - 7200,
		Ports: []shardsPort{
			{"0.0.0.0", 5000, 5000, "tcp"}, {"0.0.0.0", 5001, 6001, "tcp"}, {"0.0.0.0", 5002, 5002, "tcp"},
			{"::", 9000, 9001, "udp"}, {"", 22, 0, "tcp"}, {"10.0.0.1", 9000, 9000, "tcp"}, {"", 23, 0, "tcp"},
			{"0.0.0.0", 7, 7, "sctp"}, {"", 24, 0, "tcp"}, {"", 26, 0, "tcp"},
		},
		SizeRw: 999_999, Labels: map[string]string{"html": "<b>&amp;</b>", "com.docker.compose.project": "demo", "empty": ""},
		State: "running", Status: "Up 2 hours (health: starting)", Networks: []string{"demo_default"},
	},
	{
		ID: idB, Names: []string{"/zero"}, Image: idB, ImageID: idB,
		Platform: &shardsPlatform{"", "amd64", ""}, Command: "echo 😀 done\x01\x7f", Created: 0,
		State: "created", Status: "Created", SizeRootFs: 0,
	},
	{
		ID: "e0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000004", Names: []string{"/a/b"},
		Image: "UPPER/Case:tag", ImageID: "sha256:4bed76d3ad428b889c56c1ecc2bf2ed95cb08256db22dc5ef5863e1d03252a19",
		Command: "echo 日本語の文字列です", Created: shardsNow + 100, State: "paused", Status: "Up 25 hours (Paused)",
		Labels: map[string]string{}, Networks: []string{"host"},
	},
	{
		ID: "f0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000005", Names: []string{"/short"},
		Image: "4bed76d3ad42", ImageID: "sha256:4bed76d3ad428b889c56c1ecc2bf2ed95cb08256db22dc5ef5863e1d03252a19",
		Command: "", Created: shardsNow - 5_184_000, State: "exited", Status: "Exited (137) 8 weeks ago",
		SizeRw: 0, SizeRootFs: 1_290_000_000,
		Ports: []shardsPort{{"0.0.0.0", 80, 80, "tcp"}, {"0.0.0.0", 81, 81, "tcp"}, {"::", 80, 80, "tcp"}, {"0.0.0.0", 83, 83, "tcp"}, {"::", 81, 81, "tcp"}},
	},
	{
		ID: "a1318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000006", Names: []string{"/ghcr"},
		Image: "ghcr.io/org/image:1.0", ImageID: idA, Command: "echo …… ambiguous width",
		Created: shardsNow - 172800, State: "exited", Status: "Exited (1) 2 days ago (unhealthy)",
		Ports: []shardsPort{{"", 80, 0, "tcp"}, {"", 81, 0, "tcp"}, {"", 82, 0, "tcp"}, {"", 90, 0, "udp"}, {"", 84, 0, "tcp"}},
	},
	{
		ID: "a2318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000007", Names: []string{"/a-rather-long-container-name_1"},
		Image: "docker.io/library/busybox:1.36", ImageID: idA, Command: "sh -c 'echo \"quoted\" \\ back'",
		Created: shardsNow - 1209600, State: "running", Status: "Up 2 weeks",
		Ports: []shardsPort{{"::1", 8080, 8080, "tcp"}, {"::ffff:10.0.0.1", 53, 53, "udp"}},
	},
}

var shardsImages = []shardsImage{
	{idA, []string{"alpine:3.22", "alpine:latest"}, []string{"alpine@" + dig1}, shardsNow - 3600*30, 13_400_000, 1_200, 1},
	{idB, []string{"docker.io/team/a-rather-long-repository-name-for-a-column:v1.2.3-rc.1"}, nil, shardsNow - 120, 1_290_000_000, -1, -1},
	{"sha256:bdf57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b9", nil, []string{"busybox@" + dig2}, shardsNow - 86400*9, 6_140_000, 0, 0},
	{"sha256:aaaa57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b", nil, nil, shardsNow - 5, 999, 0, 2},
	{"sha256:bbbb57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b", []string{"<none>:<none>"}, []string{"<none>@<none>"}, 0, -1, 5, 0},
	{"sha256:cccc57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b", []string{"localhost:5000/日本語/イメージ:最新"}, nil, shardsNow - 60, 1_000_000, 0, 0},
	{"sha256:dddd57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b", []string{"foo:1", "foo:2"}, []string{"bar@" + dig2}, -62135596800, 1_500, 1_000, 0},
	{"sha256:eeee57e528e45b1b3f8d1c8a1b7e3d2f9e4c5a6b7c8d9e0f1a2b3c4d5e6f7a8b", []string{"localhost:5000/team/app:v1"}, []string{"localhost:5000/team/app@" + dig1, "localhost:5000/team/app@" + dig2}, shardsNow - 7*86400*10, 1_125, 0, 0},
}

var shardsStatsSet = []shardsStats{
	{"web", "/web", "4bed76d3ad428b889c56c1ecc2bf2ed95cb08256db22dc5ef5863e1d03252a19", 0.005, 12_345_678, 2 << 30, 0.57, 1_500, 999_999, 0, 4096, 3, false},
	{"b0318bca5aef", "/日本語の名前", "b0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000001", 199.999, 1 << 30, 1 << 40, 100, 1.25e9, 3e12, 1 << 20, 1 << 30, 0, false},
	{"gone", "", "c0318bca5aef1e4c6b4a0a3c9c6d8e2f00000000000000000000000000000002", 0, 0, 0, 0, 0, 0, 0, 0, 0, true},
	{"x", "/", "short", 12.3456, 1023, 1024, 0.125, 999.5, 1e21, 0.5, 1, 18446744073709551615, false},
}

func shardsPtr(v int64) *int64 { return &v }

var shardsVolumes = []shardsVolume{
	{"data-volume-with-a-long-name", "local", "local", "/var/lib/docker/volumes/data/_data", map[string]string{"b": "2", "a": "<1>"}, []int64{2, 1_500_000}},
	{"9b7c3f4a8e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d7c6b", "local", "local", "/var/lib/docker/volumes/anon/_data", nil, nil},
	{"日本語", "local", "local", "/v", map[string]string{}, []int64{0, 0}},
}

var shardsCacheSet = []shardsCache{
	{"zz9xk2b4c1y8w7v6u5t4s3r2q", []string{"aa1xk2b4c1y8w7v6u5t4s3r2q"}, "regular", "mount / from exec /bin/sh -c apk add --no-cache git", true, false, 41_000_000, (shardsNow - 3600) * 1e9, shardsPtr((shardsNow-60)*1e9 + 123_000_000), 3},
	{"aa1xk2b4c1y8w7v6u5t4s3r2q", nil, "source.local", "local source for context", false, true, 4_096, (shardsNow - 86400*3) * 1e9, nil, 1},
	{"mm1xk2b4c1y8w7v6u5t4s3r2q", []string{"p1", "p2"}, "frontend", "", false, false, 0, (shardsNow - 120) * 1e9, shardsPtr((shardsNow - 60) * 1e9), 0},
	{"bb1xk2b4c1y8w7v6u5t4s3r2q", nil, "internal", "<b>", false, false, 999_999, 0, shardsPtr((shardsNow - 60) * 1e9), 12},
}

var shardsCountSet = map[string]shardsCounts{
	"images":      {8, 2, 1_310_000_000, 1_296_000_000},
	"containers":  {8, 3, 1_291_000_000, 0},
	"volumes":     {3, 1, 1_500_000, 1_500_000},
	"build_cache": {4, 1, 41_103_000, 999_999_999_999},
}

func shardsToContainer(c shardsContainer) container.Summary {
	s := container.Summary{
		ID: c.ID, Names: c.Names, Image: c.Image, ImageID: c.ImageID, Command: c.Command,
		Created: c.Created, SizeRw: c.SizeRw, SizeRootFs: c.SizeRootFs, Labels: c.Labels,
		State: container.ContainerState(c.State), Status: c.Status,
	}
	if c.Platform != nil {
		s.ImageManifestDescriptor = &ocispec.Descriptor{Platform: &ocispec.Platform{OS: c.Platform.OS, Architecture: c.Platform.Architecture, Variant: c.Platform.Variant}}
	}
	for _, p := range c.Ports {
		var ip netip.Addr
		if p.IP != "" {
			ip = netip.MustParseAddr(p.IP)
		}
		s.Ports = append(s.Ports, container.PortSummary{IP: ip, PrivatePort: p.Private, PublicPort: p.Public, Type: p.Type})
	}
	if c.Health != "" {
		s.Health = &container.HealthSummary{Status: container.HealthStatus(c.Health)}
	}
	if c.Networks != nil {
		s.NetworkSettings = &container.NetworkSettingsSummary{Networks: map[string]*network.EndpointSettings{}}
		for _, n := range c.Networks {
			s.NetworkSettings.Networks[n] = &network.EndpointSettings{}
		}
	}
	for _, m := range c.Mounts {
		s.Mounts = append(s.Mounts, container.MountPoint{Name: m.Name, Source: m.Source, Driver: m.Driver})
	}
	return s
}

func shardsToImage(i shardsImage) image.Summary {
	return image.Summary{ID: i.ID, RepoTags: i.RepoTags, RepoDigests: i.RepoDigests, Created: i.Created, Size: i.Size, SharedSize: i.SharedSize, Containers: i.Containers}
}

func shardsToStats(s shardsStats) StatsEntry {
	return StatsEntry{
		Container: s.Container, Name: s.Name, ID: s.ID, CPUPercentage: s.CPUPercentage, Memory: s.Memory,
		MemoryLimit: s.MemoryLimit, MemoryPercentage: s.MemoryPercentage, NetworkRx: s.NetworkRx,
		NetworkTx: s.NetworkTx, BlockRead: s.BlockRead, BlockWrite: s.BlockWrite, PidsCurrent: s.PidsCurrent,
		IsInvalid: s.IsInvalid,
	}
}

func shardsToVolume(v shardsVolume) volume.Volume {
	out := volume.Volume{Name: v.Name, Driver: v.Driver, Scope: v.Scope, Mountpoint: v.Mountpoint, Labels: v.Labels}
	if len(v.Usage) == 2 {
		out.UsageData = &volume.UsageData{RefCount: v.Usage[0], Size: v.Usage[1]}
	}
	return out
}

func shardsToCache(c shardsCache) build.CacheRecord {
	out := build.CacheRecord{
		ID: c.ID, Parents: c.Parents, Type: c.Type, Description: c.Description, InUse: c.InUse,
		Shared: c.Shared, Size: c.Size, CreatedAt: time.Unix(0, c.CreatedAt), UsageCount: c.UsageCount,
	}
	if c.LastUsedAt != nil {
		t := time.Unix(0, *c.LastUsedAt)
		out.LastUsedAt = &t
	}
	return out
}

func shardsMap[T, U any](in []T, f func(T) U) []U {
	var out []U
	for _, v := range in {
		out = append(out, f(v))
	}
	return out
}

// runAt runs f with time.Now at shardsNow.
func shardsRunAt(t *testing.T, f func()) {
	synctest.Test(t, func(t *testing.T) {
		time.Sleep(time.Unix(shardsNow, 0).Sub(time.Now()))
		f()
	})
}

func shardsPs(c *shardsCase, list []container.Summary) {
	// runPs, with no psFormat configured; --size given sets sizeChanged.
	options := &psOptions{filter: opts.NewFilterOpt(), format: c.Format, quiet: c.Quiet, size: c.Size, sizeChanged: c.Size, noTrunc: c.NoTrunc, last: -1}
	listOptions, err := buildContainerListOptions(options)
	if err != nil {
		c.Error = err.Error()
		return
	}
	var buf bytes.Buffer
	ctx := formatter.Context{Output: &buf, Format: formatter.NewContainerFormat(options.format, options.quiet, listOptions.Size), Trunc: !options.noTrunc}
	if err := formatter.ContainerWrite(ctx, list); err != nil {
		c.Error = err.Error()
	}
	c.Output = buf.String()
}

func shardsDefaultTable(f string) string {
	if f == "" {
		return formatter.TableFormatKey
	}
	return f
}

func shardsImagesCase(c *shardsCase, list []image.Summary) {
	var buf bytes.Buffer
	ctx := formatter.ImageContext{
		Context: formatter.Context{Output: &buf, Format: formatter.NewImageFormat(shardsDefaultTable(c.Format), c.Quiet, c.Digests), Trunc: !c.NoTrunc},
		Digest:  c.Digests,
	}
	if err := formatter.ImageWrite(ctx, list); err != nil {
		c.Error = err.Error()
	}
	c.Output = buf.String()
}

func shardsStatsCase(c *shardsCase, list []StatsEntry) {
	var buf bytes.Buffer
	ctx := formatter.Context{Output: &buf, Format: NewStatsFormat(shardsDefaultTable(c.Format), "linux")}
	if err := statsFormatWrite(ctx, list, "linux", !c.NoTrunc); err != nil {
		c.Error = err.Error()
	}
	c.Output = buf.String()
}

func shardsDf(c *shardsCase, empty bool) {
	var buf bytes.Buffer
	counts := func(k string) shardsCounts {
		if empty {
			return shardsCounts{}
		}
		return shardsCountSet[k]
	}
	du := formatter.DiskUsageContext{
		Context: formatter.Context{Output: &buf, Format: formatter.NewDiskUsageFormat(shardsDefaultTable(c.Format), c.Verbose)},
		Verbose: c.Verbose,
	}
	i, ct, v, b := counts("images"), counts("containers"), counts("volumes"), counts("build_cache")
	du.ImageDiskUsage = client.ImagesDiskUsage{TotalCount: i.Total, ActiveCount: i.Active, TotalSize: i.Size, Reclaimable: i.Reclaimable}
	du.ContainerDiskUsage = client.ContainersDiskUsage{TotalCount: ct.Total, ActiveCount: ct.Active, TotalSize: ct.Size, Reclaimable: ct.Reclaimable}
	du.VolumeDiskUsage = client.VolumesDiskUsage{TotalCount: v.Total, ActiveCount: v.Active, TotalSize: v.Size, Reclaimable: v.Reclaimable}
	du.BuildCacheDiskUsage = client.BuildCacheDiskUsage{TotalCount: b.Total, ActiveCount: b.Active, TotalSize: b.Size, Reclaimable: b.Reclaimable}
	if !empty {
		du.ImageDiskUsage.Items = shardsMap(shardsImages, shardsToImage)
		du.ContainerDiskUsage.Items = shardsMap(shardsContainers, shardsToContainer)
		du.VolumeDiskUsage.Items = shardsMap(shardsVolumes, shardsToVolume)
		du.BuildCacheDiskUsage.Items = shardsMap(shardsCacheSet, shardsToCache)
	}
	if err := du.Write(); err != nil {
		c.Error = err.Error()
	}
	c.Output = buf.String()
}

// Formats every command is run with; FIELD is each command's own column.
var shardsCommonFormats = []string{
	"", "table", "json", "raw", "{{json .}}", "table {{json .}}", "{{.ID}}", "table {{.ID}}",
	"{{.Missing}}", "table {{.Missing}}", "{{.ID", "table {{.ID}}\\t{{.Missing.X}}", `{{.ID "x"}}`,
	"table {{.FullHeader}}\\t{{.ID}}", "{{.FullHeader}}|{{.Header}}", "{{json .FullHeader}}",
	"  table {{.ID}}  ", "tablex {{.ID}}", "table {{.ID}}\\n\\t{{.ID}}", "{{.ID}}\\t\\\\t{{.ID}}",
	"table {{upper .ID}}\\t{{lower .ID}}\\t{{title .ID}}\\t{{truncate .ID 3}}\\t{{json .ID}}\\t{{split .ID \"a\"}}\\t{{join .ID \",\"}}",
	"table {{.Label \"com.example.team-name_x\"}}", "table {{json .}}\\t{{.ID}}", "table\t{{.ID}}",
	"{{printf \"%q\" .ID}}", "table {{.ID}}\f{{.ID}}\v{{.ID}}",
}

var shardsCommandFormats = map[string][]string{
	"ps": {
		"table {{.ID}}\\t{{.Names}}", "{{.Names}}: {{.Status}}", "table {{.Ports}}", "{{.Ports}}",
		"table {{.Image}}\\t{{.Command}}\\t{{.CreatedAt}}\\t{{.RunningFor}}", "{{.Size}}", "table {{.Size}}\\t{{.ID}}",
		`{{.Label "com.example.team"}}|{{.Label "html"}}|{{.Label "none"}}`, "table {{.Labels}}\\t{{.Mounts}}\\t{{.LocalVolumes}}",
		"table {{.Networks}}\\t{{.Platform}}\\t{{.HealthStatus}}\\t{{.State}}", "{{json .Platform}}|{{json .Labels}}",
		"{{range split .Labels \",\"}}[{{.}}]{{end}}", "{{pad .State 2 1}}|", "table {{.Names}}\\t{{.Names}}\\t{{.Names}}\\t{{.Names}}",
		"table {{.Platform}}", "{{if .Ports}}{{.Ports}}{{else}}none{{end}}",
	},
	"images": {
		"table {{.Repository}}\\t{{.Tag}}", "{{.Repository}}:{{.Tag}} {{.Digest}}", "table {{.Digest}}\\t{{.ID}}",
		"table {{.CreatedAt}}\\t{{.CreatedSince}}\\t{{.Size}}", "{{.Containers}} {{.SharedSize}} {{.UniqueSize}}",
		"table {{.Repository}}\\t{{.Digest}}", "{{json .Repository}}",
	},
	"stats": {
		"table {{.Container}}\\t{{.Name}}", "{{.CPUPerc}} {{.MemUsage}} {{.MemPerc}}", "table {{.NetIO}}\\t{{.BlockIO}}\\t{{.PIDs}}",
		"table {{.ID}}\\t{{.Name}}\\t{{.CPUPerc}}", "{{.Name}}",
	},
	"df": {
		"table {{.Type}}\\t{{.Size}}", "{{.Type}}: {{.Reclaimable}}", "table {{.TotalCount}}\\t{{.Active}}\\t{{.Reclaimable}}",
		"{{range .Images}}{{.Repository}} {{end}}", "{{len .Containers}}", "{{range .Volumes}}{{.Name}} {{.Links}} {{.Size}} {{.Labels}} {{.Label \"a\"}} {{.Group}};{{end}}",
		"{{range .BuildCache}}{{.ID}} {{.Parent}} {{.CacheType}} {{.CreatedAt}} {{.LastUsedAt}} {{.LastUsedSince}} {{.InUse}} {{.UsageCount}};{{end}}",
		"{{json .Volumes}}", "{{json .BuildCache}}", "{{json .}}",
	},
}

func TestShardsFormat(t *testing.T) {
	out := os.Getenv("SHARDS_FORMAT_PART")
	if out == "" {
		t.Skip("SHARDS_FORMAT_PART names the file to write")
	}
	oracle := shardsOracle{
		Now: shardsNow, Containers: shardsContainers, Images: shardsImages, Stats: shardsStatsSet,
		Volumes: shardsVolumes, Cache: shardsCacheSet, Counts: shardsCountSet, Zones: map[string]shardsZone{},
	}
	add := func(c shardsCase, run func(*shardsCase)) {
		runewidth.DefaultCondition.EastAsianWidth = c.EastAsian
		shardsRunAt(t, func() { run(&c) })
		runewidth.DefaultCondition.EastAsianWidth = false
		oracle.Cases = append(oracle.Cases, c)
	}
	containers := shardsMap(shardsContainers, shardsToContainer)
	images := shardsMap(shardsImages, shardsToImage)
	stats := shardsMap(shardsStatsSet, shardsToStats)
	for _, set := range []string{"all", "empty"} {
		for _, f := range append(append([]string{}, shardsCommonFormats...), shardsCommandFormats["ps"]...) {
			for _, flags := range [][3]bool{{false, false, false}, {true, false, false}, {false, true, false}, {false, false, true}} {
				c := shardsCase{Command: "ps", Set: set, Format: f, Quiet: flags[0], Size: flags[1], NoTrunc: flags[2]}
				add(c, func(c *shardsCase) {
					if c.Set == "all" {
						shardsPs(c, containers)
					} else {
						shardsPs(c, nil)
					}
				})
			}
		}
		for _, f := range append(append([]string{}, shardsCommonFormats...), shardsCommandFormats["images"]...) {
			for _, flags := range [][3]bool{{false, false, false}, {true, false, false}, {false, true, false}, {false, false, true}} {
				c := shardsCase{Command: "images", Set: set, Format: f, Quiet: flags[0], Digests: flags[1], NoTrunc: flags[2]}
				add(c, func(c *shardsCase) {
					if c.Set == "all" {
						shardsImagesCase(c, images)
					} else {
						shardsImagesCase(c, nil)
					}
				})
			}
		}
		for _, f := range append(append([]string{}, shardsCommonFormats...), shardsCommandFormats["stats"]...) {
			for _, noTrunc := range []bool{false, true} {
				c := shardsCase{Command: "stats", Set: set, Format: f, NoTrunc: noTrunc}
				add(c, func(c *shardsCase) {
					if c.Set == "all" {
						shardsStatsCase(c, stats)
					} else {
						shardsStatsCase(c, nil)
					}
				})
			}
		}
		for _, f := range append(append([]string{}, shardsCommonFormats...), shardsCommandFormats["df"]...) {
			for _, verbose := range []bool{false, true} {
				c := shardsCase{Command: "df", Set: set, Format: f, Verbose: verbose}
				add(c, func(c *shardsCase) { shardsDf(c, c.Set == "empty") })
			}
		}
	}
	// Default tables in an East Asian locale.
	for _, cmd := range []string{"ps", "images", "stats", "df"} {
		for _, verbose := range []bool{false, true} {
			if verbose && cmd != "df" {
				continue
			}
			c := shardsCase{Command: cmd, Set: "all", Format: "", Verbose: verbose, EastAsian: true}
			add(c, func(c *shardsCase) {
				switch c.Command {
				case "ps":
					shardsPs(c, containers)
				case "images":
					shardsImagesCase(c, images)
				case "stats":
					shardsStatsCase(c, stats)
				case "df":
					shardsDf(c, false)
				}
			})
		}
	}
	// The zone of each time printed.
	zone := func(sec int64) {
		name, offset := time.Unix(sec, 0).Zone()
		oracle.Zones[strconv.FormatInt(sec, 10)] = shardsZone{offset, name}
	}
	for _, c := range shardsContainers {
		zone(c.Created)
	}
	for _, i := range shardsImages {
		zone(i.Created)
	}
	for _, b := range shardsCacheSet {
		zone(b.CreatedAt / 1e9)
		if b.LastUsedAt != nil {
			zone(*b.LastUsedAt / 1e9)
		}
	}
	zone(0)
	data, err := json.MarshalIndent(oracle, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
