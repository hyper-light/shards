// What BuildKit's Dockerfile parser and shell lexer make of a corpus, for
// crates/dockerfile's tests to expect byte for byte. `generate`, beside this directory,
// runs it inside a moby/buildkit checkout at the pinned tag, whose packages it imports.
// Every string it writes is Go-quoted (strconv.Quote), so that any bytes, valid UTF-8 or
// not, compare exactly.
//
//	go run ./shards-oracle TESTDATA
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"google.golang.org/protobuf/proto"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"unicode"

	"github.com/docker/go-units"
	"github.com/moby/buildkit/client/llb"
	"github.com/moby/buildkit/client/llb/sourceresolver"
	"github.com/moby/buildkit/frontend/dockerfile/dfgitutil"
	"github.com/moby/buildkit/frontend/dockerfile/dockerfile2llb"
	"github.com/moby/buildkit/frontend/dockerfile/instructions"
	"github.com/moby/buildkit/frontend/dockerfile/linter"
	"github.com/moby/buildkit/frontend/dockerfile/parser"
	"github.com/moby/buildkit/frontend/dockerfile/shell"
	"github.com/moby/buildkit/frontend/dockerui"
	gwclient "github.com/moby/buildkit/frontend/gateway/client"
	gwpb "github.com/moby/buildkit/frontend/gateway/pb"
	"github.com/moby/buildkit/frontend/subrequests"
	"github.com/moby/buildkit/frontend/subrequests/outline"
	"github.com/moby/buildkit/frontend/subrequests/targets"
	"github.com/moby/buildkit/solver/pb"
	"github.com/moby/buildkit/util/gitutil"
	dockerspec "github.com/moby/docker-image-spec/specs-go/v1"
	"github.com/moby/patternmatcher"
	"github.com/moby/patternmatcher/ignorefile"
	digest "github.com/opencontainers/go-digest"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
	fstypes "github.com/tonistiigi/fsutil/types"
	"github.com/tonistiigi/go-csvvalue"
	"google.golang.org/protobuf/encoding/protojson"
)

func q(s string) string { return strconv.Quote(s) }

func qs(ss []string) []string {
	out := make([]string, 0, len(ss))
	for _, s := range ss {
		out = append(out, q(s))
	}
	return out
}

type heredoc struct {
	Name    string `json:"name"`
	Fd      uint   `json:"fd"`
	Expand  bool   `json:"expand"`
	Chomp   bool   `json:"chomp"`
	Content string `json:"content"`
}

type child struct {
	Dump        string    `json:"dump"`
	Original    string    `json:"original"`
	Flags       []string  `json:"flags"`
	Attributes  []string  `json:"attributes"`
	StartLine   int       `json:"start_line"`
	EndLine     int       `json:"end_line"`
	PrevComment []string  `json:"prev_comment"`
	Heredocs    []heredoc `json:"heredocs"`
}

type parsed struct {
	File     string     `json:"file"`
	Escape   string     `json:"escape,omitempty"`
	Dump     string     `json:"dump,omitempty"`
	Children []child    `json:"children,omitempty"`
	Warnings []string   `json:"warnings,omitempty"`
	Error    string     `json:"error,omitempty"`
	Location [][][2]int `json:"location,omitempty"`
}

func parseFile(root, rel string) parsed {
	data, err := os.ReadFile(filepath.Join(root, rel))
	if err != nil {
		panic(err)
	}
	out := parsed{File: rel}
	res, err := parser.Parse(bytes.NewReader(data))
	if err != nil {
		out.Error = q(err.Error())
		var le *parser.LocationError
		if errors.As(err, &le) {
			for _, rs := range le.Locations {
				var lines [][2]int
				for _, r := range rs {
					lines = append(lines, [2]int{r.Start.Line, r.End.Line})
				}
				out.Location = append(out.Location, lines)
			}
		}
		return out
	}
	out.Escape = q(string(res.EscapeToken))
	out.Dump = q(res.AST.Dump())
	for _, w := range res.Warnings {
		out.Warnings = append(out.Warnings, q(fmt.Sprintf("%s|%s|%d", w.Short, w.URL, w.Location.Start.Line)))
	}
	for _, n := range res.AST.Children {
		c := child{
			Dump:        q(n.Dump()),
			Original:    q(n.Original),
			Flags:       qs(n.Flags),
			StartLine:   n.StartLine,
			EndLine:     n.EndLine,
			PrevComment: qs(n.PrevComment),
		}
		for k, v := range n.Attributes {
			if v {
				c.Attributes = append(c.Attributes, k)
			}
		}
		sort.Strings(c.Attributes)
		for _, h := range n.Heredocs {
			c.Heredocs = append(c.Heredocs, heredoc{q(h.Name), h.FileDescriptor, h.Expand, h.Chomp, q(h.Content)})
		}
		out.Children = append(out.Children, c)
	}
	return out
}

