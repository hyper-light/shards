package container

// What the Docker CLI makes of `--network` values, for crates/cmdline/tests/network.rs:
// netip.ParseAddr and net.ParseMAC of addresses (Go 1.26.1), opts.NetworkOpt.Set of
// single values (its CSV, keys and parse errors), and `run`'s parse of whole command lines
// (parseNetworkOpts: the network mode, the endpoints asked for, the errors). generate
// copies it into cli/command/container, beside the parseRun it uses.

import (
	"encoding/json"
	"net"
	"net/netip"
	"os"
	"sort"
	"testing"

	"github.com/docker/cli/opts"
)

type shardsAddr struct {
	In  string `json:"in"`
	Out string `json:"out,omitempty"`
	Err string `json:"err,omitempty"`
}

type shardsMac struct {
	In string `json:"in"`
	Ok bool   `json:"ok"`
}

type shardsAttachment struct {
	In         string            `json:"in"`
	Err        string            `json:"err,omitempty"`
	Target     string            `json:"target"`
	Aliases    []string          `json:"aliases"`
	IPv4       string            `json:"ipv4"`
	IPv6       string            `json:"ipv6"`
	LinkLocal  []string          `json:"link_local"`
	Mac        string            `json:"mac"`
	DriverOpts map[string]string `json:"driver_opts"`
	GwPriority int               `json:"gw_priority"`
}

type shardsRun struct {
	Networks  []string `json:"networks"`
	Err       string   `json:"err,omitempty"`
	Mode      string   `json:"mode"`
	Endpoints []string `json:"endpoints"`
}

func shardsAddrString(a netip.Addr) string {
	if !a.IsValid() {
		return ""
	}
	return a.String()
}

