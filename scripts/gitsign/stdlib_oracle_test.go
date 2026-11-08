package stdliboracle

// Go 1.26's standard library, for crates/gitsign's tests. TestShardsPem: encoding/pem.Decode's
// answers to the inputs below (the first block's type, headers and bytes, or none), for
// tests/adversarial.rs; the large inputs are recorded by their SHA-256 and how they are
// built (`repeat` times `unit`, then `tail`), which the Rust test builds again.
// TestShardsStdlib: encoding/base64's StdEncoding.Decode (into DecodedLen bytes: the bytes
// written and the error) and bytes.TrimSpace, for src/go.rs. `generate-stdlib` runs them
// with the go on PATH, which must be the 1.26 line.

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"os"
	"runtime"
	"sort"
	"strings"
	"testing"
)

type decodeCase struct {
	Input string `json:"input"`
	Out   string `json:"out"`
	Error string `json:"error,omitempty"`
}

type trimCase struct {
	Input string `json:"input"`
	Out   string `json:"out"`
}

type stdlibOracle struct {
	Decode []decodeCase `json:"decode"`
	Trim   []trimCase   `json:"trim"`
}

type xorshift uint64

func (x *xorshift) next() uint64 {
	v := uint64(*x)
	v ^= v >> 12
	v ^= v << 25
	v ^= v >> 27
	*x = xorshift(v)
	return v * 0x2545F4914F6CDD1D
}

func (x *xorshift) below(n int) int { return int(x.next() % uint64(n)) }

// atoms: up to `max` of `from`, its first nine times as likely as each other.
func atoms(x *xorshift, max int, from []string) string {
	var b strings.Builder
	for n := x.below(max + 1); n > 0; n-- {
		i := x.below(len(from) + 8)
		if i >= len(from) {
			i = 0
		}
		b.WriteString(from[i])
	}
	return b.String()
}