// What BuildKit's instructions make of a parsed file: its stages and the ARGs before them,
// with the lint warnings they give, or its error. The linter is set up as
// dockerfile2llb sets it up, from the file's check directive.
func instructionsFile(root, rel string) map[string]any {
	data, err := os.ReadFile(filepath.Join(root, rel))
	if err != nil {
		panic(err)
	}
	out := map[string]any{"file": rel}
	res, err := parser.Parse(bytes.NewReader(data))
	if err != nil {
		out["parse_error"] = true
		return out
	}
	var warnings []string
	checkStr, _, _, _ := parser.ParseDirective("check", data)
	cfg, err := linter.ParseLintOptions(checkStr)
	if err != nil {
		out["error"] = q("failed to parse check options: " + err.Error())
		return out
	}
	cfg.Warn = func(rule, desc, url, msg string, loc []parser.Range) {
		var lines []string
		for _, r := range loc {
			lines = append(lines, fmt.Sprintf("%d-%d", r.Start.Line, r.End.Line))
		}
		warnings = append(warnings, q(fmt.Sprintf("%s|%s|%s|%s|%s", rule, desc, url, msg, strings.Join(lines, ","))))
	}
	lint := linter.New(cfg)
	stages, metaArgs, err := instructions.Parse(res.AST, lint)
	out["warnings"] = warnings
	if err != nil {
		out["error"] = q(err.Error())
		var le *parser.LocationError
		if errors.As(err, &le) {
			var locs [][][2]int
			for _, rs := range le.Locations {
				var lines [][2]int
				for _, r := range rs {
					lines = append(lines, [2]int{r.Start.Line, r.End.Line})
				}
				locs = append(locs, lines)
			}
			out["location"] = locs
		}
		return out
	}
	if lerr := lint.Error(); lerr != nil {
		out["lint_error"] = true
	}
	var meta []any
	for _, a := range metaArgs {
		meta = append(meta, command(&a))
	}
	out["meta_args"] = meta
	var st []any
	for _, s := range stages {
		var cmds []any
		for _, c := range s.Commands {
			cmds = append(cmds, command(c))
		}
		st = append(st, map[string]any{
			"name": q(s.Name), "orig_cmd": q(s.OrigCmd), "base_name": q(s.BaseName), "platform": q(s.Platform),
			"doc_comment": q(s.DocComment), "source_code": q(s.SourceCode), "location": ranges(s.Location),
			"comments": qs(s.Comments), "commands": cmds,
		})
	}
	out["stages"] = st
	return out
}

func ranges(rs []parser.Range) [][2]int {
	out := [][2]int{}
	for _, r := range rs {
		out = append(out, [2]int{r.Start.Line, r.End.Line})
	}
	return out
}

func kvps(kv instructions.KeyValuePairs) []any {
	out := []any{}
	for _, p := range kv {
		out = append(out, []any{q(p.Key), q(p.Value), p.NoDelim})
	}
	return out
}

func optq(s *string) any {
	if s == nil {
		return nil
	}
	return q(*s)
}

func optb(b *bool) any {
	if b == nil {
		return nil
	}
	return *b
}

func optu(u *uint64) any {
	if u == nil {
		return nil
	}
	return *u
}

func shellCmd(c instructions.ShellDependantCmdLine) map[string]any {
	var files []any
	for _, f := range c.Files {
		files = append(files, []any{q(f.Name), q(f.Data), f.Chomp})
	}
	cl := []string{}
	if c.CmdLine != nil {
		cl = qs(c.CmdLine)
	}
	return map[string]any{"cmd_line": cl, "cmd_line_nil": c.CmdLine == nil, "files": files, "prepend_shell": c.PrependShell}
}

func sources(s instructions.SourcesAndDest) map[string]any {
	var contents []any
	for _, c := range s.SourceContents {
		contents = append(contents, []any{q(c.Path), q(c.Data), c.Expand})
	}
	return map[string]any{"dest": q(s.DestPath), "paths": qs(s.SourcePaths), "contents": contents}
}

// One command, its kind and fields.
func command(c any) map[string]any {
	type common interface {
		Name() string
		Location() []parser.Range
		Comments() []string
		String() string
	}
	out := map[string]any{"kind": fmt.Sprintf("%T", c)}
	if cc, ok := c.(common); ok {
		out["name"] = q(cc.Name())
		out["code"] = q(cc.String())
		out["location"] = ranges(cc.Location())
		out["comments"] = qs(cc.Comments())
	}
	switch c := c.(type) {
	case *instructions.EnvCommand:
		out["env"] = kvps(c.Env)
	case *instructions.MaintainerCommand:
		out["maintainer"] = q(c.Maintainer)
	case *instructions.LabelCommand:
		out["labels"] = kvps(c.Labels)
	case *instructions.AddCommand:
		out["sources"] = sources(c.SourcesAndDest)
		out["chown"], out["chmod"], out["link"] = q(c.Chown), q(c.Chmod), c.Link
		out["exclude"], out["keep_git_dir"], out["checksum"], out["unpack"] = qs(c.ExcludePatterns), optb(c.KeepGitDir), q(c.Checksum), optb(c.Unpack)
	case *instructions.CopyCommand:
		out["sources"] = sources(c.SourcesAndDest)
		out["from"], out["chown"], out["chmod"], out["link"] = q(c.From), q(c.Chown), q(c.Chmod), c.Link
		out["exclude"], out["parents"] = qs(c.ExcludePatterns), c.Parents
	case *instructions.OnbuildCommand:
		out["expression"] = q(c.Expression)
	case *instructions.WorkdirCommand:
		out["path"] = q(c.Path)
	case *instructions.RunCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
		used := append([]string(nil), c.FlagsUsed...)
		sort.Strings(used)
		out["flags_used"] = qs(used)
		var mounts []any
		for _, m := range instructions.GetMounts(c) {
			mounts = append(mounts, map[string]any{
				"type": q(string(m.Type)), "from": q(m.From), "source": q(m.Source), "target": q(m.Target),
				"read_only": m.ReadOnly, "size": m.SizeLimit, "id": q(m.CacheID), "sharing": q(string(m.CacheSharing)),
				"required": m.Required, "env": optq(m.Env), "mode": optu(m.Mode), "uid": optu(m.UID), "gid": optu(m.GID),
			})
		}
		out["mounts"] = mounts
		out["network"] = q(instructions.GetNetwork(c))
		out["security"] = q(instructions.GetSecurity(c))
		var devices []any
		for _, d := range instructions.GetDevices(c) {
			devices = append(devices, []any{q(d.Name), d.Required})
		}
		out["devices"] = devices
	case *instructions.CmdCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
	case *instructions.EntrypointCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
	case *instructions.HealthCheckCommand:
		h := c.Health
		out["health"] = map[string]any{"test": qs(h.Test), "interval": int64(h.Interval), "timeout": int64(h.Timeout),
			"start_period": int64(h.StartPeriod), "start_interval": int64(h.StartInterval), "retries": h.Retries}
	case *instructions.ExposeCommand:
		out["ports"] = qs(c.Ports)
	case *instructions.UserCommand:
		out["user"] = q(c.User)
	case *instructions.VolumeCommand:
		out["volumes"] = qs(c.Volumes)
	case *instructions.StopSignalCommand:
		out["signal"] = q(c.Signal)
	case *instructions.ArgCommand:
		var args []any
		for _, a := range c.Args {
			args = append(args, []any{q(a.Key), optq(a.Value), q(a.DocComment)})
		}
		out["args"] = args
	case *instructions.ShellCommand:
		out["shell_words"] = qs(c.Shell)
	}
	return out
}

