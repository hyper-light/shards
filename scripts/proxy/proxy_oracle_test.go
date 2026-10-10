//go:build linux

// Copied by scripts/proxy/generate into BuildKit v0.33.0's proxyprovider package: what its
// exec proxy (util/network/proxyprovider/provider_linux.go) makes of each case, for
// crates/shards/src/build/proxy's tests to expect of shards' proxy:
//   - urls: captureURL and redactURL of each URL;
//   - finals: finalURL of a request's URL and a response's Location;
//   - plain: each raw request sent to the proxy's own handler on Go's server, as BuildKit
//     serves it (ReadHeaderTimeout 30 s), under a policy refusing every request: the URLs
//     the policy was asked, and what the client read;
//   - tunnels: each CONNECT, then each raw request inside the tunnel's TLS (the client
//     trusting the proxy's CA): what the client read of each, and the URLs asked.
//
// A response's Date field is said as DATE.
package proxyprovider

import (
	"bufio"
	"bytes"
	"container/list"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/json"
	stderrors "errors"
	"io"
	"math/big"
	"net"
	"net/http"
	neturl "net/url"
	"os"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/moby/buildkit/solver/pb"
	"github.com/moby/buildkit/util/network"
)

type refusing struct {
	mu   sync.Mutex
	seen []string
}

func (p *refusing) Evaluate(_ context.Context, op *pb.Op) (bool, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.seen = append(p.seen, op.GetSource().GetIdentifier())
	return false, stderrors.New("refused")
}

func (p *refusing) take() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	out := p.seen
	p.seen = nil
	if out == nil {
		out = []string{}
	}
	return out
}

func bytesOf(b []byte) any {
	if utf8.Valid(b) {
		return string(b)
	}
	return map[string]string{"base64": base64.StdEncoding.EncodeToString(b)}
}

// dated says each Date field of an HTTP date as DATE.
func dated(b []byte) []byte {
	lines := bytes.Split(b, []byte("\r\n"))
	for i, l := range lines {
		if bytes.HasPrefix(l, []byte("Date: ")) && len(l) == len("Date: ")+len(http.TimeFormat) {
			if _, err := time.Parse(http.TimeFormat, string(l[len("Date: "):])); err == nil {
				lines[i] = []byte("Date: DATE")
			}
		}
	}
	return bytes.Join(lines, []byte("\r\n"))
}

// exchange writes raw on a new connection, then ends its writing, and reads what comes
// until the server closes.
func exchange(t *testing.T, addr string, raw []byte) []byte {
	c, err := net.Dial("tcp4", addr)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(20 * time.Second))
	go func() {
		_, _ = c.Write(raw)
		_ = c.(*net.TCPConn).CloseWrite()
	}()
	got, _ := io.ReadAll(c)
	return got
}

type readConn struct {
	net.Conn
	r io.Reader
}

func (c readConn) Read(p []byte) (int, error) { return c.r.Read(p) }

// tunnel sends connect, reads the proxy's answer to it, and where it established the
// tunnel, writes raw in TLS to serverName, ends its writing (close_notify), and reads.
func tunnel(t *testing.T, addr string, roots *x509.CertPool, connect, serverName string, raw []byte) (established []byte, response []byte, handshake bool) {
	c, err := net.Dial("tcp4", addr)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(20 * time.Second))
	if _, err := io.WriteString(c, connect); err != nil {
		t.Fatal(err)
	}
	br := bufio.NewReader(c)
	for !bytes.HasSuffix(established, []byte("\r\n\r\n")) {
		b, err := br.ReadByte()
		if err != nil {
			return established, nil, false
		}
		established = append(established, b)
	}
	if !bytes.HasPrefix(established, []byte("HTTP/1.1 200 ")) {
		rest, _ := io.ReadAll(br)
		return append(established, rest...), nil, false
	}
	tc := tls.Client(readConn{c, br}, &tls.Config{RootCAs: roots, ServerName: serverName, NextProtos: []string{"http/1.1"}})
	if err := tc.Handshake(); err != nil {
		return established, nil, false
	}
	if _, err := tc.Write(raw); err != nil {
		return established, nil, true
	}
	// Its close_notify only once the proxy has gone quiet: one the proxy left unread as it
	// closed would reset the connection, and with it what the client had yet to read.
	buf := make([]byte, 32<<10)
	ended := false
	for {
		wait := 20 * time.Second
		if !ended {
			wait = 2 * time.Second
		}
		_ = c.SetReadDeadline(time.Now().Add(wait))
		n, err := tc.Read(buf)
		response = append(response, buf[:n]...)
		if err == nil {
			continue
		}
		var ne net.Error
		if !ended && stderrors.As(err, &ne) && ne.Timeout() {
			ended = true
			_ = tc.CloseWrite()
			continue
		}
		return established, response, true
	}
}

