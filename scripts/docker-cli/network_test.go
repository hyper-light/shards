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
	"slices"
	"sort"
	"testing"

	"github.com/docker/cli/opts"
	"github.com/fvbommel/sortorder"
	"github.com/moby/moby/api/types/network"
)

type shardsAddr struct {
	In  string `json:"in"`
	Out string `json:"out,omitempty"`
	Err string `json:"err,omitempty"`
}

type shardsMac struct {
	In  string `json:"in"`
	Ok  bool   `json:"ok"`
	Out string `json:"out"`
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

// What `port CONTAINER PORT` makes of its PORT (network.ParsePort): the port it names, or
// the error.
type shardsPortArg struct {
	Arg  string `json:"arg"`
	Port string `json:"port,omitempty"`
	Err  string `json:"err,omitempty"`
}

// What `run -p` makes of its values: the ports exposed, and each one's bindings, sorted.
type shardsPorts struct {
	Publish  []string `json:"publish"`
	Err      string   `json:"err,omitempty"`
	Exposed  []string `json:"exposed"`
	Bindings []string `json:"bindings"`
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
		Ports       []shardsPorts      `json:"ports"`
		PortArgs    []shardsPortArg    `json:"port_args"`
		Natural     []string           `json:"natural"`
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
		"02:AB:cd:EF:00:9f", "02-AB-CD-EF-00-9F", "02AB.CDEF.009F", " 02:11:22:33:44:55",
	} {
		hw, err := net.ParseMAC(s)
		answers.Macs = append(answers.Macs, shardsMac{In: s, Ok: err == nil, Out: hw.String()})
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
	for _, publish := range [][]string{
		{"80"}, {"80/udp"}, {"80/SCTP"}, {"8080:80"}, {"127.0.0.1:8080:80"}, {"127.0.0.1::80"},
		{"[::1]:8080:80"}, {"[::1]::80/udp"}, {"8000-8002:80-82"}, {"8000-8010:80"}, {"80-82"},
		{"0.0.0.0:80:80", "[::]:80:80"}, {"80", "80"}, {"8080:80", "8081:80"},
		{"published=8080,target=80"}, {"published=8080,target=80,protocol=udp"}, {"target=80"},
		{"x"}, {"80/xyz"}, {"99999"}, {"1-x"}, {"x:80"}, {"8000-8002:80-81"}, {"a:b:c:d"},
		{"[::1:8080:80"}, {"[::1]x:8080:80"}, {"1.2.3:80:80"}, {"fe80::1%eth0:80:80"}, {""},
		{":80"}, {"8080:"}, {"/udp"}, {"82-80"}, {"8080-8000:80"}, {"-1"}, {"080"}, {"+80"},
		{"80:80:80:80"}, {"published=8080"}, {"published"}, {"=8080,target=80"}, {"0"}, {"0:0"},
		{"1.2.3.4:80-81:90-91"}, {"[1.2.3.4]:80:80"}, {"::1:8080:80"}, {"65535"}, {"65536"}, {"X:80"}, {"80/XYZ"},
		// Every value is put in standard notation before any is parsed (convertToStandardNotation,
		// then nat.ParsePortSpecs): the second's notation fails before the first's port.
		{"80:80/bogus", "published=1,x"}, {"x", "published=1,=2"}, {"published=1,x", "80/xyz"},
	} {
		args := []string{}
		for _, p := range publish {
			args = append(args, "-p", p)
		}
		args = append(args, "img")
		r := shardsPorts{Publish: publish}
		cfg, host, _, err := parseRun(args)
		if err != nil {
			r.Err = err.Error()
		} else {
			r.Exposed, r.Bindings = []string{}, []string{}
			for p := range cfg.ExposedPorts {
				r.Exposed = append(r.Exposed, p.String())
			}
			for p, bs := range host.PortBindings {
				for _, b := range bs {
					ip := ""
					if b.HostIP.IsValid() {
						ip = b.HostIP.String()
					}
					r.Bindings = append(r.Bindings, p.String()+" "+ip+" "+b.HostPort)
				}
			}
			sort.Strings(r.Exposed)
			sort.Strings(r.Bindings)
		}
		answers.Ports = append(answers.Ports, r)
	}
	for _, arg := range []string{
		"80", "80/tcp", "80/UDP", "80/xyz", "x", "", "/udp", "65535", "65536", "-1", "+80",
		"080", "80/", "80-81", "0", "80/tcp/x", " 80",
	} {
		r := shardsPortArg{Arg: arg}
		if p, err := network.ParsePort(arg); err != nil {
			r.Err = err.Error()
		} else {
			r.Port = p.String()
		}
		answers.PortArgs = append(answers.PortArgs, r)
	}
	// `docker port`'s lines, in the order it prints them.
	answers.Natural = []string{
		"80/tcp -> 0.0.0.0:18080", "9/tcp -> [::]:9", "9/tcp -> 0.0.0.0:9", "81/udp -> 127.0.0.1:81",
		"080/tcp -> x", "80/tcp -> [::]:18080", "443/tcp -> 0.0.0.0:443", "a10", "a9", "a09", "a009b",
		"a9b", "", "0", "00", "10.0.0.1:80", "10.0.0.10:80", "10.0.0.9:80", "[::1]:5", "[::]:5", "Z", "a",
	}
	slices.SortFunc(answers.Natural, sortorder.NaturalCompare)
	data, err := json.MarshalIndent(answers, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