// The base images a plan may use, by the reference BuildKit resolves (testdata/images.json).
type fakeImage struct {
	Ref    string          `json:"ref"`
	Digest string          `json:"digest"`
	Config json.RawMessage `json:"config"`
}

type resolver map[string]fakeImage

func (r resolver) ResolveImageConfig(_ context.Context, ref string, _ sourceresolver.Opt) (string, digest.Digest, []byte, error) {
	img, ok := r[ref]
	if !ok {
		return "", "", nil, fmt.Errorf("%s: not found", ref)
	}
	return img.Ref, digest.Digest(img.Digest), img.Config, nil
}

// A plan's options, from FILE.opts.json beside a corpus file.
type planOpts struct {
	BuildArgs map[string]string `json:"build_args"`
	Target    string            `json:"target"`
	Labels    map[string]string `json:"labels"`
	Hostname  string            `json:"hostname"`
	// The frontend's `ulimit` option, as buildx sends --ulimit's values.
	Ulimit string `json:"ulimit"`
	// The frontend's named contexts (`context:NAME` options, as buildx sends
	// --build-context's) and their `sharedkey:localdir:NAME` keys.
	Contexts   map[string]string `json:"contexts"`
	SharedKeys map[string]string `json:"shared_keys"`
	// The frontend's `no-cache` option, as buildx sends --no-cache-filter's stages (or
	// "" for --no-cache).
	NoCache *string `json:"no_cache"`
	// The frontend's other options, as buildx sends them (add-hosts, shm-size,
	// cgroup-parent, force-network-mode, memory and the like), read by dockerui; among
	// them `context`, a Git or HTTP URL that is the build's context.
	Frontend map[string]string `json:"frontend"`
	// What an HTTP context's download begins with (base64), which dockerui reads to
	// tell an archive from a Dockerfile.
	HTTPContext string `json:"http_context"`
}

// A gateway as the frontend sees one, of a build given only named contexts: its options,
// the fake images for ResolveImageConfig, and a context with no .dockerignore. Anything
// else the frontend asks of it is a nil interface's panic: the plan asks nothing else.
type gateway struct {
	gwclient.Client
	opts   gwclient.BuildOpts
	images resolver
	// An HTTP context's download.
	download []byte
}

func (g *gateway) BuildOpts() gwclient.BuildOpts { return g.opts }

func (g *gateway) ResolveImageConfig(ctx context.Context, ref string, opt sourceresolver.Opt) (string, digest.Digest, []byte, error) {
	return g.images.ResolveImageConfig(ctx, ref, opt)
}

func (g *gateway) Solve(context.Context, gwclient.SolveRequest) (*gwclient.Result, error) {
	res := gwclient.NewResult()
	if g.download != nil {
		res.SetRef(downloaded{data: g.download})
		return res, nil
	}
	res.SetRef(noFiles{})
	return res, nil
}

// A solved HTTP source: its one file, `context`, read up to a range's length.
type downloaded struct {
	gwclient.Reference
	data []byte
}

func (d downloaded) ReadFile(_ context.Context, r gwclient.ReadRequest) ([]byte, error) {
	if r.Filename != "context" {
		return nil, os.ErrNotExist
	}
	if r.Range != nil && int(r.Range.Length) < len(d.data) {
		return d.data[:r.Range.Length], nil
	}
	return d.data, nil
}

// The frontend's inputs: none, as buildx gives none for these contexts.
func (g *gateway) Inputs(context.Context) (map[string]llb.State, error) {
	return map[string]llb.State{}, nil
}

// A solved local source holding none of the files asked for.
type noFiles struct{ gwclient.Reference }

func (noFiles) StatFile(context.Context, gwclient.StatRequest) (*fstypes.Stat, error) {
	return nil, os.ErrNotExist
}

func (noFiles) ReadFile(context.Context, gwclient.ReadRequest) ([]byte, error) {
	return nil, os.ErrNotExist
}

// parseUlimits reads the frontend's `ulimit` option as dockerui's own (unexported)
// parseUlimits does: CSV fields, each read by go-units.
func parseUlimits(v string) ([]*pb.Ulimit, error) {
	if v == "" {
		return nil, nil
	}
	fields, err := csvvalue.Fields(v, nil)
	if err != nil {
		return nil, err
	}
	out := make([]*pb.Ulimit, 0)
	for _, field := range fields {
		u, err := units.ParseUlimit(field)
		if err != nil {
			return nil, err
		}
		out = append(out, &pb.Ulimit{Name: u.Name, Soft: u.Soft, Hard: u.Hard})
	}
	return out, nil
}

