package container

// The Engine API's create bodies, as docker/cli's `run` makes them (parse, over the
// daemon's OS, linux): for each command line, the CreateRequest its client posts to
// /containers/create, or the error the CLI refuses it with. generate copies it into
// cli/command/container, beside the addFlags and parse it uses, and writes
// crates/shards/src/daemon/engine/testdata/create-bodies.json, which shards' translation of a
// body into a run is held to: each body's run is the run shards' CLI makes of the same
// command line (D170).

import (
	"encoding/json"
	"io"
	"os"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/spf13/pflag"
)

// The command lines: each flag that changes the body, alone or with the others it goes
// with. Files and the process's environment are left out (--env-file, --label-file,
// --seccomp=FILE, `-e NAME`), which the tests that read the bodies could not give alike.
var engineCases = [][]string{
	{"img"},
	{"img", "echo", "hi"},
	{"-e", "A=1", "-e", "C=two words", "img"},
	{"-l", "a=b", "--label", "c", "img"},
	{"-p", "80", "-p", "8080:80/udp", "-p", "127.0.0.1:9000:9000", "-p", "[::1]:7000:7000", "img"},
	{"-p", "3000-3002:4000-4002", "img"},
	{"-P", "--expose", "1000-1002", "--expose", "53/udp", "img"},
	{"-v", "/host:/ctr:ro", "-v", "named:/data", "-v", "/anon", "img"},
	{"--mount", "type=bind,source=/src,target=/dst,readonly", "img"},
	{"--mount", "type=volume,source=vol,target=/v,volume-opt=a=b,volume-nocopy", "img"},
	{"--mount", "type=tmpfs,target=/t,tmpfs-size=1m,tmpfs-mode=1700", "img"},
	{"--tmpfs", "/run:rw,size=64m", "--tmpfs", "/tmp", "img"},
	{"--read-only", "img"},
	{"-u", "1000:1000", "-w", "/work", "--hostname", "h", "--domainname", "d.example", "img"},
	{"--entrypoint", "/bin/sh", "img", "-c", "true"},
	{"--entrypoint", "", "img"},
	{"-t", "-i", "img"},
	{"-a", "stdout", "-a", "stderr", "img"},
	{"-a", "stdin", "-i", "img"},
	{"--rm", "img"},
	{"--restart", "on-failure:3", "img"},
	{"--restart", "always", "img"},
	{"--restart", "unless-stopped", "img"},
	{"--network", "none", "img"},
	{"--network", "host", "img"},
	{"--network", "mynet", "--ip", "10.1.0.5", "--ip6", "fd00::5", "--network-alias", "a", "img"},
	{"--network", "name=n1,alias=x,ip=10.2.0.2", "--network", "n2", "img"},
	{"--network", "container:other", "img"},
	{"--link-local-ip", "169.254.1.1", "--network", "mynet", "img"},
	{"--mac-address", "02:42:ac:11:00:02", "img"},
	{"--dns", "1.1.1.1", "--dns-search", "example.com", "--dns-option", "ndots:2", "img"},
	{"--add-host", "h:1.2.3.4", "--add-host", "g:host-gateway", "img"},
	{"--cap-add", "NET_ADMIN", "--cap-drop", "ALL", "img"},
	{"--privileged", "img"},
	{"--security-opt", "no-new-privileges", "--security-opt", "seccomp=unconfined", "img"},
	{"--security-opt", "apparmor=unconfined", "--security-opt", "label=disable", "img"},
	{"--sysctl", "net.ipv4.ip_forward=1", "--sysctl", "kernel.msgmax=65536", "img"},
	{"--ulimit", "nofile=1024:2048", "--ulimit", "nproc=64", "img"},
	{"--memory", "512m", "--memory-swap", "1g", "--memory-reservation", "256m", "--memory-swappiness", "10", "--oom-kill-disable", "img"},
	{"--cpus", "1.5", "img"},
	{"--cpu-shares", "512", "--cpu-period", "100000", "--cpu-quota", "50000", "--cpuset-cpus", "0-1", "--cpuset-mems", "0", "img"},
	{"--cpu-rt-period", "1000000", "--cpu-rt-runtime", "950000", "img"},
	{"--cpu-count", "2", "--cpu-percent", "50", "img"},
	{"--pids-limit", "100", "img"},
	{"--blkio-weight", "300", "--blkio-weight-device", "/dev/sda:200", "img"},
	{"--device-read-bps", "/dev/sda:1mb", "--device-write-bps", "/dev/sda:2mb", "--device-read-iops", "/dev/sda:100", "--device-write-iops", "/dev/sda:200", "img"},
	{"--io-maxbandwidth", "1mb", "--io-maxiops", "100", "img"},
	{"--device", "/dev/fuse", "--device", "/dev/sda:/dev/xvda:rwm", "--device-cgroup-rule", "c 1:3 mr", "img"},
	{"--gpus", "all", "img"},
	{"--gpus", "\"device=0,1\",capabilities=compute", "img"},
	{"--gpus", "\"capabilities=gpu,utility\",driver=nvidia,count=2", "img"},
	{"--device", "vendor.com/gpu=0", "img"},
	{"--security-opt", "systempaths=unconfined", "img"},
	{"--ip", "10.0.0.5", "img"},
	{"--umask", "0027", "img"},
	{"--group-add", "audio", "--group-add", "1234", "img"},
	{"--ipc", "shareable", "--pid", "host", "--uts", "host", "img"},
	{"--userns", "host", "--cgroupns", "private", "img"},
	{"--cgroup-parent", "agents-ci.slice", "img"},
	{"--init", "img"},
	{"--oom-score-adj", "100", "--shm-size", "64m", "img"},
	{"--stop-signal", "SIGINT", "--stop-timeout", "5", "img"},
	{"--health-cmd", "true", "--health-interval", "5s", "--health-retries", "3", "--health-timeout", "2s", "--health-start-period", "1s", "--health-start-interval", "1s", "img"},
	{"--no-healthcheck", "img"},
	{"--log-driver", "json-file", "--log-opt", "max-size=1m", "--log-opt", "max-file=2", "img"},
	{"--runtime", "runc", "--isolation", "default", "img"},
	{"--storage-opt", "size=1G", "img"},
	{"--volumes-from", "other:ro", "--volume-driver", "local", "img"},
	{"--annotation", "a=b", "--annotation", "c=d", "img"},
	{"--kernel-memory", "1g", "img"},
	{"--cidfile", "/tmp/shards.cid", "img"},
	{"--memory", "1g", "--cpus", "2", "-p", "8080:80", "-v", "/a:/b", "-e", "X=1", "-l", "k=v", "--rm", "-t", "img", "sh", "-c", "echo hi"},
}