func TestShardsProxy(t *testing.T) {
	type urlOut struct {
		In      string `json:"in"`
		Capture string `json:"capture"`
		Redact  string `json:"redact"`
	}
	var urls []urlOut
	for _, u := range []string{
		"",
		"http://example.com/a",
		"http://example.com:80/a",
		"https://example.com:443/a",
		"http://example.com:443/a",
		"https://example.com:80/a",
		"http://example.com:8080/a",
		"http://[::1]:80/a",
		"https://[2001:db8::1]:443/a?b",
		"http://[fe80::1%25en0]:80/",
		"http://user:pass@example.com/a",
		"http://user@example.com:80/a",
		"http://:pass@example.com/a",
		"http://@example.com/a",
		"http://user:@example.com/a",
		"http://us%20er:pa%2Fss@example.com/a",
		"HTTP://Example.COM:80/A",
		"http://example.com/a%2Fb/%7e?x=%20#frag",
		"http://example.com/a b",
		"http://example.com/%zz",
		"http://example.com:abc/",
		"http://example.com?q",
		"http://example.com",
		"http:///path",
		"http:",
		"mailto:user@example.com",
		"/relative/path",
		"example.com:80/a",
		"not a url\x7f",
		"https://xxxxx:xxxxx@example.com/",
	} {
		urls = append(urls, urlOut{In: u, Capture: captureURL(u), Redact: redactURL(u)})
	}

	type finalOut struct {
		Request  string `json:"request"`
		Location string `json:"location"`
		Final    string `json:"final"`
	}
	var finals []finalOut
	for _, f := range [][2]string{
		{"http://example.com/a/b?c", ""},
		{"http://example.com/a/b?c", "/x"},
		{"http://example.com/a/b?c", "x/y"},
		{"http://example.com/a/b?c", "../x"},
		{"http://example.com/a/b?c", "?q"},
		{"http://example.com/a/b?c", "#f"},
		{"http://example.com/a/b?c", "//other.example/p"},
		{"http://example.com/a/b?c", "https://other.example:443/p"},
		{"https://example.com:443/a", "http://u:p@other.example/"},
		{"http://example.com/a/b", "%zz"},
		{"http://example.com/a/b", "http://[::1"},
		{"http://example.com/a/b", "./"},
		{"http://example.com/a/b", "../../../../x"},
		{"http://example.com/a/b", "x y"},
		{"http://example.com/", "mailto:x@example.com"},
	} {
		u, err := neturl.Parse(f[0])
		if err != nil {
			t.Fatal(err)
		}
		resp := &http.Response{Header: http.Header{}}
		if f[1] != "" {
			resp.Header.Set("Location", f[1])
		}
		finals = append(finals, finalOut{Request: f[0], Location: f[1], Final: finalURL(&http.Request{URL: u}, resp)})
	}

	caPEM, ca, key, err := newCA()
	if err != nil {
		t.Fatal(err)
	}
	p := &provider{caPEM: caPEM, ca: ca, caKey: key, certs: map[string]*certCacheEntry{}, lru: list.New(), transport: newProxyTransport()}
	policy := &refusing{}
	h := &proxyHandler{provider: p, policy: policy, capture: network.NewProxyCapture(), transport: p.transport}
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	srv := &http.Server{Handler: h, ReadHeaderTimeout: 30 * time.Second}
	go func() { _ = srv.Serve(ln) }()
	defer srv.Close()
	addr := ln.Addr().String()
	roots := x509.NewCertPool()
	roots.AddCert(ca)

	const host = "Host: example.com\r\n"
	type plainOut struct {
		Name     string   `json:"name"`
		Request  any      `json:"request"`
		Checked  []string `json:"checked"`
		Response any      `json:"response"`
	}
	var plain []plainOut
	for _, c := range [][2]string{
		{"absolute form", "GET http://example.com/a?b=c HTTP/1.1\r\n" + host + "\r\n"},
		{"origin form", "GET /a?b=c HTTP/1.1\r\nHost: example.com:8080\r\n\r\n"},
		{"absolute form over another Host", "GET http://a.example/x HTTP/1.1\r\nHost: b.example\r\n\r\n"},
		{"user and password", "GET http://user:pass@example.com/p HTTP/1.1\r\n" + host + "\r\n"},
		{"user alone", "GET http://user@example.com/p HTTP/1.1\r\n" + host + "\r\n"},
		{"password alone", "GET http://:pass@example.com/p HTTP/1.1\r\n" + host + "\r\n"},
		{"escapes kept", "GET http://example.com/a%2Fb/%7e/c%20d?x=%20&y=%zz HTTP/1.1\r\n" + host + "\r\n"},
		{"a fragment in the target", "GET http://example.com/a#frag HTTP/1.1\r\n" + host + "\r\n"},
		{"default port", "GET http://example.com:80/ HTTP/1.1\r\n" + host + "\r\n"},
		{"https over plain", "GET https://example.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"scheme and host cased", "GET HTTP://Example.COM/A HTTP/1.1\r\nHost: Example.COM\r\n\r\n"},
		{"IPv6 literal", "GET http://[::1]:8080/ HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n"},
		{"IPv6 zone", "GET http://[fe80::1%25en0]:8080/ HTTP/1.1\r\nHost: [fe80::1%25en0]:8080\r\n\r\n"},
		{"percent in host", "GET http://ex%41mple.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"port not numeric", "GET http://example.com:abc/ HTTP/1.1\r\n" + host + "\r\n"},
		{"asterisk", "OPTIONS * HTTP/1.1\r\n" + host + "\r\n"},
		{"asterisk, HTTP/1.0", "OPTIONS * HTTP/1.0\r\n\r\n"},
		{"asterisk, HTTP/1.0 keep-alive", "OPTIONS * HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"},
		{"asterisk with a body", "OPTIONS * HTTP/1.1\r\n" + host + "Content-Length: 5\r\n\r\nhello"},
		{"asterisk with a short body", "OPTIONS * HTTP/1.1\r\n" + host + "Content-Length: 50\r\n\r\nhello"},
		{"asterisk with a bad chunk", "OPTIONS * HTTP/1.1\r\n" + host + "Transfer-Encoding: chunked\r\n\r\nzz\r\n"},
		{"asterisk with 100-continue", "OPTIONS * HTTP/1.1\r\n" + host + "Expect: 100-continue\r\nContent-Length: 5\r\n\r\nhello"},
		{"asterisk, then another", "OPTIONS * HTTP/1.1\r\n" + host + "\r\nGET http://example.com/after HTTP/1.1\r\n" + host + "\r\n"},
		{"asterisk asking to close", "OPTIONS * HTTP/1.1\r\n" + host + "Connection: close\r\n\r\n"},
		{"asterisk of another method", "GET * HTTP/1.1\r\n" + host + "\r\n"},
		{"empty host, absolute", "GET http:///path HTTP/1.1\r\nHost: h\r\n\r\n"},
		{"scheme alone", "GET http: HTTP/1.1\r\nHost: h\r\n\r\n"},
		{"opaque", "GET mailto:x@example.com HTTP/1.1\r\nHost: h\r\n\r\n"},
		{"absolute without a path", "GET http://example.com HTTP/1.1\r\n" + host + "\r\n"},
		{"query alone", "GET http://example.com?q HTTP/1.1\r\n" + host + "\r\n"},
		{"double slash", "GET //example.com/a HTTP/1.1\r\nHost: h\r\n\r\n"},
		{"Host of a port alone", "GET /a HTTP/1.1\r\nHost: :80\r\n\r\n"},
		{"Host with a user", "GET /a HTTP/1.1\r\nHost: user@example.com\r\n\r\n"},
		{"HTTP/1.0 without Host", "GET /a HTTP/1.0\r\n\r\n"},
		{"HTTP/1.0 keep-alive", "GET http://example.com/ HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"},
		{"HTTP/1.0 pipelined", "GET http://example.com/1 HTTP/1.0\r\n\r\nGET http://example.com/2 HTTP/1.0\r\n\r\n"},
		{"HTTP/1.0 kept alive, pipelined", "GET http://example.com/1 HTTP/1.0\r\nConnection: keep-alive\r\n\r\nGET http://example.com/2 HTTP/1.0\r\n\r\nGET http://example.com/3 HTTP/1.0\r\n\r\n"},
		{"HTTP/1.1 without Host", "GET /a HTTP/1.1\r\n\r\n"},
		{"two Hosts", "GET /a HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"},
		{"malformed Host", "GET /a HTTP/1.1\r\nHost: a b\r\n\r\n"},
		{"invalid header name", "GET http://example.com/ HTTP/1.1\r\n" + host + "Bad Name: x\r\n\r\n"},
		{"field without a colon", "GET http://example.com/ HTTP/1.1\r\n" + host + "NoColon\r\n\r\n"},
		{"space before the colon", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-A : x\r\n\r\n"},
		{"bad version", "GET http://example.com/ HTTP/1.x\r\n" + host + "\r\n"},
		{"HTTP/2.0", "GET / HTTP/2.0\r\nHost: h\r\n\r\n"},
		{"HTTP/1.2", "GET http://example.com/ HTTP/1.2\r\n" + host + "\r\n"},
		{"space in the target", "GET http://example.com/a b HTTP/1.1\r\n" + host + "\r\n"},
		{"invalid escape in the path", "GET http://example.com/%zz HTTP/1.1\r\n" + host + "\r\n"},
		{"lowercase method", "get http://example.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"bad method", "G(ET http://example.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"HEAD", "HEAD http://example.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"a body", "POST http://example.com/form HTTP/1.1\r\n" + host + "Content-Length: 5\r\n\r\nhello"},
		{"a chunked body", "POST http://example.com/form HTTP/1.1\r\n" + host + "Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"},
		{"a short body", "POST http://example.com/form HTTP/1.1\r\n" + host + "Content-Length: 50\r\n\r\nhello"},
		{"a bad length", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: x\r\n\r\n"},
		{"two lengths", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 1\r\nContent-Length: 2\r\n\r\nab"},
		{"the same length twice", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 2\r\nContent-Length: 2\r\n\r\nab"},
		{"gzip transfer coding", "POST http://example.com/ HTTP/1.1\r\n" + host + "Transfer-Encoding: gzip\r\n\r\n"},
		{"a length and chunked", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"},
		{"100-continue", "POST http://example.com/ HTTP/1.1\r\n" + host + "Expect: 100-continue\r\nContent-Length: 5\r\n\r\nhello"},
		{"another expectation", "GET http://example.com/ HTTP/1.1\r\n" + host + "Expect: something\r\n\r\n"},
		{"pipelined", "GET http://example.com/1 HTTP/1.1\r\n" + host + "\r\nGET http://example.com/2 HTTP/1.1\r\n" + host + "\r\n"},
		{"close asked", "GET http://example.com/ HTTP/1.1\r\n" + host + "Connection: close\r\n\r\nGET http://example.com/never HTTP/1.1\r\n" + host + "\r\n"},
		{"a continuation line", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-A: a\r\n b\r\n\r\n"},
		{"blank lines first", "\r\n\r\nGET http://example.com/ HTTP/1.1\r\n" + host + "\r\n"},
		{"bare LF", "GET http://example.com/ HTTP/1.1\nHost: example.com\n\n"},
		{"a control in a value", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-A: a\x01b\r\n\r\n"},
		{"a value not UTF-8", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-A: \xff\r\n\r\n"},
	} {
		got := exchange(t, addr, []byte(c[1]))
		plain = append(plain, plainOut{Name: c[0], Request: bytesOf([]byte(c[1])), Checked: policy.take(), Response: bytesOf(dated(got))})
	}
	// Requests too large to write out: their text, a byte many times, then more text.
	for _, c := range []struct {
		name, prefix string
		fill         byte
		times        int
		suffix       string
	}{
		{"a large body", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 300000\r\n\r\n", 'b', 300000, ""},
		{"a body past what is discarded", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 262145\r\n\r\n", 'b', 262145, "GET http://example.com/next HTTP/1.1\r\n" + host + "\r\n"},
		{"a body just discarded", "POST http://example.com/ HTTP/1.1\r\n" + host + "Content-Length: 262144\r\n\r\n", 'b', 262144, "GET http://example.com/next HTTP/1.1\r\n" + host + "\r\n"},
		{"asterisk with a large body", "OPTIONS * HTTP/1.1\r\n" + host + "Content-Length: 5000\r\n\r\n", 'b', 5000, ""},
		{"asterisk with a body at its bound", "OPTIONS * HTTP/1.1\r\n" + host + "Content-Length: 4096\r\n\r\n", 'b', 4096, ""},
		{"asterisk with a body one past its bound", "OPTIONS * HTTP/1.1\r\n" + host + "Content-Length: 4097\r\n\r\n", 'b', 4097, ""},
		{"a large head", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-Big: ", 'a', (1 << 20) + 8192, "\r\n\r\n"},
		// The head's 64 bytes besides: at Go's bound (MaxHeaderBytes and 4096), and one past.
		{"a head at the bound", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-Big: ", 'a', (1 << 20) + 4096 - 64, "\r\n\r\n"},
		{"a head one past the bound", "GET http://example.com/ HTTP/1.1\r\n" + host + "X-Big: ", 'a', (1 << 20) + 4096 - 63, "\r\n\r\n"},
	} {
		raw := c.prefix + strings.Repeat(string(c.fill), c.times) + c.suffix
		got := exchange(t, addr, []byte(raw))
		plain = append(plain, plainOut{
			Name:     c.name,
			Request:  map[string]any{"prefix": c.prefix, "fill": string(c.fill), "times": c.times, "suffix": c.suffix},
			Checked:  policy.take(),
			Response: bytesOf(dated(got)),
		})
	}

	type tunnelOut struct {
		Name        string   `json:"name"`
		Connect     string   `json:"connect"`
		ServerName  string   `json:"server_name"`
		Request     any      `json:"request"`
		Established string   `json:"established"`
		Handshake   bool     `json:"handshake"`
		Checked     []string `json:"checked"`
		Response    any      `json:"response"`
	}
	var tunnels []tunnelOut
	connect := func(authority string) string {
		return "CONNECT " + authority + " HTTP/1.1\r\nHost: " + authority + "\r\n\r\n"
	}
	for _, c := range [][4]string{
		{"origin form", connect("example.com:443"), "example.com", "GET /a?b HTTP/1.1\r\n" + host + "\r\n"},
		{"another port", connect("example.com:8443"), "example.com", "GET / HTTP/1.1\r\nHost: other.example\r\n\r\n"},
		{"absolute form inside", connect("example.com:443"), "example.com", "GET http://evil.example/x HTTP/1.1\r\nHost: evil.example\r\n\r\n"},
		{"a user inside", connect("example.com:443"), "example.com", "GET http://u:p@evil.example/x HTTP/1.1\r\nHost: evil.example\r\n\r\n"},
		{"no port", connect("example.com"), "example.com", "GET / HTTP/1.1\r\n" + host + "\r\n"},
		{"IPv4", connect("192.0.2.1:443"), "192.0.2.1", "GET / HTTP/1.1\r\nHost: 192.0.2.1\r\n\r\n"},
		{"IPv6", connect("[2001:db8::1]:443"), "2001:db8::1", "GET / HTTP/1.1\r\nHost: [2001:db8::1]\r\n\r\n"},
		{"a Host other than the authority", "CONNECT a.example:443 HTTP/1.1\r\nHost: b.example:443\r\n\r\n", "a.example", "GET / HTTP/1.1\r\nHost: a.example\r\n\r\n"},
		{"no Host", "CONNECT a.example:443 HTTP/1.1\r\n\r\n", "a.example", "GET / HTTP/1.1\r\nHost: a.example\r\n\r\n"},
		{"a path after the authority", "CONNECT a.example:443/x HTTP/1.1\r\nHost: a.example:443\r\n\r\n", "a.example", "GET / HTTP/1.1\r\nHost: a.example\r\n\r\n"},
		{"origin-form CONNECT", "CONNECT /x HTTP/1.1\r\nHost: a.example:443\r\n\r\n", "a.example", "GET / HTTP/1.1\r\nHost: a.example\r\n\r\n"},
		{"pipelined inside", connect("example.com:443"), "example.com", "GET /1 HTTP/1.1\r\n" + host + "\r\nGET /2 HTTP/1.1\r\n" + host + "\r\n"},
		{"HTTP/1.0 inside", connect("example.com:443"), "example.com", "GET /a HTTP/1.0\r\n\r\n"},
		{"malformed inside", connect("example.com:443"), "example.com", "GARBAGE\r\n\r\n"},
		{"a body inside", connect("example.com:443"), "example.com", "POST /f HTTP/1.1\r\n" + host + "Content-Length: 3\r\n\r\nabc"},
		{"escapes inside", connect("example.com:443"), "example.com", "GET /a%2Fb?c=%zz HTTP/1.1\r\n" + host + "\r\n"},
		{"asterisk inside", connect("example.com:443"), "example.com", "OPTIONS * HTTP/1.1\r\n" + host + "\r\n"},
		{"invalid escape inside", connect("example.com:443"), "example.com", "GET /%zz HTTP/1.1\r\n" + host + "\r\n"},
		{"a bad CONNECT", "CONNECT a b HTTP/1.1\r\nHost: a\r\n\r\n", "a", "GET / HTTP/1.1\r\n\r\n"},
	} {
		established, response, handshake := tunnel(t, addr, roots, c[1], c[2], []byte(c[3]))
		tunnels = append(tunnels, tunnelOut{
			Name: c[0], Connect: c[1], ServerName: c[2], Request: bytesOf([]byte(c[3])),
			Established: string(dated(established)), Handshake: handshake, Checked: policy.take(), Response: bytesOf(dated(response)),
		})
	}

	passed, passedTunnels := passedOn(t)

	type upstreamProxyOut struct {
		Value string `json:"value"`
		Valid bool   `json:"valid"`
	}
	var upstreamProxies []upstreamProxyOut
	for _, v := range []string{
		"http://proxy:3128", "proxy:3128", "proxy", "https://user:secret@proxy", "socks5://proxy:1080",
		"socks5h://proxy", "http://[::1]:3128", "[::1]:3128", "ftp://proxy", "socks4://proxy", "socks5://",
		"http://", "http://:3128", "://x", "http://user:secret@", ":3128", "proxy:abc", "http://proxy:abc",
		"HTTP://Proxy", "http:proxy", "//proxy", "http:///p", "user@proxy:1", "a b", "http://a b",
		"http://%zz", "%zz", "http://[::1", "1.2.3.4", "http://proxy/path?q#f", " http://proxy",
	} {
		_, err := parseProxyEnvironmentValue(v)
		upstreamProxies = append(upstreamProxies, upstreamProxyOut{Value: v, Valid: err == nil})
	}

	type sniffOut struct {
		Data any    `json:"data"`
		Type string `json:"type"`
	}
	var sniffed []sniffOut
	for _, s := range sniffCases {
		sniffed = append(sniffed, sniffOut{Data: bytesOf([]byte(s)), Type: http.DetectContentType([]byte(s))})
	}

	dt, err := json.MarshalIndent(map[string]any{
		"urls": urls, "finals": finals, "plain": plain, "tunnels": tunnels,
		"upstream": upstreamResponses, "passed": passed, "passed_tunnels": passedTunnels,
		"sniff": sniffed, "upstream_proxies": upstreamProxies,
	}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_PROXY_OUT"), append(dt, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// upstreamResponses: what the tests' upstream answers, raw, by the path asked, after
// reading the request and its body; one ending in "#close" (sent with the rest) closes the
// connection after it, as does a request asking to close.
var upstreamResponses = map[string]string{
	"/hello":           "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Type: text/plain\r\nX-B: 2\r\nX-A: 1\r\n\r\nhello\n",
	"/denied":          "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nnever\n",
	"/chunked":         "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
	"/chunked-long":    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\nbb8\r\n" + strings.Repeat("c", 3000) + "\r\n0\r\n\r\n",
	"/unframed":        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nabc#close",
	"/unframed-long":   "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n" + strings.Repeat("u", 5000) + "#close",
	"/redirect":        "HTTP/1.1 302 Found\r\nLocation: /hello\r\nContent-Length: 0\r\n\r\n",
	"/away":            "HTTP/1.1 301 Moved Permanently\r\nLocation: http://user:pw@other.example:80/x\r\nContent-Length: 0\r\n\r\n",
	"/missing":         "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 4\r\n\r\ngone",
	"/empty":           "HTTP/1.1 204 No Content\r\nX-E: 1\r\n\r\n",
	"/unchanged":       "HTTP/1.1 304 Not Modified\r\nEtag: \"v1\"\r\n\r\n",
	"/dated":           "HTTP/1.1 200 OK\r\nDate: Mon, 02 Jan 2006 15:04:05 GMT\r\nContent-Type: text/plain\r\nContent-Length: 1\r\n\r\nd",
	"/hops":            "HTTP/1.1 200 OK\r\nConnection: keep-alive, X-Hop\r\nKeep-Alive: timeout=5\r\nX-Hop: 1\r\nProxy-Authenticate: Basic\r\nUpgrade: h2c\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
	"/cookies":         "HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nx-lower: l\r\nContent-Length: 0\r\n\r\n",
	"/part":            "HTTP/1.1 206 Partial Content\r\nContent-Type: text/plain\r\nContent-Range: bytes 0-1/6\r\nContent-Length: 2\r\n\r\nhe",
	"/odd-status":      "HTTP/1.1 299 Whatever\r\nContent-Length: 0\r\n\r\n",
	"/reason":          "HTTP/1.1 200 Fine Thanks\r\nContent-Length: 0\r\n\r\n",
	"/continue-first":  "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
	"/cut":             "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\n\r\nabc#close",
	"/head":            "HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nContent-Type: text/plain\r\n\r\n",
	"/http10":          "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
	"/http10-unframed": "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nold#close",
	"/server-error":    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nbad",
	"/sniffed":         "HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n<html>hi</html>",
	"/sniffed-text":    "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc",
	"/not-sniffed":     "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 5\r\n\r\nzzzzz",
	"/empty-200":       "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
	"/unframed-505":    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n" + strings.Repeat("v", 505) + "#close",
	"/unframed-506":    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n" + strings.Repeat("v", 506) + "#close",
	"/chunked-1000":    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n3e8\r\n" + strings.Repeat("k", 1000) + "\r\n0\r\n\r\n",
	"/sniffed-binary":  "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n\x01\x02ab",
	"/typed-empty":     "HTTP/1.1 200 OK\r\nContent-Type: \r\nContent-Length: 3\r\n\r\nabc",
	"/sniffed-late":    "HTTP/1.1 200 OK\r\nContent-Length: 600\r\n\r\n" + strings.Repeat(" ", 520) + strings.Repeat("x", 80),
	"/encoded-empty":   "HTTP/1.1 200 OK\r\nContent-Encoding: \r\nContent-Length: 3\r\n\r\nabc",
}

// sniffCases: bodies of every kind Go's DetectContentType tells apart, and their edges.
var sniffCases = []string{
	"", "abc", "\x01", "a\x0bb", "a\x1bb", "a\x0cb", " \t\r\n\x0c", "\x7f\x80\xff",
	"<!DOCTYPE HTML>", "<!doctype html ", "<!DOCTYPE HTMLx", "  <html>", "<HTML", "<html>", "<head>", "<script>",
	"<iframe ", "<h1>", "<div>", "<font>", "<table>", "<a>", "<a href=x>", "<abbr>", "<style>", "<title>",
	"<b>", "<body>", "<br>", "<p>", "<!-->", "<!-- x", "\n<?xml version", "<?XML", "%PDF-1.7", "%!PS-Adobe-3.0",
	"\xfe\xff\x00\x00", "\xfe\xff", "\xff\xfe\x00\x00", "\xef\xbb\xbf\x00", "\xef\xbb\xbfabc",
	"\x00\x00\x01\x00", "\x00\x00\x02\x00", "BM", "GIF87a", "GIF89a", "GIF88a",
	"RIFF\x01\x02\x03\x04WEBPVP8 ", "RIFF\x01\x02\x03\x04WEBPXX", "\x89PNG\x0d\x0a\x1a\x0a", "\xff\xd8\xff\xe0",
	"FORM\x00\x00\x00\x00AIFF", "ID3\x04", "OggS\x00\x02", "OggS\x01", "MThd\x00\x00\x00\x06", "RIFF\x00\x00\x00\x00AVI ",
	"RIFF\x00\x00\x00\x00WAVE", "\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp41isom", "\x00\x00\x00\x14ftypisom\x00\x00\x00\x00mp41",
	"\x00\x00\x00\x10ftypmp42\x00\x00\x00\x00", "\x00\x00\x00\x0cftypmp42", "\x00\x00\x00\x13ftypmp42\x00\x00\x00\x00mp41",
	"\x00\x00\x00\xffftypmp42", "\x1a\x45\xdf\xa3", strings.Repeat("\x00", 34) + "LP", strings.Repeat("\x01", 34) + "LP",
	"\x00\x01\x00\x00", "OTTO", "ttcf", "wOFF", "wOF2", "\x1f\x8b\x08", "PK\x03\x04", "Rar!\x1a\x07\x00", "Rar!\x1a\x07\x01\x00",
	"\x00asm", strings.Repeat("a", 600) + "\x01", strings.Repeat("a", 511) + "\x01", strings.Repeat(" ", 600),
}

// upstream serves upstreamResponses on the loopback, over TLS where config is given.
func upstream(t *testing.T, config *tls.Config) string {
	var ln net.Listener
	var err error
	if config != nil {
		ln, err = tls.Listen("tcp4", "127.0.0.1:0", config)
	} else {
		ln, err = net.Listen("tcp4", "127.0.0.1:0")
	}
	if err != nil {
		t.Fatal(err)
	}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func() {
				defer c.Close()
				br := bufio.NewReader(c)
				for {
					req, err := http.ReadRequest(br)
					if err != nil {
						return
					}
					_, _ = io.Copy(io.Discard, req.Body)
					raw, ok := upstreamResponses[req.URL.Path]
					if !ok {
						raw = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
					}
					if _, err := io.WriteString(c, raw); err != nil || strings.HasSuffix(raw, "#close") || req.Close {
						return
					}
				}
			}()
		}
	}()
	return ln.Addr().String()
}

// selfSigned: a TLS config for 127.0.0.1, and a pool trusting it.
func selfSigned(t *testing.T) (*tls.Config, *x509.CertPool) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(7),
		Subject:      pkix.Name{CommonName: "upstream"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(time.Hour),
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
		KeyUsage:     x509.KeyUsageDigitalSignature,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	pool := x509.NewCertPool()
	pool.AddCert(cert)
	return &tls.Config{Certificates: []tls.Certificate{{Certificate: [][]byte{der}, PrivateKey: key}}}, pool
}

// allowing refuses what names /denied, and allows the rest, keeping each URL asked.
type allowing struct{ refusing }

func (p *allowing) Evaluate(_ context.Context, op *pb.Op) (bool, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	id := op.GetSource().GetIdentifier()
	p.seen = append(p.seen, id)
	if strings.Contains(id, "/denied") {
		return false, stderrors.New("refused")
	}
	return true, nil
}

type captured struct {
	Requests   []map[string]any `json:"requests"`
	Materials  []map[string]any `json:"materials"`
	Incomplete []map[string]any `json:"incomplete"`
}

func captureOf(c *network.ProxyCapture, n func(string) string) captured {
	out := captured{Requests: []map[string]any{}, Materials: []map[string]any{}, Incomplete: []map[string]any{}}
	for _, r := range c.Requests() {
		out.Requests = append(out.Requests, map[string]any{"method": r.Method, "url": n(r.URL), "redirect": n(r.RedirectTarget), "status": r.StatusCode})
	}
	materials := c.Materials()
	sort.Slice(materials, func(i, j int) bool { return materials[i].URL < materials[j].URL })
	for _, m := range materials {
		out.Materials = append(out.Materials, map[string]any{"url": n(m.URL), "digest": m.Digest.String()})
	}
	for _, i := range c.Incomplete() {
		out.Incomplete = append(out.Incomplete, map[string]any{"method": i.Method, "url": n(i.URL), "reason": i.Reason})
	}
	return out
}

func mapped(in []string, f func(string) string) []string {
	out := make([]string, 0, len(in))
	for _, s := range in {
		out = append(out, f(s))
	}
	return out
}

type passOut struct {
	Name     string   `json:"name"`
	Request  string   `json:"request"`
	Checked  []string `json:"checked"`
	Response any      `json:"response"`
	Capture  captured `json:"capture"`
}

// passedOn: requests the policy lets go (all but /denied), each sent to the proxy as it
// is, plain and in a tunnel, to upstreams answering upstreamResponses: what the client
// read, the URLs asked, and what the proxy recorded of each. {upstream} and
// {upstream_tls} stand for the upstreams' addresses.
func passedOn(t *testing.T) ([]passOut, []passOut) {
	upstreamTLS, roots := selfSigned(t)
	plainAddr := upstream(t, nil)
	secureAddr := upstream(t, upstreamTLS)

	caPEM, ca, key, err := newCA()
	if err != nil {
		t.Fatal(err)
	}
	transport := newProxyTransport()
	transport.TLSClientConfig = &tls.Config{RootCAs: roots}
	p := &provider{caPEM: caPEM, ca: ca, caKey: key, certs: map[string]*certCacheEntry{}, lru: list.New(), transport: transport}
	policy := &allowing{}
	h := &proxyHandler{provider: p, policy: policy, transport: transport}
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	srv := &http.Server{Handler: h, ReadHeaderTimeout: 30 * time.Second}
	go func() { _ = srv.Serve(ln) }()
	defer srv.Close()
	addr := ln.Addr().String()
	proxyRoots := x509.NewCertPool()
	proxyRoots.AddCert(ca)

	n := func(s string) string {
		return strings.ReplaceAll(strings.ReplaceAll(s, secureAddr, "{upstream_tls}"), plainAddr, "{upstream}")
	}
	fill := func(s string) string {
		return strings.ReplaceAll(strings.ReplaceAll(s, "{upstream_tls}", secureAddr), "{upstream}", plainAddr)
	}
	get := func(path string) string {
		return "GET http://{upstream}" + path + " HTTP/1.1\r\nHost: {upstream}\r\n\r\n"
	}
	var passed []passOut
	for _, c := range [][2]string{
		{"passed on", "GET http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nUser-Agent: t/1\r\nAccept-Encoding: gzip\r\nProxy-Authorization: p\r\n\r\n"},
		{"origin form", "GET /hello HTTP/1.1\r\nHost: {upstream}\r\n\r\n"},
		{"chunked", get("/chunked")},
		{"chunked, long", get("/chunked-long")},
		{"unframed", get("/unframed")},
		{"unframed, long", get("/unframed-long")},
		{"a redirect", get("/redirect")},
		{"a redirect away", get("/away")},
		{"missing", get("/missing")},
		{"no content", get("/empty")},
		{"not modified", get("/unchanged")},
		{"its own date", get("/dated")},
		{"hop-by-hop fields", get("/hops")},
		{"repeated fields", get("/cookies")},
		{"a part", "GET http://{upstream}/part HTTP/1.1\r\nHost: {upstream}\r\nRange: bytes=0-1\r\n\r\n"},
		{"a status of no text", get("/odd-status")},
		{"a reason of its own", get("/reason")},
		{"100 Continue first", get("/continue-first")},
		{"cut short", get("/cut")},
		{"HEAD", "HEAD http://{upstream}/head HTTP/1.1\r\nHost: {upstream}\r\n\r\n"},
		{"an HTTP/1.0 upstream", get("/http10")},
		{"an HTTP/1.0 upstream, unframed", get("/http10-unframed")},
		{"a server error", get("/server-error")},
		{"sniffed", get("/sniffed")},
		{"sniffed as text", get("/sniffed-text")},
		{"encoded, not sniffed", get("/not-sniffed")},
		{"empty", get("/empty-200")},
		{"unframed, just short of what is sniffed", get("/unframed-505")},
		{"unframed, what is sniffed", get("/unframed-506")},
		{"chunked, past what is sniffed", get("/chunked-1000")},
		{"sniffed as binary", get("/sniffed-binary")},
		{"typed, empty", get("/typed-empty")},
		{"sniffed past its first bytes", get("/sniffed-late")},
		{"encoded, empty", get("/encoded-empty")},
		{"unframed, what is sniffed, to an HTTP/1.0 client", "GET http://{upstream}/unframed-506 HTTP/1.0\r\n\r\n"},
		{"a body passed on", "POST http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nContent-Length: 5\r\n\r\nhello"},
		{"a chunked body passed on", "POST http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"},
		{"100-continue passed on", "POST http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello"},
		{"a short body passed on", "POST http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nContent-Length: 50\r\n\r\nhello"},
		{"a bad chunk passed on", "POST http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n"},
		{"an HTTP/1.0 client", "GET http://{upstream}/hello HTTP/1.0\r\n\r\n"},
		{"an HTTP/1.0 client kept alive", "GET http://{upstream}/hello HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"},
		{"an HTTP/1.0 client, unframed", "GET http://{upstream}/unframed-long HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"},
		{"close asked", "GET http://{upstream}/hello HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n"},
		{"pipelined", get("/hello") + get("/chunked")},
		{"refused after passed", get("/hello") + get("/denied")},
		{"redirect then target", get("/redirect") + get("/hello")},
	} {
		capture := network.NewProxyCapture()
		h.capture = capture
		got := exchange(t, addr, []byte(fill(c[1])))
		passed = append(passed, passOut{Name: c[0], Request: c[1], Checked: mapped(policy.take(), n), Response: bytesOf([]byte(n(string(dated(got))))), Capture: captureOf(capture, n)})
	}

	var tunnels []passOut
	in := func(path string) string {
		return "GET " + path + " HTTP/1.1\r\nHost: {upstream_tls}\r\n\r\n"
	}
	for _, c := range [][2]string{
		{"passed in a tunnel", in("/hello")},
		{"chunked", in("/chunked")},
		{"chunked, long", in("/chunked-long")},
		{"unframed", in("/unframed")},
		{"a redirect", in("/redirect")},
		{"missing", in("/missing")},
		{"no content", in("/empty")},
		{"not modified", in("/unchanged")},
		{"hop-by-hop fields", in("/hops")},
		{"a status of no text", in("/odd-status")},
		{"a reason of its own", in("/reason")},
		{"cut short", in("/cut")},
		{"HEAD", "HEAD /head HTTP/1.1\r\nHost: {upstream_tls}\r\n\r\n"},
		{"an HTTP/1.0 upstream, unframed", in("/http10-unframed")},
		{"sniffed", in("/sniffed")},
		{"a body", "POST /hello HTTP/1.1\r\nHost: {upstream_tls}\r\nContent-Length: 5\r\n\r\nhello"},
		{"a short body", "POST /hello HTTP/1.1\r\nHost: {upstream_tls}\r\nContent-Length: 50\r\n\r\nhello"},
		{"an HTTP/1.0 request", "GET /hello HTTP/1.0\r\n\r\n"},
		{"close asked", "GET /hello HTTP/1.1\r\nHost: {upstream_tls}\r\nConnection: close\r\n\r\n"},
		{"pipelined", in("/hello") + in("/chunked")},
		{"refused after passed", in("/hello") + in("/denied")},
	} {
		capture := network.NewProxyCapture()
		h.capture = capture
		connect := "CONNECT " + secureAddr + " HTTP/1.1\r\nHost: " + secureAddr + "\r\n\r\n"
		_, response, handshake := tunnel(t, addr, proxyRoots, connect, "127.0.0.1", []byte(fill(c[1])))
		if !handshake {
			t.Fatalf("%s: no handshake", c[0])
		}
		tunnels = append(tunnels, passOut{Name: c[0], Request: c[1], Checked: mapped(policy.take(), n), Response: bytesOf([]byte(n(string(dated(response))))), Capture: captureOf(capture, n)})
	}
	return passed, tunnels
}