// The options BuildKit's frontend gives Dockerfile2LLB for a file of the corpus: its
// .opts.json read as buildx and dockerui pass them, the fake images, and `warn` for the
// checks' warnings.
func convertOpt(root, rel string, images resolver, warn func(string)) ([]byte, dockerfile2llb.ConvertOpt) {
	data, err := os.ReadFile(filepath.Join(root, rel))
	if err != nil {
		panic(err)
	}
	var opts planOpts
	if o, err := os.ReadFile(filepath.Join(root, rel+".opts.json")); err == nil {
		if err := json.Unmarshal(o, &opts); err != nil {
			panic(err)
		}
	}
	ulimits, err := parseUlimits(opts.Ulimit)
	if err != nil {
		panic(err)
	}
	platform := ocispecs.Platform{OS: "linux", Architecture: "amd64"}
	caps := pb.Caps.CapSet(pb.Caps.All())
	var named *dockerui.Client
	if len(opts.Contexts) > 0 || opts.NoCache != nil || len(opts.Frontend) > 0 {
		bopts := gwclient.BuildOpts{Opts: map[string]string{}, LLBCaps: caps, Caps: gwpb.Caps.CapSet(gwpb.Caps.All())}
		for k, v := range opts.Frontend {
			bopts.Opts[k] = v
		}
		if opts.NoCache != nil {
			bopts.Opts["no-cache"] = *opts.NoCache
		}
		for k, v := range opts.Contexts {
			bopts.Opts["context:"+k] = v
		}
		for k, v := range opts.SharedKeys {
			bopts.Opts["sharedkey:localdir:"+k] = v
		}
		var download []byte
		if opts.HTTPContext != "" {
			download, err = base64.StdEncoding.DecodeString(opts.HTTPContext)
			if err != nil {
				panic(err)
			}
		}
		named, err = dockerui.NewClient(&gateway{opts: bopts, images: images, download: download})
		if err != nil {
			panic(err)
		}
	}
	cfg := dockerui.Config{
		BuildArgs:      opts.BuildArgs,
		Target:         opts.Target,
		Labels:         opts.Labels,
		Hostname:       opts.Hostname,
		Ulimits:        ulimits,
		BuildPlatforms: []ocispecs.Platform{platform},
	}
	// What dockerui made of the frontend's options.
	if named != nil {
		cfg.ExtraHosts = named.Config.ExtraHosts
		cfg.ShmSize = named.Config.ShmSize
		cfg.CgroupParent = named.Config.CgroupParent
		cfg.NetworkMode = named.Config.NetworkMode
		cfg.LinuxResources = named.Config.LinuxResources
		cfg.ImageResolveMode = named.Config.ImageResolveMode
	}
	return data, dockerfile2llb.ConvertOpt{
		Client:         named,
		Config:         cfg,
		TargetPlatform: &platform,
		MetaResolver:   images,
		LLBCaps:        &caps,
		Warn: func(rule, desc, url, msg string, loc []parser.Range) {
			if warn != nil {
				warn(q(fmt.Sprintf("%s|%s|%s", rule, msg, rangesText(loc))))
			}
		},
	}
}

// What BuildKit's frontend answers a file's subrequests with (dockerui's
// HandleSubrequest): the outline of its target, as Dockerfile2Outline makes it, and its
// stages, as ListTargets lists them, each its result.json and the text buildx prints of
// it (printValue: PrintOutline, PrintTargets), or its error.
func subrequestsFile(root, rel string, images resolver) map[string]any {
	out := map[string]any{"file": rel}
	data, opt := convertOpt(root, rel, images, nil)
	if o, err := dockerfile2llb.Dockerfile2Outline(context.Background(), data, opt); err != nil {
		out["outline_error"] = q(err.Error())
	} else {
		res, err := o.ToResult()
		if err != nil {
			panic(err)
		}
		out["outline"] = q(string(res.Metadata["result.json"]))
		b := bytes.NewBuffer(nil)
		if err := outline.PrintOutline(res.Metadata["result.json"], b); err != nil {
			panic(err)
		}
		out["outline_text"] = q(b.String())
	}
	// ListTargets parses with no linter, which a stage's `check=` comment dereferences:
	// BuildKit's frontend panics there, recorded as such.
	l, err := func() (l *targets.List, err error) {
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("panic: %v", r)
			}
		}()
		return dockerfile2llb.ListTargets(context.Background(), data)
	}()
	if err != nil {
		out["targets_error"] = q(err.Error())
	} else {
		res, err := l.ToResult()
		if err != nil {
			panic(err)
		}
		out["targets"] = q(string(res.Metadata["result.json"]))
		b := bytes.NewBuffer(nil)
		if err := targets.PrintTargets(res.Metadata["result.json"], b); err != nil {
			panic(err)
		}
		out["targets_text"] = q(b.String())
	}
	// The lint subrequest (DockerfileLint), its source the file as dockerui reads it, named
	// as a Dockerfile in the context's root is; its LLB definition, which names a session
	// of one build alone, cleared (testdata/deviations.json).
	lopt := opt
	lopt.SourceMap = llb.NewSourceMap(nil, "Dockerfile", "Dockerfile", data)
	lopt.SourceMap.Definition = &llb.Definition{}
	lr, err := dockerfile2llb.DockerfileLint(context.Background(), data, lopt)
	if err != nil {
		panic(err)
	}
	for _, s := range lr.Sources {
		s.Definition = nil
	}
	res, err := lr.ToResult(nil)
	if err != nil {
		panic(err)
	}
	out["lint"] = q(string(res.Metadata["result.json"]))
	out["lint_status"] = string(res.Metadata["result.statuscode"])
	return out
}

