package main

// The Docker CLI's own answers to command lines, for shards-cmdline's tests to match
// (crates/cmdline/tests/docker_cli.rs). `generate` copies this file into docker/cli's
// cmd/docker and runs it there, against the CLI's real command tree: its root set up as
// the CLI sets it up (cli.SetupRootCommand), its commands as the CLI adds them, named
// `shards`, with the flags shards does not serve hidden (as dockerd's unsupported ones
// are), and each command's work replaced by a line that says what it was asked.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"
	"testing"

	"github.com/docker/cli/cli"
	"github.com/docker/cli/cli/command"
	"github.com/docker/cli/cli/command/commands"
	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// The flags shards serves, by command; the rest are hidden, as shards hides them.
var served = map[string][]string{
	"run": {"add-host", "blkio-weight", "cap-add", "cap-drop", "cidfile", "cpu-period", "cpu-quota", "cpu-shares", "cpus", "cpuset-cpus", "cpuset-mems", "detach", "detach-keys", "disable-content-trust", "dns", "dns-opt", "dns-option", "dns-search", "domainname", "entrypoint", "env", "env-file", "expose", "group-add", "health-cmd", "health-interval", "health-retries", "health-start-interval", "health-start-period", "health-timeout", "help", "hostname", "init", "interactive", "kernel-memory", "label", "label-file", "memory", "memory-reservation", "memory-swap", "memory-swappiness", "mount", "name", "net", "network", "no-healthcheck", "oom-kill-disable", "oom-score-adj", "pids-limit", "platform", "privileged", "publish", "publish-all", "pull", "quiet", "read-only", "restart", "rm", "security-opt", "shm-size", "sig-proxy", "stop-signal", "stop-timeout", "sysctl", "tmpfs", "tty", "ulimit", "user", "volume", "volume-driver", "volumes-from", "workdir"},
	"ps":   {"all", "filter", "format", "help", "last", "latest", "no-trunc", "quiet", "size"},
	"ls":   {"all", "filter", "format", "help", "last", "latest", "no-trunc", "quiet", "size"},
	"wait": {"help"},
	"logs": {"details", "follow", "help", "since", "tail", "timestamps", "until"},
	"rm":   {"force", "help", "volumes"},
	"stop": {"help", "signal", "time", "timeout"},
	"kill": {"help", "signal"},
	"port": {"help"},
	"images": {"all", "digests", "filter", "format", "help", "no-trunc", "quiet", "tree"},
	"tag":    {"help"},
	"rmi":    {"force", "help", "no-prune"},
	"image ls": {"all", "digests", "filter", "format", "help", "no-trunc", "quiet", "tree"},
	"image rm": {"force", "help", "no-prune"},
	"image tag": {"help"},
	"image inspect": {"format", "help"},
	"version":       {"format", "help"},
	"df":            {"format", "help", "verbose"},
	"save":          {"help", "output"},
	"image save":    {"help", "output"},
	"load":          {"help", "input", "quiet"},
	"image load":    {"help", "input", "quiet"},
	"pull":          {"all-tags", "disable-content-trust", "help", "platform", "quiet"},
	"image pull":    {"all-tags", "disable-content-trust", "help", "platform", "quiet"},
	"push":          {"all-tags", "disable-content-trust", "help", "quiet"},
	"image push":    {"all-tags", "disable-content-trust", "help", "quiet"},
	"exec": {"detach", "detach-keys", "env", "env-file", "help", "interactive", "privileged", "tty", "user", "workdir"},
	"stats":           {"all", "format", "help", "no-stream", "no-trunc"},
	"container stats": {"all", "format", "help", "no-stream", "no-trunc"},
	"pause":             {"help"},
	"container pause":   {"help"},
	"unpause":           {"help"},
	"container unpause": {"help"},
	"top":               {"help"},
	"container top":     {"help"},
	"diff":              {"help"},
	"container diff":    {"help"},
	"events":            {"filter", "format", "help", "since", "until"},
	"system events":     {"filter", "format", "help", "since", "until"},
	"export":            {"help", "output"},
	"container export":  {"help", "output"},
	"info":              {"format", "help"},
	// `container inspect` (and the top-level inspect, whose own flags shards does not
	// serve yet).
	"inspect": {"format", "help", "size", "type"},
	// `container prune` and `system prune`, which share the name.
	"prune":             {"all", "filter", "force", "help", "volumes"},
	"image prune":       {"all", "filter", "force", "help"},
	"volume create":     {"driver", "help", "label", "name", "opt"},
	"import":            {"change", "help", "message", "platform"},
	"image import":      {"change", "help", "message", "platform"},
	"update":            {"blkio-weight", "cpu-period", "cpu-quota", "cpu-shares", "cpus", "cpuset-cpus", "cpuset-mems", "help", "kernel-memory", "memory", "memory-reservation", "memory-swap", "pids-limit", "restart"},
	"volume ls":         {"filter", "format", "help", "quiet"},
	"volume inspect":    {"format", "help"},
	"volume rm":         {"force", "help"},
	"volume prune":      {"all", "filter", "force", "help"},
	"create": {"add-host", "blkio-weight", "cap-add", "cap-drop", "cidfile", "cpu-period", "cpu-quota", "cpu-shares", "cpus", "cpuset-cpus", "cpuset-mems", "disable-content-trust", "dns", "dns-opt", "dns-option", "dns-search", "domainname", "entrypoint", "env", "env-file", "expose", "group-add", "health-cmd", "health-interval", "health-retries", "health-start-interval", "health-start-period", "health-timeout", "help", "hostname", "init", "interactive", "kernel-memory", "label", "label-file", "memory", "memory-reservation", "memory-swap", "memory-swappiness", "mount", "name", "net", "network", "no-healthcheck", "oom-kill-disable", "oom-score-adj", "pids-limit", "platform", "privileged", "publish", "publish-all", "pull", "quiet", "read-only", "restart", "rm", "security-opt", "shm-size", "stop-signal", "stop-timeout", "sysctl", "tmpfs", "tty", "ulimit", "user", "volume", "volume-driver", "volumes-from", "workdir"},
	"container create": {
		"disable-content-trust", "entrypoint", "env", "help",
		"hostname", "init", "interactive", "kernel-memory", "name", "net", "network", "pull",
		"health-cmd", "health-interval", "health-retries", "health-start-interval",
		"health-start-period", "health-timeout", "no-healthcheck", "publish", "publish-all",
		"rm", "stop-signal",
		"stop-timeout", "tty", "user", "workdir",
	},
	"start":             {"attach", "detach-keys", "help", "interactive"},
	"container start":   {"attach", "detach-keys", "help", "interactive"},
	"restart":           {"help", "signal", "time", "timeout"},
	"container restart": {"help", "signal", "time", "timeout"},
	"commit":            {"author", "change", "help", "message", "no-pause", "pause"},
	"container commit":  {"author", "change", "help", "message", "no-pause", "pause"},
	"cp":                {"archive", "follow-link", "help", "quiet"},
	"container cp":      {"archive", "follow-link", "help", "quiet"},
}

