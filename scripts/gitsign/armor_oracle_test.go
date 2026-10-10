package main

// go-crypto's armor.Decode (v1.4.1, as buildx v0.37.1 vendors it), with the block's body
// read by io.ReadAll as BuildKit's pgpsign.ParseArmoredDetachedSignature reads it, and
// BuildKit's pgpsign.ReadAllArmoredKeyRings: their answers to the inputs below, for
// crates/gitsign/tests/adversarial.rs.
//
//   - `cases`: inputs written out (or, for the large ones, built: `head`, then `repeat`
//     times `unit` with its index for each `%d`, then `tail`, checked by SHA-256): the
//     type, the headers (sorted; for the large ones their count and SHA-256) and the
//     body (for the large ones its digest), or the error.
//   - `fuzz`: inputs that armorFuzz builds from each seed (a xorshift64* the Rust test
//     runs too, checked by each input's SHA-256), answered by armorSummary.
//   - `sweeps`: lines of `width` symbols, one quantum of them "QQ==" at each place in
//     turn: where io.ReadAll's reads end the decoder's chunks, and so which it takes.
//   - `keyRings`: ReadAllArmoredKeyRings's errors.
//
// `generate-armor` copies this file into buildx's cmd/buildx and runs it there.

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sort"
	"strconv"
	"strings"
	"testing"

	"github.com/ProtonMail/go-crypto/openpgp/armor"
	"github.com/moby/buildkit/util/pgpsign"
)

type armorCase struct {
	Name          string     `json:"name"`
	Input         string     `json:"input,omitempty"`
	Head          string     `json:"head,omitempty"`
	Unit          string     `json:"unit,omitempty"`
	Repeat        int        `json:"repeat,omitempty"`
	Tail          string     `json:"tail,omitempty"`
	SHA256        string     `json:"sha256,omitempty"`
	Error         string     `json:"error,omitempty"`
	Type          string     `json:"type,omitempty"`
	Headers       [][]string `json:"headers,omitempty"`
	HeaderCount   int        `json:"headerCount,omitempty"`
	HeadersSHA256 string     `json:"headersSha256,omitempty"`
	Body          string     `json:"body,omitempty"`
}

type armorFuzzCase struct {
	Seed   uint64 `json:"seed"`
	SHA256 string `json:"sha256"`
	Answer string `json:"answer"`
}

type armorSweep struct {
	Width   int      `json:"width"`
	Lines   int      `json:"lines"`
	Answers []string `json:"answers"`
}

type keyRingCase struct {
	Name   string `json:"name"`
	Input  string `json:"input"`
	Answer string `json:"answer"`
}

type armorOracle struct {
	Cases    []armorCase     `json:"cases"`
	Fuzz     []armorFuzzCase `json:"fuzz"`
	Sweeps   []armorSweep    `json:"sweeps"`
	KeyRings []keyRingCase   `json:"keyRings"`
}

func readArmor(data []byte) (*armor.Block, []byte, error) {
	p, err := armor.Decode(bytes.NewReader(data))
	if err != nil {
		return nil, nil, err
	}
	body, err := io.ReadAll(p.Body)
	if err != nil {
		return nil, nil, err
	}
	return p, body, nil
}