// The subrequests the Dockerfile frontend describes (dockerui's describe, with Outline
// and ListTargets set, as the frontend sets them): result.json and the text buildx
// prints of it.
func describeFile() map[string]any {
	all := []subrequests.Request{
		outline.SubrequestsOutlineDefinition,
		targets.SubrequestsTargetsDefinition,
		subrequests.SubrequestsDescribeDefinition,
	}
	dt, err := json.MarshalIndent(all, "", "  ")
	if err != nil {
		panic(err)
	}
	b := bytes.NewBuffer(nil)
	if err := subrequests.PrintDescribe(dt, b); err != nil {
		panic(err)
	}
	return map[string]any{"json": q(string(dt)), "text": q(b.String())}
}

// What BuildKit's Dockerfile2LLB plans for a file: every operation of its graph, in an
// order each input comes before what uses it, with inputs by that order; each op's
// metadata; the image config; and the build checks' warnings. Or its error.
func planFile(root, rel string, images resolver) map[string]any {
	out := map[string]any{"file": rel}
	var warnings []string
	data, opt := convertOpt(root, rel, images, func(w string) { warnings = append(warnings, w) })
	// The Dockerfile's source map, as dockerui gives a build one: each op's locations.
	opt.SourceMap = llb.NewSourceMap(nil, "Dockerfile", "Dockerfile", data)
	res, err := dockerfile2llb.Dockerfile2LLB(context.Background(), data, opt)
	out["warnings"] = warnings
	if err != nil {
		out["error"] = q(err.Error())
		var le *parser.LocationError
		if errors.As(err, &le) {
			var locs [][][2]int
			for _, rs := range le.Locations {
				locs = append(locs, ranges(rs))
			}
			out["location"] = locs
		}
		return out
	}
	// What an SBOM scanner is given beside the target (D81): each extra by name, as the
	// normalized digest of its definition's root.
	if res.SBOM != nil && len(res.SBOM.Extras) > 0 {
		extras := map[string]any{}
		for name, st := range res.SBOM.Extras {
			d, err := st.Marshal(context.Background())
			if err != nil {
				panic(err)
			}
			extras[name] = normalizedRoot(d.Def)
		}
		out["sbom_extras"] = extras
	}
	img, err := json.Marshal(res.Image)
	if err != nil {
		panic(err)
	}
	// The config's bytes, as BuildKit writes them: their digest is the image's.
	out["image"] = string(img)
	def, err := res.State.Marshal(context.Background())
	if err != nil {
		out["marshal_error"] = q(err.Error())
		return out
	}
	ops := map[digest.Digest]*pb.Op{}
	var last digest.Digest
	for _, dt := range def.Def {
		var op pb.Op
		if err := op.UnmarshalVT(dt); err != nil {
			panic(err)
		}
		d := digest.FromBytes(dt)
		ops[d] = &op
		last = d
	}
	index := map[digest.Digest]int{}
	var order []digest.Digest
	var visit func(d digest.Digest)
	visit = func(d digest.Digest) {
		if _, ok := index[d]; ok {
			return
		}
		for _, in := range ops[d].Inputs {
			visit(digest.Digest(in.Digest))
		}
		index[d] = len(order)
		order = append(order, d)
	}
	// A build of scratch alone marshals no ops.
	if last != "" {
		visit(last)
	}
	list := []any{}
	// Each op as BuildKit marshals it (client/llb deterministicMarshal), its local
	// source's unique ID as "*" and its inputs named by their own ops so marshalled: the
	// bytes crates/dockerfile's protobuf encoding is held to (D80).
	norm := map[digest.Digest]digest.Digest{}
	pbOf := map[digest.Digest][]byte{}
	for _, d := range order {
		op := proto.Clone(ops[d]).(*pb.Op)
		if src := op.GetSource(); src != nil {
			if _, ok := src.Attrs["local.unique"]; ok {
				src.Attrs["local.unique"] = "*"
			}
		}
		for _, in := range op.Inputs {
			in.Digest = string(norm[digest.Digest(in.Digest)])
		}
		b, err := proto.MarshalOptions{Deterministic: true}.Marshal(op)
		if err != nil {
			panic(err)
		}
		norm[d] = digest.FromBytes(b)
		pbOf[d] = b
	}
	for _, d := range order {
		op := ops[d]
		inputs := []any{}
		for _, in := range op.Inputs {
			inputs = append(inputs, []any{index[digest.Digest(in.Digest)], in.Index})
		}
		op.Inputs = nil
		b, err := protojson.MarshalOptions{UseProtoNames: true}.Marshal(op)
		if err != nil {
			panic(err)
		}
		var v map[string]any
		if err := json.Unmarshal(b, &v); err != nil {
			panic(err)
		}
		v["inputs"] = inputs
		v["pb"] = base64.StdEncoding.EncodeToString(pbOf[d])
		// A local source's unique ID and a progress group's ID are random.
		if src, ok := v["source"].(map[string]any); ok {
			if attrs, ok := src["attrs"].(map[string]any); ok {
				if _, ok := attrs["local.unique"]; ok {
					attrs["local.unique"] = "*"
				}
			}
		}
		if md, ok := def.Metadata[d]; ok {
			mb, err := protojson.MarshalOptions{UseProtoNames: true}.Marshal(md.ToPB())
			if err != nil {
				panic(err)
			}
			var mv map[string]any
			if err := json.Unmarshal(mb, &mv); err != nil {
				panic(err)
			}
			delete(mv, "caps")
			if pg, ok := mv["progress_group"].(map[string]any); ok {
				pg["id"] = "*"
			}
			v["metadata"] = mv
		}
		// Where in the Dockerfile the op comes from (the source map, D80): each of its
		// locations, the source's index and its ranges, start and end line and character.
		if def.Source != nil {
			if ls, ok := def.Source.Locations[d.String()]; ok {
				locs := []any{}
				for _, l := range ls.Locations {
					rs := []any{}
					for _, r := range l.Ranges {
						rs = append(rs, []int32{r.Start.Line, r.Start.Character, r.End.Line, r.End.Character})
					}
					locs = append(locs, map[string]any{"source": l.SourceIndex, "ranges": rs})
				}
				v["locations"] = locs
			}
		}
		list = append(list, v)
	}
	out["ops"] = list
	return out
}