// The command lines asked, each the words after `shards`.
var cases = [][]string{
	{"stats", "--help"},
	{"stats", "-h"},
	{"stats", "--nope"},
	{"stats", "--format", "x"},
	{"container", "stats", "--help"},
	{"pause"},
	{"pause", "--help"},
	{"pause", "--nope", "a"},
	{"container", "pause", "-h"},
	{"unpause"},
	{"unpause", "--help"},
	{"container", "unpause", "a", "--nope"},
	{"top"},
	{"top", "--help"},
	{"top", "-h"},
	{"top", "--nope"},
	{"container", "top", "--help"},
	{"diff"},
	{"diff", "a", "b"},
	{"diff", "--help"},
	{"container", "diff", "-h"},
	{"events", "--help"},
	{"events", "x"},
	{"events", "--filter", "nope"},
	{"events", "-f", "a=b", "--since"},
	{"system", "events", "-h"},
	{"export"},
	{"export", "--help"},
	{"export", "-o"},
	{"container", "export", "-h"},
	{"info", "--help"},
	{"info", "x"},
	{"system", "info", "-h"},
	{"info", "--help"},
	{"info", "-f", "{{.ID}}"},
	{"system", "info", "--format=json"},
	{"system", "df", "--help"},
	{"system", "df", "-v", "--format", "{{.Type}}"},
	{"system", "df", "x"},
	{"version", "--help"},
	{"version", "extra"},
	{"version", "-f"},
	{"inspect", "--help"},
	{"inspect"},
	{"inspect", "--type", "container", "-f", "{{.Id}}", "x"},
	{"container", "inspect", "-h"},
	{"container", "inspect", "-f", "{{.Id}}", "x", "y"},
	{"container", "inspect", "--format=json", "x"},
	{"container", "inspect"},
	{"container", "prune", "--help"},
	{"container", "prune", "-f", "--filter", "until=1h"},
	{"image", "prune", "-h"},
	{"import", "--help"},
	{"image", "import", "-c", "ENV A=1", "-c", "CMD x", "-m", "msg", "--platform", "linux/arm64", "f.tar", "repo:tag"},
	{"import"},
	{"update", "--help"},
	{"update", "-m", "64m", "--memory-swap", "-1", "--cpus", "1.5", "-c", "512", "--blkio-weight", "300", "--pids-limit", "50", "--restart", "on-failure:3", "a", "b"},
	{"update", "--blkio-weight", "70000", "a"},
	{"update", "--kernel-memory", "10m", "a"},
	{"container", "update", "--cpu-rt-period", "1000", "a"},
	{"update"},
	{"volume", "create", "--help"},
	{"volume", "create", "-d", "local", "--label", "a=b", "-o", "type=tmpfs", "-o", "device=tmpfs", "v1"},
	{"volume", "create", "--name", "n", "v"},
	{"volume", "create", "a", "b"},
	{"volume", "create", "--scope", "multi"},
	{"volume", "ls", "--help"},
	{"volume", "list", "-q", "-f", "dangling=true", "--format", "{{.Name}}"},
	{"volume", "ls", "x"},
	{"volume", "ls", "--cluster"},
	{"volume", "inspect", "--help"},
	{"volume", "inspect", "-f", "{{.Name}}", "a", "b"},
	{"volume", "inspect"},
	{"volume", "rm", "--help"},
	{"volume", "remove", "-f", "a"},
	{"volume", "rm"},
	{"volume", "prune", "--help"},
	{"volume", "prune", "-af", "--filter", "label=a"},
	{"volume", "prune", "x"},
	{"image", "prune", "-af", "--filter", "label=a", "--filter", "label!=b"},
	{"system", "prune", "--help"},
	{"system", "prune", "--filter", "nope"},
	{"start"},
	{"start", "--help"},
	{"start", "-a", "--nope", "x"},
	{"container", "start", "-h"},
	{"restart", "--help"},
	{"restart", "-t", "x", "a"},
	{"restart", "--time", "1", "--timeout", "2", "a"},
	{"container", "restart", "-h"},
	{"create", "--help"},
	{"create"},
	{"create", "-d", "alpine"},
	{"container", "create", "-h"},
	{"commit"},
	{"commit", "--help"},
	{"commit", "a", "b", "c"},
	{"commit", "-p=false", "x"},
	{"container", "commit", "-h"},
	{"cp"},
	{"cp", "--help"},
	{"cp", "a", "b", "c"},
	{"cp", "-x", "a:/x", "b"},
	{"container", "cp", "-h"},
	{"run"},
	{"run", "--label", "a=1", "-l", "b", "--label-file", "f", "alpine"},
	{"run", "--label", "=x", "alpine"},
	{"run", "--label", "a b=1", "alpine"},
	{"run", "--dns", " 1.1.1.1 ", "--dns", "::ffff:8.8.8.8", "alpine"},
	{"run", "--dns", "nope", "alpine"},
	{"run", "--dns-search", "example.com.", "--dns-search", ".", "alpine"},
	{"run", "--dns-search", "-bad", "alpine"},
	{"run", "--dns-search", "123", "alpine"},
	{"run", "--dns-option", "ndots:2", "--dns-opt", "edns0", "alpine"},
	{"run", "--add-host", "a=1.2.3.4", "--add-host", "b:[::1]", "--add-host", "c:host-gateway", "alpine"},
	{"run", "--add-host", "a", "alpine"},
	{"run", "--add-host", "a:nope", "alpine"},
	{"run", "--domainname", "example.org", "--expose", "80", "--expose", "7000-7002/udp", "alpine"},
	{"run", "--env-file", "a.env", "alpine"},
	{"run", "-q", "--sig-proxy=false", "--cidfile", "/tmp/cid", "--platform", "linux/amd64", "alpine"},
	{"run", "--sig-proxy", "x", "alpine"},
	{"run", "-m", "1.5g", "--memory-reservation", "512m", "--memory-swap", "-1", "--cpus", "1.5", "-c", "512", "--cpuset-cpus", "0-1", "--pids-limit", "100", "alpine"},
	{"run", "--memory", "32", "--memory-swap", "64MiB", "--cpus", "3/2", "alpine"},
	{"run", "-m", "32.3 MiB", "--cpus", "0x1p-1", "alpine"},
	{"run", "-m", "0.3 MB", "--cpus", "1e-9", "alpine"},
	{"run", "-m", "1_024k", "--cpus", "2.5e1", "alpine"},
	{"run", "-m", "1023.99999k", "alpine"},
	{"run", "-m", "32m b", "alpine"},
	{"run", "-m", "32 mbmb", "alpine"},
	{"run", "-m", "32 MBMB", "alpine"},
	{"run", "-m", "32bm", "alpine"},
	{"run", "-m", "32Xb", "alpine"},
	{"run", "-m", "-32", "alpine"},
	{"run", "-m", "hello", "alpine"},
	{"run", "-m", "", "alpine"},
	{"run", "-m", "1e400", "alpine"},
	{"run", "-m", "0x10", "alpine"},
	{"run", "-m", "0x1p4k", "alpine"},
	{"run", "-m", "inf b", "alpine"},
	{"run", "--memory-swap", "-2", "alpine"},
	{"run", "--cpus", "1/3", "alpine"},
	{"run", "--cpus", "x", "alpine"},
	{"run", "--cpus", "1/0", "alpine"},
	{"run", "--cpus", "-1", "alpine"},
	{"run", "--cpus", "0b1.1", "alpine"},
	{"run", "--cpus", "010/2", "alpine"},
	{"run", "--cpus", "1_0", "alpine"},
	{"run", "--cpus", "1.9995", "alpine"},
	{"run", "--cpus", "0.0005", "alpine"},
	{"run", "--cpus", "1e30", "alpine"},
	{"run", "--cpus", "0x1.8", "alpine"},
	{"run", "--cpus", "1p1", "alpine"},
	{"run", "--cpus", ".5", "alpine"},
	{"run", "--cpus", "5.", "alpine"},
	{"run", "--cpus", "+0", "alpine"},
	{"run", "--cpus", "1e-10", "alpine"},
	{"run", "--memory-swappiness", "101", "alpine"},
	{"run", "--oom-kill-disable", "--cpu-period", "50000", "--cpu-quota", "25000", "--cpuset-mems", "0", "alpine"},
	{"create", "-m", "64m", "--cpus", "2", "alpine"},
	{"run", "--read-only", "--tmpfs", "/run:size=1m,exec", "--tmpfs", "/tmp", "--shm-size", "128m", "--ulimit", "nofile=1024:2048", "--ulimit", "nproc=10", "--sysctl", "net.core.somaxconn=1024", "--sysctl", "kernel.shmmax=1", "--sysctl", "net.core.somaxconn=2048", "alpine"},
	{"run", "--sysctl", "kernel.hostname=x", "alpine"},
	{"run", "--sysctl", "net", "alpine"},
	{"run", "--sysctl", "=1", "alpine"},
	{"run", "--sysctl", "fs.mqueue.msg_max=10", "--sysctl", "kernel.sem=1 2 3 4", "alpine"},
	{"run", "--ulimit", "as=1", "alpine"},
	{"run", "--ulimit", "nofile=2:1", "alpine"},
	{"run", "--ulimit", "nofile", "alpine"},
	{"run", "--ulimit", "nofile=x", "alpine"},
	{"run", "--ulimit", "nofile=-1:5", "alpine"},
	{"run", "--shm-size", "x", "alpine"},
	{"run", "--shm-size", "1g", "alpine"},
	{"run", "--cap-add", "net_admin", "--cap-add", "ALL", "--cap-drop", "chown", "--group-add", "audio", "--group-add", "1234", "--oom-score-adj", "-500", "--privileged", "alpine"},
	{"run", "--oom-score-adj", "x", "alpine"},
	{"run", "-v", "/h:/c:ro", "-v", "name:/d", "-v", "/anon", "--mount", "type=bind,src=/a,dst=/b,readonly", "--mount", "type=tmpfs,dst=/t,tmpfs-size=64m", "--volumes-from", "web:ro", "--volume-driver", "local", "alpine"},
	{"run", "--mount", "", "alpine"},
	{"run", "--mount", "type=bind,src", "alpine"},
	{"run", "--mount", "type=volume,bind-propagation=shared", "alpine"},
	{"run", "--mount", "type=bind,ro=maybe", "alpine"},
	{"run", "--mount", "type=bind,nope=1", "alpine"},
	{"run", "--mount", "type=tmpfs,tmpfs-size=x", "alpine"},
	{"run", "--mount", "type=tmpfs,tmpfs-mode=9", "alpine"},
	{"run", "--mount", "type=bind,bind-nonrecursive", "alpine"},
	{"run", "--mount", "type=bind,src= /a", "alpine"},
	{"run", "--mount", "type=bind, src=/a", "alpine"},
	{"run", "--mount", "type=bind,bind-recursive=writable", "alpine"},
	{"run", "--mount", "type=bind,bind-recursive=readonly,readonly", "alpine"},
	{"run", "--mount", "type=bind,bind-recursive=nope", "alpine"},
	{"run", "--mount", "type=bind,src=\"a,b\",dst=/x", "alpine"},
	{"run", "--mount", "type=bind,src=\"a", "alpine"},
	{"run", "--mount", "src=/a,dst=/b,volume-label=x=1,volume-opt=o=2,volume-driver=local,volume-nocopy,volume-subpath=s", "alpine"},
	{"run", "--mount", "type=,dst=/b", "alpine"},
	{"exec", "--privileged", "--env-file", "a.env", "web", "ls"},
	{"create", "-q", "--cidfile", "c", "--platform", "arm64", "alpine"},
	{"run", "--help"},
	{"run", "-h"},
	{"run", "--nope", "alpine"},
	{"run", "alpine"},
	{"run", "alpine", "--rm", "-d"},
	{"run", "-d", "--rm", "--name", "web", "alpine", "sleep", "1"},
	{"run", "-di", "-eA=1", "--name=web", "--pull=never", "alpine"},
	{"run", "-e", "A=1", "-e", "B", "-e", "UNSET", "alpine", "env"},
	{"run", "-e", "=x", "alpine"},
	{"run", "--network", "none", "alpine"},
	{"run", "--health-cmd", "true", "--health-interval", "1m30.5s", "--health-retries", "2", "--health-timeout=500ms", "--health-start-period", "1h", "--health-start-interval", "2s", "alpine"},
	{"run", "--health-interval", "x", "alpine"},
	{"run", "--health-interval", "1", "alpine"},
	{"run", "--health-timeout", "1q", "alpine"},
	{"run", "--health-interval=-1s", "alpine"},
	{"run", "--no-healthcheck", "alpine"},
	{"run", "-p", "8080:80", "-p", "127.0.0.1::81/udp", "-P", "alpine"},
	{"run", "--publish=80", "--publish-all=false", "alpine"},
	{"run", "-p"},
	{"rmi"},
	{"rmi", "--help"},
	{"image", "rm", "-f", "--no-prune", "a", "b"},
	{"image", "remove", "a"},
	{"image", "rm", "--help"},
	{"image", "ls", "--help"},
	{"image", "tag", "--help"},
	{"pull", "--help"},
	{"pull"},
	{"pull", "a", "b"},
	{"image", "pull", "-a", "-q", "x"},
	{"pull", "--platform", "linux/arm64", "x"},
	{"pull", "--platform"},
	{"pull", "--disable-content-trust", "x"},
	{"image", "pull", "--help"},
	{"push", "--help"},
	{"push"},
	{"push", "a", "b"},
	{"image", "push", "-a", "-q", "x"},
	{"push", "--platform", "linux/arm64", "x"},
	{"push", "--disable-content-trust=false", "x"},
	{"load", "--help"},
	{"load", "x"},
	{"image", "load", "-q", "-i", "a.tar"},
	{"load", "--platform", "linux/arm64"},
	{"save"},
	{"save", "--help"},
	{"save", "-o", "x.tar", "a", "b"},
	{"image", "save", "--output=y", "a"},
	{"save", "--platform", "linux/arm64", "a"},
	{"image", "inspect"},
	{"image", "inspect", "--help"},
	{"image", "inspect", "a", "b"},
	{"image", "inspect", "-f", "{{.Id}}", "a"},
	{"rmi", "--platform", "linux/amd64", "a"},
	{"tag"},
	{"tag", "--help"},
	{"tag", "a"},
	{"tag", "a", "b", "c"},
	{"image", "tag", "a", "b"},
	{"tag", "a", "--", "b"},
	{"images", "--help"},
	{"images", "a", "b"},
	{"images", "-aq", "--digests", "--no-trunc", "--tree", "alpine"},
	{"image", "ls", "--format", "x"},
	{"images", "-f", "dangling=true", "-f", "reference=alp*"},
	{"images", "--filter", "x"},
	{"image", "list", "-f", "dangling=true"},
	{"port"},
	{"port", "--help"},
	{"port", "a", "b", "c"},
	{"port", "a", "80/udp"},
	{"container", "port", "a"},
	{"port", "--bogus", "a"},
	{"exec"},
	{"exec", "web"},
	{"exec", "--help"},
	{"exec", "-it", "-e", "A=1", "-e", "B", "-u", "1000:1000", "-w", "/w", "web", "sh", "-c", "env"},
	{"exec", "-d", "web", "true", "-i"},
	{"exec", "--privileged", "web", "true"},
	{"exec", "--env-file", "f", "web", "true"},
	{"container", "exec", "-t", "web", "top"},
	{"exec", "--nope", "web", "true"},
	{"run", "--stop-signal", "SIGUSR1", "--stop-timeout", "3", "alpine"},
	{"run", "--stop-timeout", "x", "alpine"},
	{"run", "--stop-timeout=-1", "alpine"},
	{"run", "--net=none", "--network", "bridge", "alpine"},
	{"run", "--network=name=bridge,alias=x", "alpine"},
	{"run", "--network", "a=b", "alpine"},
	{"run", "--net", "name=x,ip=y", "alpine"},
	{"run", "--network", "\"name=a", "alpine"},
	{"run", "--network"},
	{"run", "--network=", "alpine"},
	{"run", "-e"},
	{"run", "-it", "alpine", "sh"},
	{"run", "-t", "alpine"},
	{"run", "-ti", "--detach-keys", "ctrl-a,x", "alpine"},
	{"run", "--tty=false", "--detach-keys=", "alpine"},
	{"run", "-dt", "alpine", "top"},
	{"run", "--detach-keys"},
	{"run", "-p", "80:80", "-v", "/a:/b", "nginx"},
	{"run", "--sig-proxy=true", "alpine"},
	{"run", "--sig-proxy=false", "alpine"},
	{"run", "--init", "--init=false", "alpine"},
	{"run", "--disable-content-trust", "alpine"},
	{"run", "--kernel-memory", "0", "alpine"},
	{"run", "-h", "host", "-u", "1000:1000", "-w", "/srv", "alpine", "pwd"},
	{"run", "--entrypoint=", "alpine"},
	{"run", "--entrypoint", "", "alpine", "x"},
	{"run", "--", "alpine", "ls"},
	{"run", "--name"},
	{"run", "--cpu-shares", "x", "alpine"},
	{"run", "--memory-swappiness", "-1", "alpine"},
	{"run", "--stop-timeout", "0x0", "alpine"},
	{"ps"},
	{"ps", "--help"},
	{"ps", "-h"},
	{"ps", "-h", "extra"},
	{"ps", "extra"},
	{"ps", "-aq"},
	{"ps", "-a", "-q", "--no-trunc"},
	{"ps", "-n", "3"},
	{"ps", "-n=3"},
	{"ps", "-f", "nope"},
	{"ps", "--filter", "status=exited", "-q"},
	{"ps", "-f", "name=a", "-f", "label=x=1"},
	{"ps", "-f", " Name = b<&> ", "-f", "name=a", "-f", "", "-a"},
	{"ps", "-n3"},
	{"ps", "--last=0x10"},
	{"ps", "-n", "x"},
	{"ps", "-n", "08"},
	{"ps", "-n", "99999999999999999999"},
	{"ps", "-n"},
	{"ps", "--last"},
	{"ps", "--all=false"},
	{"ps", "-a=false"},
	{"ps", "-a="},
	{"ps", "--all=maybe"},
	{"ps", "-l"},
	{"ps", "--format", "{{.ID}}"},
	{"ps", "-s", "-f", "name=x"},
	{"ps", "-test.v"},
	{"ps", "-atest.v"},
	{"ps", "-a", "--", "-q"},
	{"ps", "---x"},
	{"ps", "--=x"},
	{"ps", "--nope=1"},
	{"ps", "-Z"},
	{"ps", "-aZq"},
	{"ps", "-é"},
	{"ps", "-aé"},
	{"ps", "-"},
	{"container", "ls", "-a"},
	{"container", "ps", "--nope"},
	{"container", "list", "--help"},
	{"container", "ls", "x"},
	{"wait"},
	{"wait", "--help"},
	{"wait", "a", "b"},
	{"wait", "-x", "a"},
	{"container", "wait"},
	{"logs"},
	{"logs", "--help"},
	{"logs", "a", "b"},
	{"logs", "-f", "a"},
	{"logs", "-n", "5", "a"},
	{"logs", "--tail", "all", "-t", "--details", "a"},
	{"logs", "--since", "10m", "a"},
	{"logs", "--until=2013-01-02T13:23:37Z", "--since", "", "a"},
	{"logs", "--since"},
	{"logs", "a", "-ft"},
	{"container", "logs"},
	{"rm"},
	{"rm", "--help"},
	{"rm", "-f", "a"},
	{"rm", "-fv", "a", "b"},
	{"rm", "-l", "a"},
	{"rm", "--link=false", "a"},
	{"container", "rm"},
	{"container", "remove", "-f", "a"},
	{"container", "remove", "--help"},
	{"stop"},
	{"stop", "--help"},
	{"stop", "-h"},
	{"stop", "a"},
	{"stop", "-t", "5", "a", "b"},
	{"stop", "--time", "5", "a"},
	{"stop", "--time=5", "--timeout=3", "a"},
	{"stop", "-s", "KILL", "a"},
	{"stop", "-t", "x", "a"},
	{"stop", "a", "-t", "-1"},
	{"stop", "--", "-a"},
	{"stop", "-t"},
	{"stop", "--time", "5"},
	{"stop", "--time", "5", "--help"},
	{"container", "stop", "--nope"},
	{"kill"},
	{"kill", "--help"},
	{"kill", "-s"},
	{"kill", "-s", "HUP", "a"},
	{"kill", "--signal=9", "a", "b"},
	{"container", "kill"},
}

