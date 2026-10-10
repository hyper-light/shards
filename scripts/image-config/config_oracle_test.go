package containerd

// What moby's dockerd makes of an image's config when it runs a container from it, for
// shards to match (crates/image/src/config.rs, testdata/image-config.json). `generate`
// copies this file into moby's daemon/containerd and runs it there, on Linux, as dockerd
// runs: GetImage (image.go) reads the config with json.Unmarshal into a
// dockerspec.DockerOCIImage, and fails the run with "could not deserialize image config:"
// and Go's error where that fails (image_list.go readJSON); its container config keeps the
// exposed ports network.ParsePort takes (imagespec.go). Each case records Go's error, or
// every field as decoded, a nil slice or map as null and an empty one as [] or {}.

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"testing"
	"time"

	dockerspec "github.com/moby/docker-image-spec/specs-go/v1"
	ocispec "github.com/opencontainers/image-spec/specs-go/v1"
)

type shardsCase struct {
	name   string
	config string
}

type shardsHealth struct {
	Test          []string `json:"test"`
	Interval      int64    `json:"interval"`
	Timeout       int64    `json:"timeout"`
	StartPeriod   int64    `json:"start_period"`
	StartInterval int64    `json:"start_interval"`
	Retries       int      `json:"retries"`
}

type shardsConfig struct {
	User         string            `json:"user"`
	ExposedPorts []string          `json:"exposed_ports"`
	Env          []string          `json:"env"`
	Entrypoint   []string          `json:"entrypoint"`
	Cmd          []string          `json:"cmd"`
	Volumes      []string          `json:"volumes"`
	WorkingDir   string            `json:"working_dir"`
	Labels       map[string]string `json:"labels"`
	StopSignal   string            `json:"stop_signal"`
	ArgsEscaped  bool              `json:"args_escaped"`
	Healthcheck  *shardsHealth     `json:"healthcheck"`
	OnBuild      []string          `json:"on_build"`
	Shell        []string          `json:"shell"`
}

type shardsHistory struct {
	Created    *string `json:"created"`
	CreatedBy  string  `json:"created_by"`
	Author     string  `json:"author"`
	Comment    string  `json:"comment"`
	EmptyLayer bool    `json:"empty_layer"`
}

type shardsImage struct {
	Created      *string         `json:"created"`
	Author       string          `json:"author"`
	Architecture string          `json:"architecture"`
	OS           string          `json:"os"`
	OSVersion    string          `json:"os_version"`
	OSFeatures   []string        `json:"os_features"`
	Variant      string          `json:"variant"`
	RootFSType   string          `json:"rootfs_type"`
	DiffIDs      []string        `json:"diff_ids"`
	History      []shardsHistory `json:"history"`
	Config       shardsConfig    `json:"config"`
	// The container config's exposed ports (dockerOCIImageConfigToContainerConfig).
	Ports []string `json:"ports"`
}

type shardsRecord struct {
	Name   string       `json:"name"`
	Config string       `json:"config"`
	Error  string       `json:"error"`
	Image  *shardsImage `json:"image"`
	// The same config read as ocispec.Image, as containerd's unpack reads its rootfs
	// (core/images RootFS) when an image is pulled.
	OCIError string       `json:"oci_error"`
	OCIImage *shardsImage `json:"oci_image"`
	// And as image_history.go reads it for `docker history`.
	HistoryError string              `json:"history_error"`
	History      *shardsHistoryImage `json:"history"`
}

type shardsHistoryImage struct {
	RootFSType string          `json:"rootfs_type"`
	DiffIDs    []string        `json:"diff_ids"`
	History    []shardsHistory `json:"history"`
}

func shardsTime(t *time.Time) *string {
	if t == nil {
		return nil
	}
	s := t.Format(time.RFC3339Nano)
	return &s
}