func rangesText(rs []parser.Range) string {
	var lines []string
	for _, r := range rs {
		lines = append(lines, fmt.Sprintf("%d-%d", r.Start.Line, r.End.Line))
	}
	return strings.Join(lines, ",")
}

// A lexer case: the lexer's settings, the environment, and the input.
type lexCase struct {
	Escape     string   `json:"escape"`
	RawQuotes  bool     `json:"raw_quotes"`
	RawEscapes bool     `json:"raw_escapes"`
	SkipUnset  bool     `json:"skip_unset"`
	SkipQuotes bool     `json:"skip_quotes"`
	Env        []string `json:"env"`
	Input      string   `json:"input"`
}

type lexed struct {
	lexCase
	Word      string   `json:"word,omitempty"`
	Matched   []string `json:"matched,omitempty"`
	Unmatched []string `json:"unmatched,omitempty"`
	WordError string   `json:"word_error,omitempty"`
	Words     []string `json:"words,omitempty"`
	WordsErr  string   `json:"words_error,omitempty"`
}

func lex(c lexCase) lexed {
	l := shell.NewLex([]rune(c.Escape)[0])
	l.RawQuotes, l.RawEscapes, l.SkipUnsetEnv, l.SkipProcessQuotes = c.RawQuotes, c.RawEscapes, c.SkipUnset, c.SkipQuotes
	env := shell.EnvsFromSlice(c.Env)
	out := lexed{lexCase: c}
	if r, err := l.ProcessWordWithMatches(c.Input, env); err != nil {
		out.WordError = q(err.Error())
	} else {
		out.Word = q(r.Result)
		for k := range r.Matched {
			out.Matched = append(out.Matched, q(k))
		}
		for k := range r.Unmatched {
			out.Unmatched = append(out.Unmatched, q(k))
		}
		sort.Strings(out.Matched)
		sort.Strings(out.Unmatched)
	}
	if ws, err := l.ProcessWords(c.Input, env); err != nil {
		out.WordsErr = q(err.Error())
	} else {
		out.Words = qs(ws)
		if out.Words == nil {
			out.Words = []string{}
		}
	}
	return out
}

