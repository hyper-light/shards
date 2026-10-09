package policy

// Placed in buildx v0.37.1's policy package by scripts/policy/generate-github: what buildx's
// own github_attestation helpers and golang/snappy v1.0.0 make of each case, for
// crates/shards/src/build/testdata/github-attestation.json.

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"math/rand"
	"os"
	"testing"

	"github.com/golang/snappy"
)

type shardsSnappyCase struct {
	In  string  `json:"in"`
	Out *string `json:"out"`
	Err string  `json:"err"`
}

type shardsResponseCase struct {
	In      string   `json:"in"`
	Bundles []string `json:"bundles"`
	URLs    []string `json:"urls"`
}

type shardsURLCase struct {
	URL      string `json:"url"`
	Snappy   bool   `json:"snappy"`
	Stripped string `json:"stripped"`
}

func shardsSnappy(in []byte) shardsSnappyCase {
	c := shardsSnappyCase{In: base64.StdEncoding.EncodeToString(in)}
	out, err := snappy.Decode(nil, in)
	if err != nil {
		c.Err = err.Error()
		return c
	}
	s := base64.StdEncoding.EncodeToString(out)
	c.Out = &s
	return c
}

func shardsUvarint(v uint64) []byte {
	var b []byte
	for v >= 0x80 {
		b = append(b, byte(v)|0x80)
		v >>= 7
	}
	return append(b, byte(v))
}

