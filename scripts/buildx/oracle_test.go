package main

// buildx's own answers to `build` command lines, for shards-cmdline's tests to match
// (crates/cmdline/tests/buildx.rs). `generate` copies this file into buildx's cmd/buildx
// and runs it there. Each case runs as `docker build ...` runs buildx: as the CLI plugin,
// its root built by commands.NewRootCmd and wrapped by the CLI's plugin.RunPlugin, its
// errors printed and its exit status chosen as main does. The flags shards does not
// serve are hidden, and the build replaced by a line that says what it was asked.

import (
	"slices"
	"github.com/containerd/platforms"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"testing"

	"github.com/docker/buildx/build"
	"github.com/docker/buildx/commands"
	"github.com/docker/buildx/util/buildflags"
	"github.com/docker/buildx/util/cobrautil"
	dockeropts "github.com/docker/cli/opts"
	"github.com/moby/buildkit/client"
	"github.com/moby/buildkit/session/secrets/secretsprovider"
	"github.com/docker/cli/cli"
	"github.com/docker/cli/cli-plugins/metadata"
	"github.com/docker/cli/cli-plugins/plugin"
	"github.com/docker/cli/cli/command"
	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// The flags shards serves.
var served = []string{
	"add-host", "allow", "annotation", "attest", "build-arg", "build-context", "builder", "cache-from", "cache-to", "call", "cgroup-parent", "check", "debug",
	"file", "help",
	"iidfile", "label", "load", "metadata-file", "network", "no-cache", "no-cache-filter", "platform", "policy", "provenance", "resource",
	"sbom", "shm-size",
	"output", "progress", "pull", "push", "quiet", "secret", "ssh", "tag", "target", "ulimit",
}

// The command lines asked, each the words after `buildx build`.
var cases = [][]string{
	{"--policy", "filename=a.rego", "--policy", "FILENAME=b.rego,filename=c.rego,reset=true,strict=1,log-level=DEBUG", "."},
	{"--policy", "disabled=true", "."},
	{"--policy", "\"filename=a,b.rego\",Log-Level=warning", "."},
	{"--policy", "strict=false", "--policy", "log-level=trace", "."},
	{"--policy", "foo", "."},
	{"--policy", "filename=", "."},
	{"--policy", "reset=maybe", "."},
	{"--policy", "log-level=loud", "."},
	{"--policy", "bogus=1", "."},
	{"--policy", "\"unterminated", "."},
	{"--policy", "foo", "--attest", "bogus", "."},
	{"--provenance", "mode=max", "."},
	{"--provenance=false", "."},
	{"--provenance=true", "--sbom=false", "."},
	{"--sbom", "true", "--provenance", "builder-id=x,reproducible=true", "."},
	{"--attest", "type=provenance,mode=min", "--attest", "type=sbom,disabled=true", "."},
	{"--attest", "type=provenance,mode=min", "--attest", "type=provenance,mode=max", "."},
	{"--attest", "TYPE=sbom,DISABLED=true", "."},
	{"--attest", "type=provenance,a=\"b,c\"", "."},
	{"--attest", "mode=min", "."},
	{"--attest", "nokey", "."},
	{"--attest", "type=provenance,disabled=maybe", "."},
	{"--attest", "", "."},
	{"--build-context", "base=docker-image://alpine:3.22", "--build-context", "src=./dir", "."},
	{"--build-context", "Alpine:latest=x", "."},
	{"--build-context", "docker.io/library/alpine:latest=x", "--build-context", "alpine=y", "."},
	{"--build-context", "noequals", "."},
	{"--build-context", "=x", "."},
	{"--build-context", "", "."},
	{"--build-context", "a=b=c", "."},
	{"--build-context", "registry.example.com:5000/team/base:1.0=oci-layout://./layout:1.0", "."},
	{"--build-context", "x@sha256:abcd=y", "."},
	{"--iidfile", "id.txt", "-o", "type=tar,dest=a.tar", "."},
	{"--iidfile", "id.txt", "-o", "./outdir", "."},
	{"--iidfile", "id.txt", "-o", "type=oci,dest=a.tar", "."},
	{},
	{"--help"},
	{"-h"},
	{"."},
	{".", "extra"},
	{"--nope", "."},
	{"-t", "app:1", "."},
	{"-t", "a", "-t", "b", "--tag=c", "."},
	{"-f", "Dockerfile.dev", "ctx"},
	{"-f", "-", "."},
	{"--file"},
	{"--build-arg", "A=1", "--build-arg", "B", "."},
	{"--target", "build", "--label", "k=v", "--label", "x", "."},
	{"--platform", "linux/amd64", "."},
	{"--platform", "linux/amd64,linux/arm64", "."},
	{"--progress", "plain", "."},
	{"--progress=quiet", "-q", "."},
	{"-q", "--iidfile", "id.txt", "."},
	{"--no-cache", "--pull", "--load", "."},
	{"--no-cache=false", "."},
	{"--push", "."},
	{"--secret", "id=a", "."},
	{"-o", "type=local,dest=out", "."},
	{"--cache-from", "user/app:cache", "--cache-to", "type=local,dest=out", "."},
	{"--cache-from", "a,b", "--cache-to", "type=registry,ref=x,mode=max", "."},
	{"--cache-to", "mode=max", "."},
	{"--cache-from", "type=local,src=dir,=x", "."},
	{"--no-cache-filter", "build", "--no-cache-filter", "a,b", "."},
	{"--metadata-file", "meta.json", "."},
	{"--add-host", "db:10.0.0.2", "--add-host", "a=1.2.3.4,b:5.6.7.8", "."},
	{"--builder", "default", "-D", "."},
	{"--call", "check,ignorestatus=true", "."},
	{"--call=outline", "."},
	{"--annotation", "org.opencontainers.image.title=app", "--annotation", "manifest,manifest-descriptor:a=b", ".",},
	{"--annotation", "index:k=v", "--annotation", "manifest[linux/amd64]:p=q", "."},
	{"--annotation", "noequals", "."},
	{"--annotation", "bogus:k=v", "."},
	{"--annotation", "Manifest:k=v", "."},
	{"--shm-size", "64m", "--cgroup-parent", "/x", "--network", "none", "."},
	{"--shm-size", "lots", "."},
	{"--resource", "memory=2g", "--resource", "cpu-quota=50000", "."},
	{"--metadata-file", "."},
	{"--network", "none", "."},
	{"--network", "default", "."},
	{"--builder", "x", "."},
	{"-D", "."},
	{"-", "<", "x"},
	{"https://github.com/docker/buildx.git"},
	{".", "-t", "late"},
	{"-tq", "app", "."},
	{"--quiet=true", "."},
	{"--progress"},
	{"--no-cache-filter", "stage", "."},
	{"--call", "check", "."},
	// Secrets, as buildx reads them and BuildKit's store finds them, in a directory of
	// small.txt, edge.txt (500 KiB) and big.txt (one byte more), with
	// SHARDS_ORACLE_SECRET set (TestShardsOracle).
	{"--secret", "id=small,src=small.txt", "."},
	{"--secret", "id=small,source=small.txt", "."},
	{"--secret", "ID=small,SRC=small.txt", "."},
	{"--secret", "type=file,id=small,src=small.txt", "."},
	{"--secret", "id=fromenv,env=SHARDS_ORACLE_SECRET", "."},
	{"--secret", "type=env,id=fromenv,src=SHARDS_ORACLE_SECRET", "."},
	{"--secret", "type=env,id=SHARDS_ORACLE_SECRET", "."},
	{"--secret", "id=SHARDS_ORACLE_SECRET", "."},
	{"--secret", "id=small.txt", "."},
	{"--secret", "id=edge,src=edge.txt", "."},
	{"--secret", "id=big,src=big.txt", "."},
	{"--secret", "id=gone,src=gone.txt", "."},
	{"--secret", "src=small.txt", "."},
	{"--secret", "type=ssh,id=a", "."},
	{"--secret", "id", "."},
	{"--secret", "id=a,what=b", "."},
	{"--secret", "\"id=a", "."},
	{"--secret", "id=\"a,b\",src=small.txt", "."},
	{"--secret", "", "."},
	{"--secret", "id=x,src=small.txt", "--secret", "id=x,src=edge.txt", "."},
	// --output, --push and --load, in the oracle's directory, where small.txt is a file.
	{"-o", "out", "."},
	{"-o", "-", "."},
	{"-o", "type=tar", "."},
	{"-o", "type=tar,dest=out.tar", "."},
	{"-o", "type=local", "."},
	{"-o", "type=local,dest=-", "."},
	{"-o", "type=local,dest=small.txt", "."},
	{"-o", "type=tar,dest=.", "."},
	{"-o", "type=oci,dest=out.tar", "."},
	{"-o", "type=oci,tar=false,dest=layout", "."},
	{"-o", "type=docker", "."},
	{"-o", "type=docker,dest=out.tar", "."},
	{"-o", "type=registry", "."},
	{"-o", "type=image,name=x,push=true", "."},
	{"-o", "-", "-o", "type=tar", "."},
	{"-o", "dest=x", "."},
	{"-o", "a=b,c", "."},
	{"-o", "type=foo,dest=x,Key=V", "."},
	{"-o", "type=local,dest=out,mode=delete", "."},
	{"-o", "type=local,dest=/tmp/shards-oracle-out,mode=delete", "."},
	{"-o", "type=local,dest=/tmp/shards-oracle-out,mode=delete", "--allow", "buildx.local.delete", "."},
	{"-o", "type=local,dest=out,mode=bogus", "."},
	{"--push", "."},
	{"--push", "-o", "type=image", "."},
	{"--push", "-o", "out", "."},
	{"--load", "."},
	{"--load", "-o", "type=docker,dest=out.tar", "."},
	{"--load", "--push", "."},
	// --ssh, SSH_AUTH_SOCK unset (TestShardsOracle): what BuildKit's provider refuses.
	{"--ssh", "default", "."},
	{"--ssh", "", "."},
	{"--ssh", "c=", "."},
	{"--ssh", "a=/shards/no/such/socket", "."},
	{"--ssh", "=/shards/no/such/socket", "."},
	{"--secret", "id=x,src=small.txt", "--secret", "id=y,env=SHARDS_ORACLE_SECRET", "."},
	{"--secret=id=small,src=small.txt", "--allow", "nope", "."},
	// Ulimits, as docker/cli's UlimitOpt and go-units read them.
	{"--ulimit", "nofile=1024", "."},
	{"--ulimit", "nofile=1024:2048", "--ulimit", "core=0", "."},
	{"--ulimit", "nofile=1", "--ulimit", "nofile=2:3", "."},
	{"--ulimit", "nofile=2048:1024", "."},
	{"--ulimit", "nofile=-1:1024", "."},
	{"--ulimit", "nofile=-1", "."},
	{"--ulimit", "nofile=5:-1", "."},
	{"--ulimit", "as=1", "."},
	{"--ulimit", "nofile", "."},
	{"--ulimit", "nofile=1:2:3", "."},
	{"--ulimit", "nofile=0x10", "."},
	{"--ulimit", "nofile=010", "."},
	{"--ulimit", "nofile=1_0", "."},
	{"--ulimit", "nofile=+5", "."},
	{"--ulimit", "nofile=", "."},
	{"--ulimit", "nproc=99999999999999999999", "."},
	{"--ulimit", "=1", "."},
	// Entitlements, as buildx reads them.
	{"--allow", "security.insecure", "."},
	{"--allow", "network.host", "--allow", "security.insecure", "."},
	{"--allow", "buildx.local.delete", "."},
	{"--allow", "buildx.local.delete=1", "."},
	{"--allow", "device", "."},
	{"--allow", "device=nvidia.com/gpu=all,alias=gpu", "."},
	{"--allow", "device=a,bad", "."},
	{"--allow", "device=a,what=b", "."},
	{"--allow", "network.host=1", "."},
	{"--allow", "nope", "."},
	{"--allow", "", "."},
	{"--check", "."},
	{"prune", "--help"},
	{"prune", "-af", "--filter", "until=24h", "--filter", "type=regular"},
	{"prune", "--keep-storage", "1g"},
	{"prune", "--reserved-space", "1g", "--max-used-space", "2g", "--min-free-space", "10gb", "--verbose"},
	{"prune", "--reserved-space", "lots"},
	{"prune", "extra"},
	{"prune", "--filter", "noequals"},
	{"prune", "--timeout", "5s", "-f"},
	{"policy"},
	{"policy", "--help"},
	{"policy", "-h"},
	{"policy", "foo"},
	{"policy", "--bogus"},
	{"policy", "eval", "--help"},
	{"policy", "eval", "-h"},
	{"policy", "eval"},
	{"policy", "eval", "a", "b"},
	{"policy", "eval", "docker-image://alpine"},
	{"policy", "eval", "--print", "--fields", "image.checksum,image.labels", "--fields", "git.commit", "--platform", "linux/arm64", "docker-image://alpine"},
	{"policy", "eval", "-f", "Agentfile", "."},
	{"policy", "eval", "--filename", "X", "--file", "Y", "."},
	{"policy", "eval", "--print=false", "."},
	{"policy", "eval", "--print=maybe", "."},
	{"policy", "eval", "--platform", "."},
	{"policy", "eval", "--bogus", "."},
	{"policy", "eval", ".", "--print"},
	{"policy", "eval", "--builder", "b", "--debug", "."},
	{"policy", "test", "--help"},
	{"policy", "test"},
	{"policy", "test", "a", "b"},
	{"policy", "test", "--run", "deny", "--filename", "Agentfile", "policies/"},
	{"policy", "test", "--run", "policies"},
	{"policy", "test", "-f", "x", "p"},
	{"policy", "test", "--bogus", "p"},
}

type answer struct {
	Argv   []string `json:"argv"`
	Stdout string   `json:"stdout"`
	Stderr string   `json:"stderr"`
	Status int      `json:"status"`
}

// ask runs buildx as the CLI runs its plugin for `docker build ARGV...`, and answers
// what it printed and its exit status.
func ask(t *testing.T, argv []string) answer {
	// Each case as a new process: an earlier case's --debug sets DEBUG (debug.Enable),
	// which the root's --debug takes as its default.
	_ = os.Unsetenv("DEBUG")
	var stdout, stderr bytes.Buffer
	dockerCli, err := command.NewDockerCli(command.WithOutputStream(&stdout), command.WithErrorStream(&stderr))
	if err != nil {
		t.Fatal(err)
	}
	rootCmd := commands.NewRootCmd("buildx", true, dockerCli)
	// A case is of `build` unless its first word names `prune`, which `docker builder
	// prune` runs.
	sub := "build"
	words := []string{}
	if len(argv) > 0 && (argv[0] == "prune" || argv[0] == "policy") {
		sub = argv[0]
		words = append(words, argv[0])
		argv = argv[1:]
	}
	var build *cobra.Command
	for _, c := range rootCmd.Commands() {
		if c.Name() == sub {
			build = c
		}
	}
	// `policy eval` and `policy test`: the policy command's own.
	if sub == "policy" && build != nil && len(argv) > 0 && (argv[0] == "eval" || argv[0] == "test") {
		for _, c := range build.Commands() {
			if c.Name() == argv[0] {
				build = c
			}
		}
		words = append(words, argv[0])
		argv = argv[1:]
	}
	if build == nil {
		t.Fatal("no " + sub + " command")
	}
	hide := func(f *pflag.Flag) {
		if !contains(served, f.Name) {
			f.Hidden = true
		}
	}
	if sub == "build" {
		build.Flags().VisitAll(hide)
		rootCmd.PersistentFlags().VisitAll(hide)
	}
	build.PreRunE, build.PreRun = nil, nil
	if build.Runnable() || sub != "policy" {
	build.RunE = func(c *cobra.Command, args []string) error {
		var line strings.Builder
		line.WriteString("RUN")
		c.Flags().VisitAll(func(f *pflag.Flag) {
			if f.Changed {
				fmt.Fprintf(&line, " --%s=%s", f.Name, f.Value.String())
			}
		})
		for _, a := range args {
			fmt.Fprintf(&line, " %q", a)
		}
		fmt.Fprintln(c.OutOrStdout(), line.String())
		if sub != "build" {
			return nil
		}
		return built(c)
	}
	}
	// As the CLI execs a plugin: its path, then its name, then the words.
	if len(words) == 0 {
		words = []string{sub}
	}
	os.Args = append(append([]string{"docker-buildx", "buildx"}, words...), argv...)
	err = plugin.RunPlugin(dockerCli, rootCmd, metadata.Metadata{SchemaVersion: "0.1.0", Vendor: "Docker Inc."})
	status := 0
	// As main does (cmd/buildx/main.go), without its debug, policy and gRPC cases.
	if err != nil {
		var sterr cli.StatusError
		var exitCodeErr cobrautil.ExitCodeError
		switch {
		case errors.As(err, &sterr):
			if sterr.Status != "" {
				fmt.Fprintln(&stderr, sterr.Status)
			}
			status = sterr.StatusCode
			if status == 0 {
				status = 1
			}
		case errors.As(err, &exitCodeErr):
			status = int(exitCodeErr)
		default:
			fmt.Fprintf(&stderr, "ERROR: %v\n", err)
			status = 1
		}
	}
	if sub != "build" {
		argv = append(words, argv...)
	}
	return answer{argv, shards(stdout.String()), shards(stderr.String()), status}
}

// built says what buildx's build makes of the secrets, ulimits and entitlements it was
// given, as runBuild meets them: the secrets' specs (toControllerOptions), their store
// (CreateSecrets, BuildKit's secretsprovider.NewStore), then the entitlements
// (ParseEntitlements); the ulimits as the frontend's `ulimit` option carries them.
func built(c *cobra.Command) error {
	out := c.OutOrStdout()
	// The policies, as toOptions reads them before the attestations (ParsePolicyConfigs).
	policyArgs, _ := c.Flags().GetStringArray("policy")
	policies, err := buildflags.ParsePolicyConfigs(policyArgs)
	if err != nil {
		return err
	}
	for _, p := range policies {
		var files []string
		for _, f := range p.Files {
			files = append(files, f.Filename)
		}
		strict, level := "unset", "unset"
		if p.Strict != nil {
			strict = strconv.FormatBool(*p.Strict)
		}
		if p.LogLevel != nil {
			level = p.LogLevel.String()
		}
		fmt.Fprintf(out, "POLICY files=%q reset=%t disabled=%t strict=%s log-level=%s\n", files, p.Reset, p.Disabled, strict, level)
	}
	// The attestations, as toBuildOptions reads them first (the shorthands canonicalized,
	// ParseAttests), and as the build hands them on (ToMap).
	attestArgs, _ := c.Flags().GetStringArray("attest")
	inAttests := slices.Clone(attestArgs)
	if v, _ := c.Flags().GetString("provenance"); v != "" {
		inAttests = append(inAttests, buildflags.CanonicalizeAttest("provenance", v))
	}
	if v, _ := c.Flags().GetString("sbom"); v != "" {
		inAttests = append(inAttests, buildflags.CanonicalizeAttest("sbom", v))
	}
	attests, err := buildflags.ParseAttests(inAttests)
	if err != nil {
		return err
	}
	attestMap := attests.ToMap()
	var attestKeys []string
	for k := range attestMap {
		attestKeys = append(attestKeys, k)
	}
	sort.Strings(attestKeys)
	for _, k := range attestKeys {
		if v := attestMap[k]; v == nil {
			fmt.Fprintf(out, "ATTEST %s disabled\n", k)
		} else {
			fmt.Fprintf(out, "ATTEST %s %q\n", k, *v)
		}
	}
	// The named contexts (ParseContextNames), as toBuildOptions reads them before the
	// outputs; then the outputs' check against an image ID file.
	ctxArgs, _ := c.Flags().GetStringArray("build-context")
	contexts, err := buildflags.ParseContextNames(ctxArgs)
	if err != nil {
		return err
	}
	var ctxNames []string
	for k := range contexts {
		ctxNames = append(ctxNames, k)
	}
	sort.Strings(ctxNames)
	for _, k := range ctxNames {
		fmt.Fprintf(out, "CONTEXT %s %q\n", k, contexts[k])
	}
	earlyOut, _ := c.Flags().GetStringArray("output")
	early, err := buildflags.ParseExports(earlyOut)
	if err != nil {
		return err
	}
	if iid, _ := c.Flags().GetString("iidfile"); iid != "" {
		for _, e := range early {
			if e.Type == "local" || e.Type == "tar" {
				return fmt.Errorf("local and tar exporters are incompatible with image ID file")
			}
		}
	}
	specs, _ := c.Flags().GetStringArray("secret")
	secrets, err := buildflags.ParseSecretSpecs(specs)
	if err != nil {
		return err
	}
	sources := make([]secretsprovider.Source, 0, len(secrets))
	ids := map[string]bool{}
	for _, s := range secrets {
		sources = append(sources, secretsprovider.Source{ID: s.ID, FilePath: s.FilePath, Env: s.Env})
		ids[s.ID] = true
	}
	store, err := secretsprovider.NewStore(sources)
	if err != nil {
		return err
	}
	var names []string
	for id := range ids {
		names = append(names, id)
	}
	sort.Strings(names)
	for _, id := range names {
		dt, err := store.GetSecret(context.Background(), id)
		if err != nil {
			fmt.Fprintf(out, "SECRET %s: %v\n", id, err)
			continue
		}
		head := dt
		if len(head) > 16 {
			head = head[:16]
		}
		fmt.Fprintf(out, "SECRET %s: %d bytes, %q\n", id, len(dt), head)
	}
	// The SSH agents (ParseSSHSpecs, CreateSSH: BuildKit's sshprovider), with no
	// SSH_AUTH_SOCK (TestShardsOracle).
	sshArgs, _ := c.Flags().GetStringArray("ssh")
	sshSpecs, err := buildflags.ParseSSHSpecs(sshArgs)
	if err != nil {
		return err
	}
	if _, err := build.CreateSSH(sshSpecs); err != nil {
		return err
	}
	for _, s := range sshSpecs {
		fmt.Fprintf(out, "SSH %s %q\n", s.ID, s.Paths)
	}
	allow, _ := c.Flags().GetStringArray("allow")
	granted, deleteOK, err := buildflags.ParseEntitlements(allow)
	if err != nil {
		return err
	}
	// The outputs: ParseExports, CreateExports, --push and --load as toBuildOptions folds
	// them in, then ValidateLocalExportDelete.
	outArgs, _ := c.Flags().GetStringArray("output")
	exports, err := buildflags.ParseExports(outArgs)
	if err != nil {
		return err
	}
	outputs, _, err := build.CreateExports(exports)
	if err != nil {
		return err
	}
	if push, _ := c.Flags().GetBool("push"); push {
		used := false
		for i := range outputs {
			if outputs[i].Type == "image" {
				outputs[i].Attrs["push"] = "true"
				if _, ok := outputs[i].Attrs["unpack"]; !ok {
					outputs[i].Attrs["unpack"] = "false"
				}
				used = true
			}
		}
		if !used {
			outputs = append(outputs, client.ExportEntry{Type: "image", Attrs: map[string]string{"push": "true", "unpack": "false"}})
		}
	}
	if load, _ := c.Flags().GetBool("load"); load {
		used := false
		for i := range outputs {
			if outputs[i].Type == "docker" {
				if _, ok := outputs[i].Attrs["dest"]; !ok {
					used = true
					break
				}
			}
		}
		if !used {
			outputs = append(outputs, client.ExportEntry{Type: "docker", Attrs: map[string]string{}})
		}
	}
	if err := build.ValidateLocalExportDelete(outputs, deleteOK); err != nil {
		return err
	}
	for _, o := range outputs {
		var keys []string
		for k := range o.Attrs {
			keys = append(keys, k+"="+o.Attrs[k])
		}
		sort.Strings(keys)
		fmt.Fprintf(out, "OUTPUT %s [%s] dir=%q file=%v\n", o.Type, strings.Join(keys, " "), o.OutputDir, o.Output != nil)
	}
	if len(granted) > 0 || deleteOK {
		fmt.Fprintf(out, "ALLOW %q local.delete=%v\n", granted, deleteOK)
	}
	if f := c.Flags().Lookup("ulimit"); f != nil && f.Changed {
		var list []string
		for _, u := range f.Value.(*dockeropts.UlimitOpt).GetList() {
			list = append(list, u.String())
		}
		fmt.Fprintf(out, "ULIMIT %s\n", strings.Join(list, ","))
	}
	// The annotations (ParseAnnotations), each type[platform] key=value, sorted.
	annArgs, _ := c.Flags().GetStringArray("annotation")
	anns, err := buildflags.ParseAnnotations(annArgs)
	if err != nil {
		return err
	}
	var annLines []string
	for k, v := range anns {
		p := ""
		if k.Platform != nil {
			p = "[" + platforms.Format(*k.Platform) + "]"
		}
		annLines = append(annLines, fmt.Sprintf("ANNOTATION %s%s %s=%s", k.Type, p, k.Key, v))
	}
	sort.Strings(annLines)
	for _, l := range annLines {
		fmt.Fprintln(out, l)
	}
	return nil
}

// shards names shards where buildx, as the CLI's plugin, names docker: its command path,
// its aliases, and the binary in its errors.
func shards(s string) string {
	return strings.NewReplacer(
		"docker buildx", "shards buildx",
		"docker build,", "shards build,",
		"docker builder build", "shards builder build",
		"docker image build", "shards image build",
		"docker: '", "shards: '",
	).Replace(s)
}

func contains(list []string, s string) bool {
	i := sort.SearchStrings(list, s)
	return i < len(list) && list[i] == s
}

func TestShardsOracle(t *testing.T) {
	out := os.Getenv("SHARDS_ORACLE_OUT")
	if out == "" {
		t.Skip("SHARDS_ORACLE_OUT names the file to write")
	}
	sort.Strings(served)
	config := t.TempDir()
	os.Clearenv()
	os.Setenv("DOCKER_CONFIG", config)
	os.Setenv("SHARDS_ORACLE_SECRET", "from the environment")
	dir := t.TempDir()
	for name, size := range map[string]int{"small.txt": -1, "edge.txt": 500 * 1024, "big.txt": 500*1024 + 1} {
		data := []byte("a small secret\n")
		if size >= 0 {
			data = bytes.Repeat([]byte{'s'}, size)
		}
		if err := os.WriteFile(filepath.Join(dir, name), data, 0o600); err != nil {
			t.Fatal(err)
		}
	}
	t.Chdir(dir)
	var answers []answer
	for _, argv := range cases {
		answers = append(answers, ask(t, argv))
	}
	data, err := json.MarshalIndent(answers, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