// BuildKit's own lexer tables, as lex_test.go runs them on Unix.
func buildkitCases(dir string) []lexCase {
	var cases []lexCase
	read := func(name string) []string {
		f, err := os.Open(filepath.Join(dir, name))
		if err != nil {
			panic(err)
		}
		defer f.Close()
		var lines []string
		s := bufio.NewScanner(f)
		for s.Scan() {
			lines = append(lines, s.Text())
		}
		return lines
	}
	env := []string{"PWD=/home", "SHELL=bash", "KOREAN=한국어", "NULL="}
	for _, line := range read("envVarTest") {
		if strings.HasPrefix(line, "#") || strings.TrimSpace(line) == "" {
			continue
		}
		f := strings.Split(strings.TrimSpace(line), "|")
		if p := strings.TrimSpace(f[0]); p != "A" && p != "U" {
			continue
		}
		cases = append(cases, lexCase{Escape: `\`, Env: env, Input: strings.TrimSpace(f[1])})
	}
	for _, only := range []bool{false, true} {
		var envs []string
		for _, line := range read("wordsTest") {
			if strings.HasPrefix(line, "#") {
				continue
			}
			if strings.HasPrefix(line, "ENV ") {
				envs = append(envs, strings.TrimLeft(line[3:], " "))
				continue
			}
			f := strings.Split(line, "|")
			cases = append(cases, lexCase{Escape: `\`, RawQuotes: only, SkipUnset: only,
				Env: append([]string(nil), envs...), Input: strings.TrimSpace(f[0])})
		}
	}
	return cases
}

// The runes for which `in` is true, as sorted inclusive ranges.
func table(name, doc string, in func(r rune) bool) string {
	var b strings.Builder
	fmt.Fprintf(&b, "\n/// %s\npub(crate) static %s: &[(u32, u32)] = &[\n", doc, name)
	start := rune(-1)
	for r := rune(0); r <= unicode.MaxRune+1; r++ {
		inside := r <= unicode.MaxRune && in(r)
		if inside && start < 0 {
			start = r
		}
		if !inside && start >= 0 {
			fmt.Fprintf(&b, "    (0x%X, 0x%X),\n", start, r-1)
			start = -1
		}
	}
	b.WriteString("];\n")
	return b.String()
}

// What Go makes of each image config in configs.json (a string, or {"base64": ...} for
// text that is not UTF-8): json.Unmarshal into the DockerOCIImage BuildKit reads a base
// image's config into, then json.Marshal of it, as BuildKit writes the config it builds.
func configsFile(testdata string) {
	data, err := os.ReadFile(filepath.Join(testdata, "configs.json"))
	if err != nil {
		panic(err)
	}
	var cases []json.RawMessage
	if err := json.Unmarshal(data, &cases); err != nil {
		panic(err)
	}
	var out []map[string]any
	for _, raw := range cases {
		var text string
		if err := json.Unmarshal(raw, &text); err != nil {
			var enc struct{ Base64 string }
			if err := json.Unmarshal(raw, &enc); err != nil {
				panic(err)
			}
			b, err := base64.StdEncoding.DecodeString(enc.Base64)
			if err != nil {
				panic(err)
			}
			text = string(b)
		}
		r := map[string]any{"input": raw}
		var img dockerspec.DockerOCIImage
		if err := json.Unmarshal([]byte(text), &img); err != nil {
			r["error"] = q(err.Error())
		} else if b, err := json.Marshal(img); err != nil {
			r["marshal_error"] = q(err.Error())
		} else {
			r["config"] = string(b)
		}
		out = append(out, r)
	}
	writeJSON(filepath.Join(testdata, "configs-answers.json"), out)
}

// What go-units' RAMInBytes, which a tmpfs mount's size= goes through, makes of each
// size in sizes.json.
func sizesFile(testdata string) {
	data, err := os.ReadFile(filepath.Join(testdata, "sizes.json"))
	if err != nil {
		panic(err)
	}
	var cases []string
	if err := json.Unmarshal(data, &cases); err != nil {
		panic(err)
	}
	var out []map[string]any
	for _, c := range cases {
		n, err := units.RAMInBytes(c)
		if err != nil {
			out = append(out, map[string]any{"input": c, "error": q(err.Error())})
		} else {
			out = append(out, map[string]any{"input": c, "bytes": strconv.FormatInt(n, 10)})
		}
	}
	writeJSON(filepath.Join(testdata, "sizes-answers.json"), out)
}

// What Go's net/url, BuildKit's gitutil.ParseURL and dfgitutil.ParseGitRef make of each
// source in urls.json, as ADD reads its sources.
func urlsFile(testdata string) {
	data, err := os.ReadFile(filepath.Join(testdata, "urls.json"))
	if err != nil {
		panic(err)
	}
	var cases []string
	if err := json.Unmarshal(data, &cases); err != nil {
		panic(err)
	}
	var out []map[string]any
	for _, c := range cases {
		r := map[string]any{"input": c}
		if u, err := url.Parse(c); err != nil {
			r["url_error"] = q(err.Error())
		} else {
			user := ""
			if u.User != nil {
				user = u.User.String() + "@"
			}
			r["url"] = map[string]any{
				"scheme": u.Scheme, "opaque": u.Opaque, "user": user, "host": u.Host, "path": u.Path,
				"raw_path": u.RawPath, "raw_query": u.RawQuery, "fragment": u.Fragment, "string": u.String(),
			}
		}
		if g, err := gitutil.ParseURL(c); err != nil {
			r["git_url_error"] = q(err.Error())
		} else {
			r["git_url"] = map[string]any{"scheme": g.Scheme, "host": g.Host, "path": g.Path, "remote": g.Remote, "opts": g.Opts != nil}
		}
		ref, isGit, err := dfgitutil.ParseGitRef(c)
		r["is_git"] = isGit
		if err != nil && isGit {
			r["git_ref_error"] = q(err.Error())
		} else if err == nil {
			r["git_ref"] = map[string]any{
				"remote": ref.Remote, "short_name": ref.ShortName, "ref": ref.Ref, "checksum": ref.Checksum,
				"subdir": ref.SubDir, "local": ref.IndistinguishableFromLocal, "tcp": ref.UnencryptedTCP,
				"keep":       fmt.Sprint(ref.KeepGitDir != nil && *ref.KeepGitDir, ref.KeepGitDir != nil),
				"submodules": fmt.Sprint(ref.Submodules != nil && *ref.Submodules, ref.Submodules != nil),
				"mtime":      ref.MTime, "fetch_by_commit": ref.FetchByCommit,
			}
		}
		out = append(out, r)
	}
	writeJSON(filepath.Join(testdata, "urls-answers.json"), out)
}

// What Go's filepath.Match and moby/patternmatcher make of each set of patterns in
// patterns.json and each path: COPY's wildcards, .dockerignore, --exclude and --parents.
// ignores.json: what ignorefile.ReadAll makes of each .dockerignore file's text.
func ignoresFile(testdata string) {
	data, err := os.ReadFile(filepath.Join(testdata, "ignores.json"))
	if err != nil {
		panic(err)
	}
	var files []string
	if err := json.Unmarshal(data, &files); err != nil {
		panic(err)
	}
	var out []map[string]any
	for _, f := range files {
		r := map[string]any{"file": q(f)}
		patterns, err := ignorefile.ReadAll(strings.NewReader(f))
		if err != nil {
			r["error"] = q(err.Error())
		} else {
			var qs []string
			for _, p := range patterns {
				qs = append(qs, q(p))
			}
			r["patterns"] = qs
		}
		out = append(out, r)
	}
	writeJSON(filepath.Join(testdata, "ignores-answers.json"), out)
}

func patternsFile(testdata string) {
	data, err := os.ReadFile(filepath.Join(testdata, "patterns.json"))
	if err != nil {
		panic(err)
	}
	var sets []struct {
		Patterns []string `json:"patterns"`
		Paths    []string `json:"paths"`
	}
	if err := json.Unmarshal(data, &sets); err != nil {
		panic(err)
	}
	ans := func(ok bool, err error) string {
		if err != nil {
			return "error " + err.Error()
		}
		return fmt.Sprint(ok)
	}
	var out []map[string]any
	for _, set := range sets {
		r := map[string]any{"patterns": set.Patterns, "paths": set.Paths}
		var match [][]string
		for _, p := range set.Patterns {
			var row []string
			for _, path := range set.Paths {
				row = append(row, ans(filepath.Match(p, path)))
			}
			match = append(match, row)
		}
		r["filepath_match"] = match
		pm, err := patternmatcher.New(set.Patterns)
		if err != nil {
			r["new_error"] = err.Error()
			out = append(out, r)
			continue
		}
		var matches, parents, walked []string
		for _, path := range set.Paths {
			matches = append(matches, ans(pm.Matches(path)))
			parents = append(parents, ans(pm.MatchesOrParentMatches(path)))
			// Down the path, each step given its parent's results, as fsutil walks.
			var info patternmatcher.MatchInfo
			var last string
			parts := strings.Split(path, "/")
			for i := range parts {
				var ok bool
				ok, info, err = pm.MatchesUsingParentResults(strings.Join(parts[:i+1], "/"), info)
				last = ans(ok, err)
				if err != nil {
					break
				}
			}
			walked = append(walked, last)
		}
		r["matches"], r["parent_matches"], r["walked"] = matches, parents, walked
		out = append(out, r)
	}
	writeJSON(filepath.Join(testdata, "patterns-answers.json"), out)
}

func writeJSON(path string, v any) {
	b, err := json.MarshalIndent(v, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(path, append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}

func main() {
	testdata := os.Args[1]
	var files []string
	for _, dir := range []string{"buildkit/parser", "buildkit/parser-negative", "corpus"} {
		filepath.WalkDir(filepath.Join(testdata, dir), func(p string, d os.DirEntry, err error) error {
			if err == nil && !d.IsDir() && (d.Name() == "Dockerfile" || strings.HasSuffix(d.Name(), ".Dockerfile")) {
				rel, _ := filepath.Rel(testdata, p)
				files = append(files, filepath.ToSlash(rel))
			}
			return nil
		})
	}
	sort.Strings(files)
	var parses []parsed
	for _, f := range files {
		parses = append(parses, parseFile(testdata, f))
	}
	writeJSON(filepath.Join(testdata, "parse.json"), parses)
	var insts []map[string]any
	for _, f := range files {
		insts = append(insts, instructionsFile(testdata, f))
	}
	writeJSON(filepath.Join(testdata, "instructions.json"), insts)
	var images resolver
	imgData, err := os.ReadFile(filepath.Join(testdata, "images.json"))
	if err != nil {
		panic(err)
	}
	if err := json.Unmarshal(imgData, &images); err != nil {
		panic(err)
	}
	var plans []map[string]any
	for _, f := range files {
		if strings.HasPrefix(f, "corpus/plan/") {
			plans = append(plans, planFile(testdata, f, images))
		}
	}
	writeJSON(filepath.Join(testdata, "plan.json"), plans)
	var subs []map[string]any
	for _, f := range files {
		if strings.HasPrefix(f, "corpus/plan/") {
			subs = append(subs, subrequestsFile(testdata, f, images))
		}
	}
	writeJSON(filepath.Join(testdata, "subrequests.json"), map[string]any{
		"describe": describeFile(),
		"files":    subs,
	})
	configsFile(testdata)
	sizesFile(testdata)
	urlsFile(testdata)
	patternsFile(testdata)
	ignoresFile(testdata)

	cases := buildkitCases(filepath.Join(testdata, "buildkit/shell"))
	extra, err := os.ReadFile(filepath.Join(testdata, "lex-cases.json"))
	if err != nil {
		panic(err)
	}
	var mine []lexCase
	if err := json.Unmarshal(extra, &mine); err != nil {
		panic(err)
	}
	cases = append(cases, mine...)
	var lexes []lexed
	for _, c := range cases {
		lexes = append(lexes, lex(c))
	}
	writeJSON(filepath.Join(testdata, "lex.json"), lexes)

	tables := fmt.Sprintf("//! Generated by scripts/dockerfile/generate with Go %s (Unicode %s), the\n"+
		"//! character classes BuildKit's parser and lexer test with. Do not edit.\n",
		strings.TrimPrefix(runtime.Version(), "go"), unicode.Version)
	tables += table("LETTER", "Go's `unicode.IsLetter`.", unicode.IsLetter)
	tables += table("DIGIT", "Go's `unicode.IsDigit`.", unicode.IsDigit)
	tables += table("SPACE", "Go's `unicode.IsSpace`.", unicode.IsSpace)
	tables += table("NOT_PRINT", "Runes from U+0080 up that Go's `strconv.IsPrint` rejects, which `strconv.Quote` escapes.",
		func(r rune) bool { return r >= 0x80 && !strconv.IsPrint(r) })
	var lower strings.Builder
	lower.WriteString("\n/// Go's `unicode.ToLower`, for the runes it changes: (rune, lower case).\npub(crate) static LOWER: &[(u32, u32)] = &[\n")
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if l := unicode.ToLower(r); l != r {
			fmt.Fprintf(&lower, "    (0x%X, 0x%X),\n", r, l)
		}
	}
	lower.WriteString("];\n")
	tables += lower.String()
	var upper strings.Builder
	upper.WriteString("\n/// Go's `unicode.ToUpper`, for the runes it changes: (rune, upper case).\npub(crate) static UPPER: &[(u32, u32)] = &[\n")
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if u := unicode.ToUpper(r); u != r {
			fmt.Fprintf(&upper, "    (0x%X, 0x%X),\n", r, u)
		}
	}
	upper.WriteString("];\n")
	tables += upper.String()
	if err := os.WriteFile(filepath.Join(testdata, "../src/tables.rs"), []byte(tables), 0o644); err != nil {
		panic(err)
	}
}

// normalizedRoot is the digest of a definition's root as the plan's ops are recorded (a
// local source's unique ID as "*", each input named by its own op's normalized digest).
func normalizedRoot(defs [][]byte) string {
	norm := map[digest.Digest]digest.Digest{}
	var last digest.Digest
	for _, dt := range defs {
		var op pb.Op
		if err := op.UnmarshalVT(dt); err != nil {
			panic(err)
		}
		if src := op.GetSource(); src != nil {
			if _, ok := src.Attrs["local.unique"]; ok {
				src.Attrs["local.unique"] = "*"
			}
		}
		for _, in := range op.Inputs {
			in.Digest = string(norm[digest.Digest(in.Digest)])
		}
		b, err := proto.MarshalOptions{Deterministic: true}.Marshal(&op)
		if err != nil {
			panic(err)
		}
		last = digest.FromBytes(dt)
		norm[last] = digest.FromBytes(b)
	}
	return string(norm[last])
}