func TestShardsEngineBodies(t *testing.T) {
	out := os.Getenv("SHARDS_ENGINE_OUT")
	if out == "" {
		t.Skip("SHARDS_ENGINE_OUT names the file to write")
	}
	os.Clearenv()
	type record struct {
		Args  []string        `json:"args"`
		Body  json.RawMessage `json:"body,omitempty"`
		Error string          `json:"error,omitempty"`
	}
	var records []record
	for _, args := range engineCases {
		r := record{Args: args}
		flags := pflag.NewFlagSet("run", pflag.ContinueOnError)
		flags.SetOutput(io.Discard)
		flags.Usage = nil
		// As NewRunCommand: its flags stop at the image, whose arguments follow it.
		flags.SetInterspersed(false)
		copts := addFlags(flags)
		if err := flags.Parse(args); err != nil {
			r.Error = err.Error()
			records = append(records, r)
			continue
		}
		if rest := flags.Args(); len(rest) > 0 {
			copts.Image = rest[0]
			copts.Args = rest[1:]
		}
		cfg, err := parse(flags, copts, "linux")
		if err != nil {
			r.Error = err.Error()
			records = append(records, r)
			continue
		}
		body, err := json.Marshal(container.CreateRequest{
			Config:           cfg.Config,
			HostConfig:       cfg.HostConfig,
			NetworkingConfig: cfg.NetworkingConfig,
		})
		if err != nil {
			t.Fatal(err)
		}
		r.Body = body
		records = append(records, r)
	}
	data, err := json.MarshalIndent(records, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