func TestShardsNetwork(t *testing.T) {
	out := os.Getenv("SHARDS_NETWORK_OUT")
	if out == "" {
		t.Skip("SHARDS_NETWORK_OUT names the file to write")
	}
	var answers struct {
		Addrs       []shardsAddr       `json:"addrs"`
		Macs        []shardsMac        `json:"macs"`
		Attachments []shardsAttachment `json:"attachments"`
		Runs        []shardsRun        `json:"runs"`
	}
	for _, s := range []string{
		"172.17.0.9", "::ffff:172.17.0.9", "fe80::1%eth0", "::", "::1", "1:2:3:4:5:6:1.2.3.4",
		"::1.2.3.4", "0:0:1:0:0:0:0:1", "1:0:0:2:0:0:0:3", "1:0:0:2:0:0:3:4", "2001:DB8::1",
		"1:2:3:4:5:6:7::", "::ffff:0.0.0.0", "0.0.0.0", "255.255.255.255",
		"foo", "", "1.2.3", "1.2.3.4.5", "01.2.3.4", "256.2.3.4", "1..3.4", ".1.2.3", "1.2.3.",
		"1.2.3.x", "%eth0", "fe80::1%", "1::2::3", "12345::", "1:2", "1:", ":1", "1:2:3:4:5:6:7:8:9",
		"1:2:3:4:5:6:7::8", "1:2:3:4:1.2.3.4", "1:2:3:4:5:6:7:1.2.3.4", "::ffff:1.2.3",
		"::ffff:1.2.3.4.5", "1:2:3:4:5:6:7:8:1.2.3.4", "1:2:3:4:5:6::1.2.3.4", "::1%a%b",
		"1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8::", "g::", "1:g::", "1.2.3.4%eth0", "é", "1.é",
	} {
		a, err := netip.ParseAddr(s)
		if err != nil {
			answers.Addrs = append(answers.Addrs, shardsAddr{In: s, Err: err.Error()})
		} else {
			answers.Addrs = append(answers.Addrs, shardsAddr{In: s, Out: a.String()})
		}
	}
	for _, s := range []string{
		"02:11:22:33:44:55", "02-11-22-33-44-55", "0211.2233.4455", "021122334455",
		"02:11:22:33:44:55:66:77", "0211.2233.4455.6677", "0211223344556677",
		"00:00:00:00:fe:80:00:00:00:00:00:00:02:00:5e:10:00:00:00:01",
		"zz", "", "02:11:22:33:44", "02:11:22:33:44:5g", "02:11-22:33:44:55", "0211223344",
		"02112233445g", "0211.2233.44:5", "02:11:22:33:44:55:", "02:11:22:33:44:555",
		"02:11:22:33:44:55:66", "0211.2233.445", "0211:2233:4455", "02112233445566",
	} {
		_, err := net.ParseMAC(s)
		answers.Macs = append(answers.Macs, shardsMac{In: s, Ok: err == nil})
	}
	for _, s := range []string{
		"bridge", "none", "name=", "", "my net", "a=", "=b",
		"name=Bridge", "NAME=bridge", "name=bridge,alias=x,alias=y", " name = bridge ",
		"name=bridge,ip=172.17.0.9,ip6=fd00::5,link-local-ip=169.254.1.1,link-local-ip=fe80::2",
		"name=bridge,mac-address=02:11:22:33:44:55", "name=bridge,mac-address=ZZ",
		"name=bridge,driver-opt=a=1,driver-opt=b=2,driver-opt=a=3", "name=bridge,driver-opt=A = B ",
		"name=bridge,gw-priority=7", "name=bridge,gw-priority=-7", "name=bridge,gw-priority=+7",
		"name=bridge,gw-priority=x", "name=bridge,gw-priority=", "name=bridge,gw-priority=1_0",
		"name=bridge,gw-priority=99999999999999999999", "name=bridge,gw-priority=0x10",
		"a=b", "name=x,nope", "name=x,=y", "alias=x", "name=x,ip=y", "name=x,ip=", "name=x,ip6=1.2.3",
		"name=x,link-local-ip=q", "name=x,driver-opt=y", "name=x,driver-opt==y", "name=İX", "name=xİ", "name=xΣΑΣ", "name=x,alias=İ",
		"name=a\"b", "\"name=a\"x", "\"name=a", "\"name=a\nb", "\"name=a,b\"", "\n\nname=x\nname=y",
		"\"name=a\"\"b\",alias=c", "alias=x,\"name=y\nz\"\"\",alias=w", "alias=a,\"name=b\r\nc\"",
		"name=a\r", "\"name=a\nb\nc", "name=a\r\n", "name=a,", ",name=a", "name=a,,alias=b",
		"\"name=a\" ", "name=a\"", "x\nname=y", "name=ΣΑΣ",
	} {
		var n opts.NetworkOpt
		a := shardsAttachment{In: s}
		if err := n.Set(s); err != nil {
			a.Err = err.Error()
		} else if v := n.Value(); len(v) == 1 {
			o := v[0]
			a.Target, a.Aliases, a.Mac, a.DriverOpts, a.GwPriority = o.Target, o.Aliases, o.MacAddress, o.DriverOpts, o.GwPriority
			a.IPv4, a.IPv6 = shardsAddrString(o.IPv4Address), shardsAddrString(o.IPv6Address)
			for _, l := range o.LinkLocalIPs {
				a.LinkLocal = append(a.LinkLocal, l.String())
			}
		}
		answers.Attachments = append(answers.Attachments, a)
	}
	for _, networks := range [][]string{
		{}, {"none"}, {"bridge"}, {"default"}, {"host"}, {"container:x"}, {"mine"}, {""}, {" "},
		{"bridge", "none"}, {"none", "bridge"}, {"none", "none"}, {"bridge", "mine"},
		{"mine", "bridge"}, {"mine", "theirs"}, {"name=bridge,gw-priority=0"},
		{"name=bridge,gw-priority=1"}, {"name=none,alias=x"}, {"name=mine,alias=x"},
		{"name=bridge,mac-address=zz"}, {"name=bridge,mac-address= 02:11:22:33:44:55 "},
		{"name=bridge,ip=172.17.0.9"}, {"bridge", "default"}, {"default", "default"},
		{"name=bridge,driver-opt=a=b"}, {"bridge", "name=bridge,alias=x"}, {"container:x", "none"},
		{"a=b"}, {"name=x,ip=y"},
	} {
		args := []string{}
		for _, n := range networks {
			args = append(args, "--network", n)
		}
		args = append(args, "img")
		r := shardsRun{Networks: networks}
		_, host, nw, err := parseRun(args)
		if err != nil {
			r.Err = err.Error()
		} else {
			r.Mode = string(host.NetworkMode)
			r.Endpoints = []string{}
			for k := range nw.EndpointsConfig {
				r.Endpoints = append(r.Endpoints, k)
			}
			sort.Strings(r.Endpoints)
		}
		answers.Runs = append(answers.Runs, r)
	}
	data, err := json.MarshalIndent(answers, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