func TestShardsStdlib(t *testing.T) {
	out := os.Getenv("SHARDS_STDLIB_OUT")
	if out == "" {
		t.Skip("SHARDS_STDLIB_OUT names the file to write")
	}
	if !strings.HasPrefix(runtime.Version(), "go1.26") {
		t.Fatalf("the standard library of go1.26 is the oracle, not %s", runtime.Version())
	}
	var o stdlibOracle
	decodes := []string{
		"", "QUJD", "QUI=", "QQ==", "QQ\r\n=\n=", "QQ=A", "QQ=", "QQ==QUJD", "QUJDQ", "QUJDQUI",
		"QU\n", "Q===", "QU!D", "QUJ=", "\n\r", "QUJ", "Q", "QQ==\n", "QQ==\nQ", "QUJ=\r\n",
		"QUJD\nQUJD", "QUJDQUJDQUJD", "QUJDQUJDQUJDQUJDQUJDQUJD!", "QUJDQUJD=", "QUJDQUJDQQ=x",
		"QU JD", "Q\nQ\nQ\nQ", "QR==", "QUJE", "////", "+/+/", "QQ===", "QQ==\r\r\n\n",
	}
	x := xorshift(0x9E3779B97F4A7C15)
	for i := 0; i < 800; i++ {
		decodes = append(decodes, atoms(&x, 24, []string{"Q", "U", "J", "D", "=", "\r", "\n", "!", " ", "/", "+", "z"}))
	}
	for _, in := range decodes {
		dst := make([]byte, base64.StdEncoding.DecodedLen(len(in)))
		n, err := base64.StdEncoding.Decode(dst, []byte(in))
		c := decodeCase{Input: base64.StdEncoding.EncodeToString([]byte(in)), Out: base64.StdEncoding.EncodeToString(dst[:n])}
		if err != nil {
			c.Error = err.Error()
		}
		o.Decode = append(o.Decode, c)
	}
	spaces := []string{
		"x", " ", "\t", "\v", "\f", "\r", "\n", "\u0085", "\u00a0", "\u1680", "\u2000", "\u200a",
		"\u2028", "\u2029", "\u202f", "\u205f", "\u3000", "\u200b", "\ufeff", "\x85", "\xc2",
		"\xe3\x80", "\xa0", "\u00e9", "\x00",
	}
	trims := []string{"", " x ", "\u00a0x\u3000", "\xff x \xff", "\u0085 \u2028", "x\xc2", "\xc2\xa0\xc2"}
	for i := 0; i < 600; i++ {
		trims = append(trims, atoms(&x, 8, spaces))
	}
	for _, in := range trims {
		o.Trim = append(o.Trim, trimCase{
			Input: base64.StdEncoding.EncodeToString([]byte(in)),
			Out:   base64.StdEncoding.EncodeToString(bytes.TrimSpace([]byte(in))),
		})
	}
	data, err := json.MarshalIndent(o, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

type pemCase struct {
	Name    string     `json:"name"`
	Input   string     `json:"input,omitempty"`
	Unit    string     `json:"unit,omitempty"`
	Repeat  int        `json:"repeat,omitempty"`
	Tail    string     `json:"tail,omitempty"`
	SHA256  string     `json:"sha256,omitempty"`
	Found   bool       `json:"found"`
	Type    string     `json:"type,omitempty"`
	Headers [][]string `json:"headers,omitempty"`
	Bytes   string     `json:"bytes,omitempty"`
}

func pemAnswer(c *pemCase, data []byte) {
	p, _ := pem.Decode(data)
	if p == nil {
		return
	}
	c.Found = true
	c.Type = p.Type
	keys := make([]string, 0, len(p.Headers))
	for k := range p.Headers {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		c.Headers = append(c.Headers, []string{k, p.Headers[k]})
	}
	c.Bytes = base64.StdEncoding.EncodeToString(p.Bytes)
}

func TestShardsPem(t *testing.T) {
	out := os.Getenv("SHARDS_PEM_OUT")
	if out == "" {
		t.Skip("SHARDS_PEM_OUT names the file to write")
	}
	if !strings.HasPrefix(runtime.Version(), "go1.26") {
		t.Fatalf("encoding/pem of go1.26 is the oracle, not %s", runtime.Version())
	}
	small := []struct{ name, input string }{
		{"one block", "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----\n"},
		{"preamble and rest", "junk\n-----BEGIN A-----\nQUJD\n-----END A-----\nmore"},
		{"two BEGIN lines, the last's END", "-----BEGIN A-----\n-----BEGIN B-----\nQUJD\n-----END B-----\n"},
		{"a BEGIN with no END, then a block", "-----BEGIN A-----\nQUJD\n-----BEGIN B-----\nREVG\n-----END B-----\n"},
		{"crlf", "-----BEGIN A-----\r\nQUJD\r\n-----END A-----\r\n"},
		{"two carriage returns", "-----BEGIN A-----\r\r\nQUJD\n-----END A-----\n"},
		{"spaces and tabs after the type", "-----BEGIN A----- \t\nQUJD\n-----END A-----\n"},
		{"a carriage return then a space", "-----BEGIN A-----\r \nQUJD\n-----END A-----\n"},
		{"headers", "-----BEGIN A-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nQUJD\n-----END A-----\n"},
		{"a header repeated", "-----BEGIN A-----\nK: one\nK: two\n\nQUJD\n-----END A-----\n"},
		{"unicode spaces about a header", "-----BEGIN A-----\n\u00a0Proc-Type\u00a0: \u20004,ENCRYPTED\u3000\n\nQUJD\n-----END A-----\n"},
		{"a vertical tab about a header", "-----BEGIN A-----\n\vK\v: \vv\v\n\nQUJD\n-----END A-----\n"},
		{"invalid utf-8 about a header", "-----BEGIN A-----\n\xffK : v\xff\n\nQUJD\n-----END A-----\n"},
		{"headers then END at once", "-----BEGIN A-----\nK: v\n-----END A-----\n"},
		{"empty body", "-----BEGIN A-----\n-----END A-----\n"},
		{"empty body, a blank line", "-----BEGIN A-----\n\n-----END A-----\n"},
		{"a one-byte body", "-----BEGIN A-----\nQ\n-----END A-----\n"},
		{"a one-byte body of a space", "-----BEGIN A-----\n \n-----END A-----\n"},
		{"junk after END", "-----BEGIN A-----\nQUJD\n-----END A----- x\n"},
		{"END of another type", "-----BEGIN A-----\nQUJD\n-----END B-----\n"},
		{"END of another type, then a block", "-----BEGIN A-----\nQUJD\n-----END B-----\n-----BEGIN C-----\nREVG\n-----END C-----\n"},
		{"bad base64, then a block", "-----BEGIN A-----\nQU!D\n-----END A-----\n-----BEGIN C-----\nREVG\n-----END C-----\n"},
		{"spaces and tabs in the body", "-----BEGIN A-----\nQU JD\tREVG\n-----END A-----\n"},
		{"padding", "-----BEGIN A-----\nQUI=\n-----END A-----\n"},
		{"padding split by a line break", "-----BEGIN A-----\nQQ=\n=\n-----END A-----\n"},
		{"trailing bits", "-----BEGIN A-----\nQUJE\nQR==\n-----END A-----\n"},
		{"missing padding", "-----BEGIN A-----\nQQ\n-----END A-----\n"},
		{"no END", "-----BEGIN A-----\nQUJD\n"},
		{"BEGIN not at a line's start", "x-----BEGIN A-----\nQUJD\n-----END A-----\n"},
		{"no type line end", "-----BEGIN A\nQUJD\n-----END A-----\n"},
		{"END without a line end", "-----BEGIN A-----\nQUJD\n-----END A-----"},
		{"nothing", ""},
	}
	var cases []pemCase
	for _, c := range small {
		pc := pemCase{Name: c.name, Input: base64.StdEncoding.EncodeToString([]byte(c.input))}
		pemAnswer(&pc, []byte(c.input))
		cases = append(cases, pc)
	}
	large := []struct {
		name, unit string
		repeat     int
		tail       string
	}{
		{"BEGIN lines, no END", "\n-----BEGIN A-----", 50000, ""},
		{"BEGIN lines, then a block", "\n-----BEGIN A-----", 50000, "\n-----BEGIN B-----\nQUJD\n-----END B-----\n"},
		{"END lines of another type", "\n-----BEGIN A-----\nQUJD\n-----END B-----", 30000, "\n-----BEGIN C-----\nQUJD\n-----END C-----\n"},
	}
	for _, c := range large {
		data := []byte(strings.Repeat(c.unit, c.repeat) + c.tail)
		sum := sha256.Sum256(data)
		pc := pemCase{
			Name:   c.name,
			Unit:   base64.StdEncoding.EncodeToString([]byte(c.unit)),
			Repeat: c.repeat,
			Tail:   base64.StdEncoding.EncodeToString([]byte(c.tail)),
			SHA256: hex.EncodeToString(sum[:]),
		}
		pemAnswer(&pc, data)
		cases = append(cases, pc)
	}
	data, err := json.MarshalIndent(cases, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
