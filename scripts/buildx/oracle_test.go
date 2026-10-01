package main

// buildx's own answers to `build` command lines, for shards-cmdline's tests to match
// (crates/cmdline/tests/buildx.rs). `generate` copies this file into buildx's cmd/buildx
// and runs it there. Each case runs as `docker build ...` runs buildx: as the CLI plugin,
// its root built by commands.NewRootCmd and wrapped by the CLI's plugin.RunPlugin, its
// errors printed and its exit status chosen as main does. The flags shards does not
// serve are hidden, and the build replaced by a line that says what it was asked.

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sort"
	"strings"
	"testing"

	"github.com/docker/buildx/commands"
	"github.com/docker/buildx/util/cobrautil"
	"github.com/docker/cli/cli"
	"github.com/docker/cli/cli-plugins/metadata"
	"github.com/docker/cli/cli-plugins/plugin"
	"github.com/docker/cli/cli/command"
	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// The flags shards serves.
var served = []string{
	"build-arg", "file", "help", "iidfile", "label", "load", "no-cache", "platform",
	"progress", "pull", "quiet", "tag", "target",
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
		return nil
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