type answer struct {
	Argv   []string `json:"argv"`
	Stdout string   `json:"stdout"`
	Stderr string   `json:"stderr"`
	Status int      `json:"status"`
}

// root builds the CLI's command tree afresh, flags unset, as `shards`.
func root(t *testing.T, stdout, stderr *bytes.Buffer) *cobra.Command {
	dockerCli, err := command.NewDockerCli(command.WithOutputStream(stdout), command.WithErrorStream(stderr))
	if err != nil {
		t.Fatal(err)
	}
	cmd := &cobra.Command{
		Use:                   "shards [OPTIONS] COMMAND [ARG...]",
		SilenceUsage:          true,
		SilenceErrors:         true,
		TraverseChildren:      true,
		DisableFlagsInUseLine: true,
	}
	cmd.SetOut(stdout)
	cmd.SetErr(stderr)
	cli.SetupRootCommand(cmd)
	commands.AddCommands(cmd, dockerCli)
	var visit func(c *cobra.Command)
	visit = func(c *cobra.Command) {
		for _, sub := range c.Commands() {
			visit(sub)
		}
		if a := c.Annotations["aliases"]; a != "" {
			c.Annotations["aliases"] = strings.ReplaceAll(a, "docker ", "shards ")
		}
		if c.Parent() == nil {
			return
		}
		// `image ls` and `image rm` share their names with container commands.
		key := c.Name()
		if p := c.Parent().Name(); p == "image" || p == "volume" {
			key = p + " " + key
		}
		keep, ok := served[key]
		if !ok {
			return
		}
		c.Flags().VisitAll(func(f *pflag.Flag) {
			if !contains(keep, f.Name) {
				f.Hidden = true
			}
		})
		c.PreRunE, c.PreRun = nil, nil
		c.RunE = func(c *cobra.Command, args []string) error {
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
	}
	visit(cmd)
	return cmd
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
	for _, keep := range served {
		sort.Strings(keep)
	}
	// What `-e B` picks up from the environment, and what `-e UNSET` does not.
	os.Clearenv()
	os.Setenv("B", "from-env")
	var answers []answer
	for _, argv := range cases {
		var stdout, stderr bytes.Buffer
		cmd := root(t, &stdout, &stderr)
		cmd.SetArgs(argv)
		status := 0
		// As main does (cmd/docker/docker.go).
		if _, err := cmd.ExecuteC(); err != nil {
			if err.Error() != "" {
				fmt.Fprintln(&stderr, err)
			}
			status = getExitCode(err)
		}
		answers = append(answers, answer{argv, stdout.String(), stderr.String(), status})
	}
	data, err := json.MarshalIndent(answers, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