func sortedHeaders(h map[string]string) []string {
	keys := make([]string, 0, len(h))
	for k := range h {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func headersDigest(h map[string]string) string {
	d := sha256.New()
	for _, k := range sortedHeaders(h) {
		fmt.Fprintf(d, "%s\x00%s\n", k, h[k])
	}
	return hex.EncodeToString(d.Sum(nil))
}

func digest16(b []byte) string {
	s := sha256.Sum256(b)
	return hex.EncodeToString(s[:8])
}

func armorAnswer(c *armorCase, data []byte, summarize bool) {
	p, body, err := readArmor(data)
	if err != nil {
		c.Error = err.Error()
		return
	}
	c.Type = p.Type
	if summarize {
		c.HeaderCount = len(p.Header)
		c.HeadersSHA256 = headersDigest(p.Header)
		c.Body = digest16(body)
		return
	}
	for _, k := range sortedHeaders(p.Header) {
		c.Headers = append(c.Headers, []string{k, p.Header[k]})
	}
	c.Body = base64.StdEncoding.EncodeToString(body)
}

// armorSummary: the error, or "ok", the type, the headers' digest, the body's length and
// digest (each digest the first 16 hex digits of a SHA-256).
func armorSummary(data []byte) string {
	p, body, err := readArmor(data)
	if err != nil {
		return err.Error()
	}
	return fmt.Sprintf("ok %q %s %d %s", p.Type, headersDigest(p.Header)[:16], len(body), digest16(body))
}

type xorshift uint64

func newXorshift(seed uint64) *xorshift {
	x := xorshift(seed * 0x9E3779B97F4A7C15)
	return &x
}

func (x *xorshift) next() uint64 {
	v := uint64(*x)
	v ^= v >> 12
	v ^= v << 25
	v ^= v >> 27
	*x = xorshift(v)
	return v * 0x2545F4914F6CDD1D
}

func (x *xorshift) below(n int) int { return int(x.next() % uint64(n)) }

const symbols = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

var specials = []string{"=", "==", "\r", " ", "\t", "\v", "\u00a0", "!", ":", "-"}

// fuzzText: up to max atoms, each a symbol or, `percent` times in a hundred, a special.
func fuzzText(x *xorshift, max, percent int) string {
	var b strings.Builder
	for n := x.below(max + 1); n > 0; n-- {
		if x.below(100) < percent {
			b.WriteString(specials[x.below(len(specials))])
		} else {
			b.WriteByte(symbols[x.below(64)])
		}
	}
	return b.String()
}

func armorFuzz(seed uint64) []byte {
	x := newXorshift(seed)
	var b strings.Builder
	for n := x.below(3); n > 0; n-- {
		b.WriteString(fuzzText(x, 140, 10))
		b.WriteString("\n")
	}
	b.WriteString("-----BEGIN PGP SIGNATURE-----\n")
	for n := x.below(4); n > 0; n-- {
		b.WriteString("K" + strconv.Itoa(x.below(3)) + ": ")
		b.WriteString(fuzzText(x, 220, 5))
		b.WriteString("\n")
	}
	b.WriteString([]string{"\n", " \n", "\r\n", "\u00a0\n"}[x.below(4)])
	if x.below(2) == 0 {
		// Whole lines of symbols, one quantum of them padded.
		width := 4 * (1 + x.below(24))
		lines := 1 + x.below(120)
		pad := x.below(lines * width / 4)
		for l := 0; l < lines; l++ {
			for q := 0; q < width/4; q++ {
				if l*width/4+q == pad {
					b.WriteString("QQ==")
					continue
				}
				for k := 0; k < 4; k++ {
					b.WriteByte(symbols[x.below(64)])
				}
			}
			b.WriteString("\n")
		}
	} else {
		for n := 1 + x.below(24); n > 0; n-- {
			b.WriteString(fuzzText(x, 100, 6))
			b.WriteString([]string{"\n", "\r\n"}[x.below(2)])
		}
	}
	b.WriteString([]string{
		"-----END PGP SIGNATURE-----\n",
		"=AAAA\n-----END PGP SIGNATURE-----\n",
		"",
		"-----END PGP SIGNATURE-----",
	}[x.below(4)])
	return []byte(b.String())
}

// sweepInput: `lines` lines of `width` symbols, their quantum `pad` "QQ==".
func sweepInput(width, lines, pad int) []byte {
	x := newXorshift(uint64(width))
	stream := make([]byte, width*lines)
	for i := range stream {
		stream[i] = symbols[x.below(64)]
	}
	copy(stream[4*pad:], "QQ==")
	var b strings.Builder
	b.WriteString("-----BEGIN PGP SIGNATURE-----\n\n")
	for l := 0; l < lines; l++ {
		b.Write(stream[l*width : (l+1)*width])
		b.WriteString("\n")
	}
	b.WriteString("-----END PGP SIGNATURE-----\n")
	return []byte(b.String())
}

func TestShardsArmor(t *testing.T) {
	out := os.Getenv("SHARDS_ARMOR_OUT")
	if out == "" {
		t.Skip("SHARDS_ARMOR_OUT names the file to write")
	}
	const begin = "-----BEGIN PGP SIGNATURE-----\n"
	const end = "-----END PGP SIGNATURE-----\n"
	long := strings.Repeat("x", 120)
	line64 := strings.Repeat("QUJD", 16) + "\n"
	small := []struct{ name, input string }{
		{"a block", begin + "\nQUJD\n" + end},
		{"headers", begin + "Version: v\nComment: c\n\nQUJD\n" + end},
		{"a header repeated", begin + "A: 1\nA: 2\n\nQUJD\n" + end},
		{"a header without its space", begin + "A:1\nB:\n\nQUJD\n" + end},
		{"a long header, continued", begin + "A: " + long + "\n\nQUJD\n" + end},
		{"a header continued twice", begin + "A: " + long + long + "\n\nQUJD\n" + end},
		{"a repeated header, continued", begin + "A: 1\nB: 2\nA: " + long + "\n\nQUJD\n" + end},
		{"a header of a hundred bytes", begin + "A: " + strings.Repeat("y", 97) + "\nB: 2\n\nQUJD\n" + end},
		{"a header of ninety-nine bytes and a carriage return", begin + "A: " + strings.Repeat("y", 96) + "\r\nB: 2\n\nQUJD\n" + end},
		{"a blank line of 120 spaces", begin + strings.Repeat(" ", 120) + "\nQUJD\n" + end},
		{"a no-break space before BEGIN", "\u00a0" + begin + "\nQUJD\n" + end},
		{"a vertical tab before BEGIN", "\v" + begin + "\nQUJD\n" + end},
		{"a blank line of a no-break space", begin + "\u00a0\nQUJD\n" + end},
		{"a no-break space after a body line", begin + "\nQUJD\u00a0\n" + end},
		{"a vertical tab about a body line", begin + "\n\vQUJD\v\n" + end},
		{"a carriage return within a body line", begin + "\nQU\rJD\n" + end},
		{"carriage returns within a body line", begin + "\nQ\r\r\rUJD\n" + end},
		{"a line feed made carriage returns", strings.ReplaceAll(begin+"\nQUJD\n"+end, "\n", "\r\n")},
		{"a checksum line", begin + "\nQUJD\n=AAAA\n" + end},
		{"a checksum line, then more", begin + "\nQUJD\n=AAAA\nREVG\n" + end},
		{"a body line of 97", begin + "\n" + strings.Repeat("Q", 97) + "\n" + end},
		{"a body line of 96", begin + "\n" + strings.Repeat("QUJD", 24) + "\n" + end},
		{"a body line past the buffer", begin + "\n" + strings.Repeat("QUJD", 30) + "\n" + end},
		{"no blank line after the headers", begin + "A: 1\nQUJD\n" + end},
		{"a header line without a colon, then a block", begin + "nocolon\n" + begin + "\nREVG\n" + end},
		{"a long header line without a colon, then BEGIN", begin + strings.Repeat("n", 100) + begin + "\nREVG\n" + end},
		{"bad base64", begin + "\nQU!D\n" + end},
		{"bad base64 on the second line", begin + "\nQUJD\nQU!D\n" + end},
		{"bad base64 after a short line", begin + "\nQU\nJ!\n" + end},
		{"a short body", begin + "\nQUJ\n" + end},
		{"a line of three, then of one", begin + "\nQUJ\nD\n" + end},
		{"a padded line, then another", begin + "\nQQ==\nQUJD\n" + end},
		{"padding within a line", begin + "\nQQ==QUJD\n" + end},
		{"padding split across lines", begin + "\nQQ=\n=\n" + end},
		{"padding, then a line of two", begin + "\nQUI=\nQQ\n" + end},
		{"three padding symbols", begin + "\nQQ===\n" + end},
		{"padding where io.ReadAll's first read ends", begin + "\n" + strings.Repeat(line64, 10) + strings.Repeat("QUJD", 9) + "QQ==" + strings.Repeat("QUJD", 6) + "\n" + end},
		{"padding just past it", begin + "\n" + strings.Repeat(line64, 10) + strings.Repeat("QUJD", 10) + "QQ==" + strings.Repeat("QUJD", 5) + "\n" + end},
		{"no END", begin + "\nQUJD\n"},
		{"nothing", ""},
		{"a long garbage line before BEGIN", strings.Repeat("g", 250) + "\n" + begin + "\nQUJD\n" + end},
		{"BEGIN within a long line", strings.Repeat("g", 90) + begin + "\nQUJD\n" + end},
	}
	var o armorOracle
	for _, c := range small {
		ac := armorCase{Name: c.name, Input: base64.StdEncoding.EncodeToString([]byte(c.input))}
		armorAnswer(&ac, []byte(c.input), false)
		o.Cases = append(o.Cases, ac)
	}
	large := []struct {
		name, head, unit string
		repeat           int
		tail             string
	}{
		{"distinct headers", begin, "Header-%d: value\n", 60000, "\nQUJD\n" + end},
		{"one header, set again and again", begin, "Header: value %d\n", 60000, "\nQUJD\n" + end},
		{"a long body", begin + "\n", "QUJDQUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAx\n", 20000, end},
	}
	for _, c := range large {
		var b strings.Builder
		b.WriteString(c.head)
		for i := 0; i < c.repeat; i++ {
			b.WriteString(strings.ReplaceAll(c.unit, "%d", strconv.Itoa(i)))
		}
		b.WriteString(c.tail)
		data := []byte(b.String())
		sum := sha256.Sum256(data)
		ac := armorCase{
			Name:   c.name,
			Head:   base64.StdEncoding.EncodeToString([]byte(c.head)),
			Unit:   base64.StdEncoding.EncodeToString([]byte(c.unit)),
			Repeat: c.repeat,
			Tail:   base64.StdEncoding.EncodeToString([]byte(c.tail)),
			SHA256: hex.EncodeToString(sum[:]),
		}
		armorAnswer(&ac, data, true)
		o.Cases = append(o.Cases, ac)
	}
	for seed := uint64(1); seed <= 600; seed++ {
		data := armorFuzz(seed)
		sum := sha256.Sum256(data)
		o.Fuzz = append(o.Fuzz, armorFuzzCase{Seed: seed, SHA256: hex.EncodeToString(sum[:]), Answer: armorSummary(data)})
	}
	for _, s := range []struct{ width, lines int }{{92, 40}, {96, 40}, {64, 14}} {
		sw := armorSweep{Width: s.width, Lines: s.lines}
		for pad := 0; pad < s.width*s.lines/4; pad++ {
			sw.Answers = append(sw.Answers, armorSummary(sweepInput(s.width, s.lines, pad)))
		}
		o.Sweeps = append(o.Sweeps, sw)
	}
	const keyBegin = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n"
	const keyEnd = "-----END PGP PUBLIC KEY BLOCK-----\n"
	rings := []struct{ name, input string }{
		{"nothing", ""},
		{"a signature block, its body corrupt", begin + "\n" + strings.Repeat("Q", 97) + "\n" + end},
		{"a key block, a line too long", keyBegin + strings.Repeat("Q", 97) + "\n" + keyEnd},
		{"a key block, bad base64", keyBegin + "QU!D\n" + keyEnd},
		{"a key block, its base64 short", keyBegin + "QUJ\n" + keyEnd},
		{"a key block, its headers cut short", "-----BEGIN PGP PUBLIC KEY BLOCK-----\nA: 1\n"},
	}
	for _, c := range rings {
		var answer string
		if ents, err := pgpsign.ReadAllArmoredKeyRings([]byte(c.input)); err != nil {
			answer = err.Error()
		} else {
			answer = fmt.Sprintf("ok %d", len(ents))
		}
		o.KeyRings = append(o.KeyRings, keyRingCase{Name: c.name, Input: base64.StdEncoding.EncodeToString([]byte(c.input)), Answer: answer})
	}
	data, err := json.MarshalIndent(o, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