func shardsKeys(m map[string]struct{}) []string {
	if m == nil {
		return nil
	}
	keys := []string{}
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func shardsDiffIDs(rootfs ocispec.RootFS) []string {
	if rootfs.DiffIDs == nil {
		return nil
	}
	ids := []string{}
	for _, d := range rootfs.DiffIDs {
		ids = append(ids, d.String())
	}
	return ids
}

func shardsHistories(history []ocispec.History) []shardsHistory {
	if history == nil {
		return nil
	}
	out := []shardsHistory{}
	for _, h := range history {
		out = append(out, shardsHistory{
			Created:    shardsTime(h.Created),
			CreatedBy:  h.CreatedBy,
			Author:     h.Author,
			Comment:    h.Comment,
			EmptyLayer: h.EmptyLayer,
		})
	}
	return out
}

// ocispec.Image's fields, those Docker adds to its config left null.
func shardsDumpOCI(img ocispec.Image) *shardsImage {
	c := img.Config
	return &shardsImage{
		Created:      shardsTime(img.Created),
		Author:       img.Author,
		Architecture: img.Architecture,
		OS:           img.OS,
		OSVersion:    img.OSVersion,
		OSFeatures:   img.OSFeatures,
		Variant:      img.Variant,
		RootFSType:   img.RootFS.Type,
		DiffIDs:      shardsDiffIDs(img.RootFS),
		History:      shardsHistories(img.History),
		Config: shardsConfig{
			User:         c.User,
			ExposedPorts: shardsKeys(c.ExposedPorts),
			Env:          c.Env,
			Entrypoint:   c.Entrypoint,
			Cmd:          c.Cmd,
			Volumes:      shardsKeys(c.Volumes),
			WorkingDir:   c.WorkingDir,
			Labels:       c.Labels,
			StopSignal:   c.StopSignal,
			ArgsEscaped:  c.ArgsEscaped, //nolint:staticcheck // the image's, as moby keeps it
		},
	}
}

func shardsDump(img dockerspec.DockerOCIImage) *shardsImage {
	c := img.Config
	out := &shardsImage{
		Created:      shardsTime(img.Created),
		Author:       img.Author,
		Architecture: img.Architecture,
		OS:           img.OS,
		OSVersion:    img.OSVersion,
		OSFeatures:   img.OSFeatures,
		Variant:      img.Variant,
		RootFSType:   img.RootFS.Type,
		Config: shardsConfig{
			User:         c.User,
			ExposedPorts: shardsKeys(c.ExposedPorts),
			Env:          c.Env,
			Entrypoint:   c.Entrypoint,
			Cmd:          c.Cmd,
			Volumes:      shardsKeys(c.Volumes),
			WorkingDir:   c.WorkingDir,
			Labels:       c.Labels,
			StopSignal:   c.StopSignal,
			ArgsEscaped:  c.ArgsEscaped, //nolint:staticcheck // the image's, as moby keeps it
			OnBuild:      c.OnBuild,
			Shell:        c.Shell,
		},
	}
	out.DiffIDs = shardsDiffIDs(img.RootFS)
	out.History = shardsHistories(img.History)
	if h := c.Healthcheck; h != nil {
		out.Config.Healthcheck = &shardsHealth{
			Test:          h.Test,
			Interval:      int64(h.Interval),
			Timeout:       int64(h.Timeout),
			StartPeriod:   int64(h.StartPeriod),
			StartInterval: int64(h.StartInterval),
			Retries:       h.Retries,
		}
	}
	ports := []string{}
	for p := range dockerOCIImageConfigToContainerConfig(c).ExposedPorts {
		ports = append(ports, p.String())
	}
	sort.Strings(ports)
	out.Ports = ports
	return out
}

// Each field of the config, a value it takes, and its name in the document.
var shardsFields = []struct{ key, value string }{
	{"User", `"u1"`},
	{"ExposedPorts", `{"80/tcp":{}}`},
	{"Env", `["A=1"]`},
	{"Entrypoint", `["/e"]`},
	{"Cmd", `["c"]`},
	{"Volumes", `{"/v":{}}`},
	{"WorkingDir", `"/w"`},
	{"Labels", `{"l":"1"}`},
	{"StopSignal", `"SIGINT"`},
	{"ArgsEscaped", `true`},
	{"Healthcheck", `{"Test":["CMD","t"],"Interval":1}`},
	{"OnBuild", `["RUN x"]`},
	{"Shell", `["/bin/sh","-c"]`},
}

var shardsTop = []struct{ key, value string }{
	{"created", `"2024-01-02T03:04:05Z"`},
	{"author", `"a"`},
	{"architecture", `"arm64"`},
	{"os", `"linux"`},
	{"os.version", `"10.0"`},
	{"os.features", `["f"]`},
	{"variant", `"v8"`},
	{"rootfs", `{"type":"layers","diff_ids":["sha256:aa"]}`},
	{"history", `[{"created_by":"b"}]`},
	{"config", `{"User":"u"}`},
}

func shardsCases() []shardsCase {
	cases := []shardsCase{
		{"empty object", `{}`},
		{"null", `null`},
		{"whitespace and null", " \n\tnull \n"},
		{"empty input", ``},
		{"whitespace alone", "  \n "},
		{"array", `[]`},
		{"string", `"x"`},
		{"number", `5`},
		{"true", `true`},
		{"truncated", `{"config":{`},
		{"trailing", `{} x`},
		{"trailing comma", `{"a":1,}`},
		{"leading zero", `{"config":{"Healthcheck":{"Retries":01}}}`},
		{"full", `{"created":"2024-01-02T03:04:05.123456789Z","author":"me","architecture":"arm64","os":"linux","os.version":"10.0.1","os.features":["win32k"],"variant":"v8",` +
			`"config":{"User":"app:staff","ExposedPorts":{"80/tcp":{},"53/udp":{}},"Env":["PATH=/bin","A=1"],"Entrypoint":["/bin/testguest"],"Cmd":["report"],` +
			`"Volumes":{"/data":{},"/logs":{}},"WorkingDir":"/work","Labels":{"a":"1","b":""},"StopSignal":"SIGQUIT","ArgsEscaped":true,` +
			`"Healthcheck":{"Test":["CMD-SHELL","true"],"Interval":30000000000,"Timeout":5000000000,"StartPeriod":1,"StartInterval":2,"Retries":3},` +
			`"OnBuild":["RUN make"],"Shell":["/bin/bash","-c"]},` +
			`"rootfs":{"type":"layers","diff_ids":["sha256:aa","sha256:bb"]},` +
			`"history":[{"created":"2024-01-02T03:04:05Z","created_by":"COPY . /","author":"x","comment":"buildkit.dockerfile.v0","empty_layer":true},{"created_by":"RUN y"}]}`},
		// Keys: matched exactly, else folded; the last of the keys that match a field wins.
		{"User then user", `{"config":{"User":"root","user":"nobody"}}`},
		{"user then User", `{"config":{"user":"nobody","User":"root"}}`},
		{"USER alone", `{"config":{"USER":"x"}}`},
		{"env alone", `{"config":{"env":["X=1"]}}`},
		{"Env then env", `{"config":{"Env":[],"env":["X"]}}`},
		{"Config", `{"Config":{"User":"u"}}`},
		{"CONFIG then config", `{"CONFIG":{"User":"a"},"config":{"Env":["E=1"]}}`},
		{"Architecture after architecture", `{"architecture":"amd64","Architecture":"arm64"}`},
		{"OS.VERSION", `{"OS.VERSION":"1"}`},
		{"RootFS", `{"RootFS":{"Type":"layers","DIFF_IDS":["sha256:aa"]}}`},
		{"History", `{"History":[{"Created_By":"x","EMPTY_LAYER":true}]}`},
		{"long s in StopSignal", "{\"config\":{\"ſtopSignal\":\"SIGTERM\"}}"},
		{"Kelvin in Healthcheck", "{\"config\":{\"HealthKheck\":{\"Retries\":4}}}"},
		{"Kelvin in a key that has no k", "{\"config\":{\"UsKr\":\"x\"}}"},
		{"long s in os.features", "{\"oſ.features\":[\"f\"]}"},
		{"other non-ASCII key", "{\"config\":{\"Usér\":\"x\"}}"},
		{"unknown keys", `{"foo":1,"config":{"Bar":[1,{"x":null}],"User":"u"},"rootfs":{"zz":true}}`},
		// Repeated keys decode into what an earlier one left.
		{"User twice", `{"config":{"User":"a","User":"b"}}`},
		{"User then null", `{"config":{"User":"a","User":null}}`},
		{"Env shorter later", `{"config":{"Env":["A","B"],"Env":[null]}}`},
		{"Env null element", `{"config":{"Env":["A",null,"C"]}}`},
		{"Env shorter then longer", `{"config":{"Env":["A","B"],"Env":[null],"Env":[null,null]}}`},
		{"Env empty then null element", `{"config":{"Env":["A","B"],"Env":[],"Env":[null]}}`},
		{"Env null then null element", `{"config":{"Env":["A","B"],"Env":null,"Env":[null]}}`},
		{"Env longer later", `{"config":{"Env":["A"],"Env":[null,"B",null]}}`},
		{"Env empty", `{"config":{"Env":[]}}`},
		{"Env null", `{"config":{"Env":null}}`},
		{"Cmd shorter later", `{"config":{"Cmd":["a","b","c"],"Cmd":[null,"x"]}}`},
		{"Entrypoint twice", `{"config":{"Entrypoint":["a"],"Entrypoint":["b","c"]}}`},
		{"Shell shorter later", `{"config":{"Shell":["/bin/sh","-c"],"Shell":[null]}}`},
		{"OnBuild across configs", `{"config":{"OnBuild":["A","B"]},"config":{"OnBuild":[null]}}`},
		{"Labels merged", `{"config":{"Labels":{"a":"1"},"Labels":{"b":"2","a":"3"}}}`},
		{"Labels then null", `{"config":{"Labels":{"a":"1"},"Labels":null}}`},
		{"Labels null value", `{"config":{"Labels":{"a":null}}}`},
		{"Labels empty", `{"config":{"Labels":{}}}`},
		{"Labels null", `{"config":{"Labels":null}}`},
		{"Volumes merged", `{"config":{"Volumes":{"/a":{}},"Volumes":{"/b":null}}}`},
		{"Volumes then null", `{"config":{"Volumes":{"/a":{}},"Volumes":null}}`},
		{"Volumes empty", `{"config":{"Volumes":{}}}`},
		{"Volumes struct with fields", `{"config":{"Volumes":{"/a":{"x":1}}}}`},
		{"ExposedPorts merged", `{"config":{"ExposedPorts":{"80/tcp":{}},"ExposedPorts":{"53/udp":{}}}}`},
		{"configs merged", `{"config":{"User":"a","Env":["E"]},"config":{"WorkingDir":"/w"}}`},
		{"config then null", `{"config":{"User":"a"},"config":null}`},
		{"config null", `{"config":null}`},
		{"Healthcheck merged", `{"config":{"Healthcheck":{"Test":["CMD","a"],"Retries":3},"Healthcheck":{"Interval":5}}}`},
		{"Healthcheck then null", `{"config":{"Healthcheck":{"Retries":3},"Healthcheck":null}}`},
		{"Healthcheck empty", `{"config":{"Healthcheck":{}}}`},
		{"Healthcheck Test shorter later", `{"config":{"Healthcheck":{"Test":["CMD","a","b"]},"Healthcheck":{"Test":[null]}}}`},
		{"Healthcheck Test null", `{"config":{"Healthcheck":{"Test":null}}}`},
		{"history merged", `{"history":[{"created_by":"a","comment":"c","empty_layer":true}],"history":[{"created_by":"b"}]}`},
		{"history longer later", `{"history":[{"created_by":"a"}],"history":[{"comment":"x"},{"author":"y"}]}`},
		{"history shorter then longer", `{"history":[{"created_by":"a"},{"created_by":"b"}],"history":[{}],"history":[{},{}]}`},
		{"history empty then longer", `{"history":[{"created_by":"a"}],"history":[],"history":[{}]}`},
		{"history null element", `{"history":[{"created_by":"a"}],"history":[null]}`},
		{"history empty", `{"history":[]}`},
		{"history null", `{"history":null}`},
		{"rootfs merged", `{"rootfs":{"type":"layers","diff_ids":["sha256:aa","sha256:bb"]},"rootfs":{"diff_ids":[null]}}`},
		{"rootfs diff_ids null", `{"rootfs":{"type":"layers","diff_ids":null}}`},
		{"rootfs diff_ids empty", `{"rootfs":{"type":"layers","diff_ids":[]}}`},
		{"rootfs null", `{"rootfs":null}`},
		{"os.features shorter later", `{"os.features":["a","b"],"os.features":[null]}`},
		{"created twice", `{"created":"2024-01-01T00:00:00Z","created":"2025-01-01T00:00:00Z"}`},
		{"created then null", `{"created":"2024-01-01T00:00:00Z","created":null}`},
		// Times: Time.UnmarshalJSON reads the text as written, and its error ends the reading.
		{"created with an offset", `{"created":"2024-01-02T03:04:05.5+01:30"}`},
		{"created trailing zeros", `{"created":"2024-01-02T03:04:05.100000000Z"}`},
		{"created escaped", `{"created":"2024-01-02T03:04:05\u005a"}`},
		{"created escaped invalid", `{"created":"2024-01-02T03:04:05\u0058"}`},
		{"created escaped digit", `{"created":"2024-01-02T03:04:0\u0035Z"}`},
		{"created number", `{"created":5}`},
		{"created empty", `{"created":""}`},
		{"created bad day", `{"created":"2024-02-30T00:00:00Z"}`},
		{"created leap second", `{"created":"2024-12-31T23:59:60Z"}`},
		{"created lowercase", `{"created":"2024-01-02t03:04:05z"}`},
		{"created no zone", `{"created":"2024-01-02T03:04:05"}`},
		{"created year 0", `{"created":"0000-01-01T00:00:00Z"}`},
		{"created hour offset 24", `{"created":"2024-01-02T03:04:05+24:00"}`},
		{"history created bad", `{"history":[{"created":"x"}]}`},
		{"history created number", `{"history":[{"created":1}]}`},
		{"type error then bad created", `{"config":{"User":5},"created":"bad"}`},
		{"bad created then type error", `{"created":"bad","config":{"User":5}}`},
		// Type errors: the first is kept, the rest read.
		{"two type errors", `{"config":{"User":5,"Env":"x"}}`},
		{"type error then fields", `{"config":{"User":5,"WorkingDir":"/w"}}`},
		{"config array", `{"config":[]}`},
		{"config string", `{"config":"x"}`},
		{"rootfs string", `{"rootfs":"x"}`},
		{"history object", `{"history":{}}`},
		{"history number element", `{"history":[1]}`},
		{"history element type error", `{"history":[{"created_by":5}]}`},
		{"diff_ids string", `{"rootfs":{"diff_ids":"x"}}`},
		{"diff_ids number element", `{"rootfs":{"diff_ids":[5]}}`},
		{"rootfs type number", `{"rootfs":{"type":5}}`},
		{"Labels array", `{"config":{"Labels":[]}}`},
		{"Labels number value", `{"config":{"Labels":{"a":1}}}`},
		{"Volumes number value", `{"config":{"Volumes":{"/a":1}}}`},
		{"Volumes array value", `{"config":{"Volumes":{"/a":[]}}}`},
		{"ExposedPorts string", `{"config":{"ExposedPorts":"80"}}`},
		{"Healthcheck array", `{"config":{"Healthcheck":[]}}`},
		{"Healthcheck string Interval", `{"config":{"Healthcheck":{"Interval":"5s"}}}`},
		{"Healthcheck fraction Interval", `{"config":{"Healthcheck":{"Interval":1.5}}}`},
		{"Healthcheck exponent Retries", `{"config":{"Healthcheck":{"Retries":1e2}}}`},
		{"Healthcheck point zero Retries", `{"config":{"Healthcheck":{"Retries":3.0}}}`},
		{"Healthcheck negative Retries", `{"config":{"Healthcheck":{"Retries":-1}}}`},
		{"Healthcheck huge Retries", `{"config":{"Healthcheck":{"Retries":9223372036854775808}}}`},
		{"Healthcheck largest Interval", `{"config":{"Healthcheck":{"Interval":9223372036854775807}}}`},
		{"Healthcheck smallest Timeout", `{"config":{"Healthcheck":{"Timeout":-9223372036854775808}}}`},
		{"Healthcheck minus zero", `{"config":{"Healthcheck":{"Retries":-0}}}`},
		{"Healthcheck Test string", `{"config":{"Healthcheck":{"Test":"x"}}}`},
		{"Healthcheck Test number element", `{"config":{"Healthcheck":{"Test":[1]}}}`},
		{"ArgsEscaped string", `{"config":{"ArgsEscaped":"yes"}}`},
		{"ArgsEscaped null", `{"config":{"ArgsEscaped":true,"ArgsEscaped":null}}`},
		{"os.features string", `{"os.features":"f"}`},
		{"architecture number", `{"architecture":5}`},
		{"author array", `{"author":[]}`},
		// The container config keeps the ports network.ParsePort takes.
		{"exposed ports parsed", `{"config":{"ExposedPorts":{"80/tcp":{},"53/udp":{},"x":{},"80/xyz":{},"0/tcp":{},"8080-8090/tcp":{},"65535":{},"65536":{},"80/TCP":{},"/tcp":{},"1-0/tcp":{},"443/sctp":{}," 80":{},"80 ":{}}}}`},
		// Strings as Go decodes them.
		{"escapes", `{"config":{"User":"a\"b\\c\/d\b\f\n\r\t\u00e9\ud83d\ude00"}}`},
		{"lone surrogate", `{"config":{"User":"\ud800x"}}`},
		{"low surrogate first", `{"config":{"User":"\udc00"}}`},
		{"invalid UTF-8", "{\"config\":{\"User\":\"a\xffb\xc3\"}}"},
		{"NUL escape", `{"config":{"User":"a\u0000b"}}`},
		{"raw control character", "{\"config\":{\"User\":\"a\x01b\"}}"},
		{"bad escape", `{"config":{"User":"\x"}}`},
		{"invalid UTF-8 key", "{\"config\":{\"Us\xffr\":\"x\"}}"},
	}
	for _, f := range shardsFields {
		lower, upper := strings.ToLower(f.key), strings.ToUpper(f.key)
		cases = append(cases,
			shardsCase{f.key + " exact", fmt.Sprintf(`{"config":{%q:%s}}`, f.key, f.value)},
			shardsCase{f.key + " lower", fmt.Sprintf(`{"config":{%q:%s}}`, lower, f.value)},
			shardsCase{f.key + " upper", fmt.Sprintf(`{"config":{%q:%s}}`, upper, f.value)},
		)
		for _, wrong := range []string{`5`, `"s"`, `[1]`, `{"x":1}`, `true`, `null`, `[]`, `{}`} {
			cases = append(cases, shardsCase{
				fmt.Sprintf("%s as %s", f.key, wrong),
				fmt.Sprintf(`{"config":{%q:%s}}`, f.key, wrong),
			})
		}
	}
	for _, f := range shardsTop {
		cases = append(cases,
			shardsCase{f.key + " exact", fmt.Sprintf(`{%q:%s}`, f.key, f.value)},
			shardsCase{f.key + " upper", fmt.Sprintf(`{%q:%s}`, strings.ToUpper(f.key), f.value)},
		)
		for _, wrong := range []string{`5`, `"s"`, `[1]`, `{"x":1}`, `true`, `[]`, `{}`} {
			cases = append(cases, shardsCase{
				fmt.Sprintf("%s as %s", f.key, wrong),
				fmt.Sprintf(`{%q:%s}`, f.key, wrong),
			})
		}
	}
	// Nesting: Go's scanner takes 10000 levels and refuses the 10001st.
	for _, depth := range []int{9999, 10000, 10001} {
		cases = append(cases, shardsCase{
			fmt.Sprintf("unknown field %d deep", depth),
			`{"x":` + strings.Repeat("[", depth-1) + strings.Repeat("]", depth-1) + `}`,
		})
	}
	return cases
}

func TestShardsImageConfig(t *testing.T) {
	var records []shardsRecord
	for _, c := range shardsCases() {
		var img dockerspec.DockerOCIImage
		r := shardsRecord{Name: c.name, Config: base64.StdEncoding.EncodeToString([]byte(c.config))}
		if err := json.Unmarshal([]byte(c.config), &img); err != nil {
			r.Error = err.Error()
		} else {
			r.Image = shardsDump(img)
		}
		var oci ocispec.Image
		if err := json.Unmarshal([]byte(c.config), &oci); err != nil {
			r.OCIError = err.Error()
		} else {
			r.OCIImage = shardsDumpOCI(oci)
		}
		// image_history.go's own type.
		var ociImage struct {
			RootFS  ocispec.RootFS    `json:"rootfs"`
			History []ocispec.History `json:"history,omitempty"`
		}
		if err := json.Unmarshal([]byte(c.config), &ociImage); err != nil {
			r.HistoryError = err.Error()
		} else {
			r.History = &shardsHistoryImage{
				RootFSType: ociImage.RootFS.Type,
				DiffIDs:    shardsDiffIDs(ociImage.RootFS),
				History:    shardsHistories(ociImage.History),
			}
		}
		records = append(records, r)
	}
	out, err := json.MarshalIndent(map[string]any{
		"moby":    "464cd50c3d9e92877d56940ea160de6fca7bea23",
		"cases":   records,
	}, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_IMAGE_CONFIG_OUT"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