func TestShardsGithubOracle(t *testing.T) {
	rng := rand.New(rand.NewSource(106))
	random := func(n int) []byte {
		b := make([]byte, n)
		rng.Read(b)
		return b
	}
	repeated := func(n int, unit string) []byte {
		return bytes.Repeat([]byte(unit), n/len(unit)+1)[:n]
	}
	var plains [][]byte
	plains = append(plains, nil, []byte("a"), []byte("hello hello hello hello"),
		random(100), repeated(5000, "abcdefg"), repeated(70000, "xyzzy and plugh "),
		append(random(70000), repeated(9000, "q")...), repeated(300, "a"))
	for _, n := range []int{1, 59, 60, 61, 255, 256, 257, 65535, 65536, 65537} {
		plains = append(plains, random(n))
	}

	var sn []shardsSnappyCase
	for _, p := range plains {
		enc := snappy.Encode(nil, p)
		sn = append(sn, shardsSnappy(enc))
		// Each encoding mutated: a byte changed, cut short, or grown.
		for i := 0; i < 24 && len(enc) > 0; i++ {
			m := append([]byte(nil), enc...)
			switch i % 3 {
			case 0:
				m[rng.Intn(len(m))] ^= byte(1 + rng.Intn(255))
			case 1:
				m = m[:rng.Intn(len(m))]
			case 2:
				m = append(m, random(1+rng.Intn(8))...)
			}
			sn = append(sn, shardsSnappy(m))
		}
	}
	hand := [][]byte{
		{},
		{0x00},
		{0x00, 0x00},
		{0x80},
		{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01},
		{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02},
		{0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01},
		// Overflowing 64 bits to nothing, and to the top bit alone.
		{0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02},
		{0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01},
		append(shardsUvarint(0xffffffff), 0x00, 'a'),
		append(shardsUvarint(0x100000000), 0x00, 'a'),
		append(shardsUvarint(1<<26), 0x08, 'a', 'b', 'c'),
		append(shardsUvarint(5), 0x08, 'a', 'b', 'c'),
		append(shardsUvarint(3), 0x08, 'a', 'b', 'c'),
		append(shardsUvarint(2), 0x08, 'a', 'b', 'c'),
		append(shardsUvarint(3), 0x08, 'a', 'b'),
		// An overlapping copy (offset 1), a run.
		append(shardsUvarint(11), 0x00, 'z', 0x01|(6<<2), 0x01),
		// Offset 0, offset past what is written, and a copy past the end.
		append(shardsUvarint(5), 0x00, 'z', 0x01, 0x00),
		append(shardsUvarint(5), 0x00, 'z', 0x01, 0x02),
		append(shardsUvarint(3), 0x00, 'z', 0x01, 0x01),
		// Two- and four-byte offsets.
		append(shardsUvarint(6), 0x08, 'a', 'b', 'c', 0x02|(2<<2), 0x03, 0x00),
		append(shardsUvarint(6), 0x08, 'a', 'b', 'c', 0x03|(2<<2), 0x03, 0x00, 0x00, 0x00),
		append(shardsUvarint(6), 0x08, 'a', 'b', 'c', 0x03|(2<<2), 0x03, 0x00, 0x00),
		append(shardsUvarint(6), 0x08, 'a', 'b', 'c', 0x03|(2<<2), 0x00, 0x00, 0x00, 0x01),
		append(shardsUvarint(6), 0x08, 'a', 'b', 'c', 0x02|(2<<2), 0x03),
	}
	for _, n := range []int{1, 2, 60, 61, 256, 257, 300} {
		body := random(n)
		if n <= 60 {
			hand = append(hand, append(append(shardsUvarint(uint64(n)), byte((n-1)<<2)), body...))
			hand = append(hand, append(append(shardsUvarint(uint64(n)), byte((n-1)<<2)), body[:n-1]...))
		}
		for _, form := range []byte{60, 61, 62, 63} {
			x := n - 1
			tag := []byte{form << 2, byte(x), byte(x >> 8), byte(x >> 16), byte(x >> 24)}[:form-58]
			hand = append(hand, append(append(shardsUvarint(uint64(n)), tag...), body...))
			hand = append(hand, append(shardsUvarint(uint64(n)), tag[:len(tag)-1]...))
		}
	}
	for _, h := range hand {
		sn = append(sn, shardsSnappy(h))
	}

	responses := []string{
		``,
		`   `,
		`null`,
		`{}`,
		`[]`,
		`"x"`,
		`{"attestations":null}`,
		`{"attestations":[]}`,
		`{"attestations":{}}`,
		`{"attestations":[{"bundle":{"mediaType":"x"}}]}`,
		`{"attestations":[{"bundle": { "a" : [1, 2] } , "bundle_url":"https://e.example/a.json.sn?sig=1"}]}`,
		`{"attestations":[{"bundle":null,"bundle_url":""}]}`,
		`{"attestations":[{"bundle":"str"},{"bundle":12.5e3},{"bundle":true},{"bundle":[1]}]}`,
		`{"attestations":[null,{"bundle":{"b":1}}]}`,
		`{"attestations":[{"bundle_url":7}]}`,
		`{"attestations":[{"bundle_url":null,"bundle":{"x":"é\n"}}]}`,
		`{"Attestations":[{"BUNDLE":{"k":1},"Bundle_URL":"u"}]}`,
		`{"attestations":[{"bundle":{"k":1}}],"attestations":[{"bundle_url":"v"}]}`,
		`{"attestations":[{"bundle":{"k":1}},{"bundle":{"k":2}}],"attestations":[{"bundle_url":"v"}]}`,
		`{"attestations":[{"bundle":{"k":1}}],"attestations":[{"bundle_url":"v"},{"bundle_url":"w"}]}`,
		`{"attestations":[{"bundle":{"k":1},"bundle":{"k":2}}]}`,
		`{"attestations":[{"bundle":{"k":1}}],"attestations":null}`,
		`{"attestations":[{"bundle":{"k":1}}],"attestations":[]}`,
		`{"attestations":[{"bundle_url":"esc"}]}`,
		`{"attestations":[{"bundle":{"k":1}}]} trailing`,
		`{"attestations":[{"bundle":{"k":1}]}`,
		" {\"attestations\":[{\"bundle\":{\"k\":1}}]} ",
		"\t{\"attestations\":[{\"bundle\":\t{\"k\":1}\t}]}\n",
		`{"attestations":[{"bundle":{"k":1}}],"other":{"deep":[1,2,{"x":null}]}}`,
		`{"attestations":[{"bundle":{"k":"\ud800"}}]}`,
		`{"attestations":[{"bundle":{"k":1}},"oops"]}`,
		`{"attestations":[{"bundle_url":"a"},{"bundle_url":"b"},{"bundle_url":"c"}]}`,
	}
	var rs []shardsResponseCase
	for _, r := range responses {
		bs, urls := githubAttestationBundlesFromResponse([]byte(r))
		c := shardsResponseCase{In: r, Bundles: []string{}, URLs: urls}
		if c.URLs == nil {
			c.URLs = []string{}
		}
		for _, b := range bs {
			c.Bundles = append(c.Bundles, string(b))
		}
		rs = append(rs, c)
	}

	urls := []string{
		"https://tmaproduction.blob.core.windows.net/attestations/212613049/2026/09/30/51331539.json.sn?se=2026-10-09T19%3A50%3A46Z&sig=abc",
		"https://example.com/a.json",
		"https://example.com/a.json.sn",
		"https://example.com/a.JSON.SN",
		"https://example.com/a%2Ejson.sn?x",
		"https://example.com/dir/a.json.sn#frag",
		"https://example.com/a.json.sn?",
		"https://example.com/a b.json.sn?q=1",
		"https://user:pw@example.com/a.json.sn?q=1",
		"http://[::1]:8080/a.json.sn?q=1",
		"%zz",
		"https://example.com/%zz.json.sn",
		"",
		"/relative/a.json.sn?x=y",
		"https://example.com/a.json.sn/?x=y",
		"https://EXAMPLE.com/a%20b?x=y",
	}
	var us []shardsURLCase
	for _, u := range urls {
		us = append(us, shardsURLCase{URL: u, Snappy: shouldDecodeSnappyBundleURL(u), Stripped: stripRawQuery(u)})
	}

	out, err := json.MarshalIndent(map[string]any{"snappy": sn, "responses": rs, "urls": us}, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_POLICY_OUT"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
