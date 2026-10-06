package main

// buildx's own answers to `build` command lines, for shards-cmdline's tests to match
// (crates/cmdline/tests/buildx.rs). `generate` copies this file into buildx's cmd/buildx
// and runs it there. Each case runs as `docker build ...` runs buildx: as the CLI plugin,
// its root built by commands.NewRootCmd and wrapped by the CLI's plugin.RunPlugin, its
// errors printed and its exit status chosen as main does. The flags shards does not
// serve are hidden, and the build replaced by a line that says what it was asked.

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"

	"github.com/docker/buildx/build"
	"github.com/docker/buildx/commands"
	"github.com/docker/buildx/util/buildflags"
	"github.com/docker/buildx/util/cobrautil"
	dockeropts "github.com/docker/cli/opts"
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
	"allow", "build-arg", "file", "help", "iidfile", "label", "load", "no-cache", "platform",
	"progress", "pull", "quiet", "secret", "ssh", "tag", "target", "ulimit",
}

// The command lines asked, each the words after `buildx build`.
var cases = [][]string{
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
	var stdout, stderr bytes.Buffer
	dockerCli, err := command.NewDockerCli(command.WithOutputStream(&stdout), command.WithErrorStream(&stderr))
	if err != nil {
		t.Fatal(err)
	}
	rootCmd := commands.NewRootCmd("buildx", true, dockerCli)
	var build *cobra.Command
	for _, c := range rootCmd.Commands() {
		if c.Name() == "build" {
			build = c
		}
	}
	if build == nil {
		t.Fatal("no build command")
	}
	hide := func(f *pflag.Flag) {
		if !contains(served, f.Name) {
			f.Hidden = true
		}
	}
	build.Flags().VisitAll(hide)
	rootCmd.PersistentFlags().VisitAll(hide)
	build.PreRunE, build.PreRun = nil, nil
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
		return built(c)
	}
	// As the CLI execs a plugin: its path, then its name, then the words.
	os.Args = append([]string{"docker-buildx", "buildx", "build"}, argv...)
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
	return answer{argv, shards(stdout.String()), shards(stderr.String()), status}
}

// built says what buildx's build makes of the secrets, ulimits and entitlements it was
// given, as runBuild meets them: the secrets' specs (toControllerOptions), their store
// (CreateSecrets, BuildKit's secretsprovider.NewStore), then the entitlements
// (ParseEntitlements); the ulimits as the frontend's `ulimit` option carries them.
func built(c *cobra.Command) error {
	out := c.OutOrStdout()
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
