package main

// Go's crypto/x509 answers (Go 1.26, as buildx v0.37.1 builds with it) for
// crates/sigstore/tests/x509.rs: certificates parsed by ParseCertificate (real Sigstore
// ones, generated ones with every extension processExtensions reads, and each mutated at
// every class of DER position), and chains verified by Certificate.Verify (name
// constraints, policies, key usages, path lengths, validity, candidate order, signature
// algorithms). `generate-x509` copies this file into its own directory of buildx's cmd
// and runs it there.

import (
	"bytes"
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/md5"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/asn1"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"testing"
	"testing/cryptotest"
	"time"
)

// A parse case's DER: the table's entry `base` with its octets between `pre` and the last
// `post` replaced by `mid` (hex).
type parseCase struct {
	Name   string         `json:"name"`
	Base   int            `json:"base"`
	Pre    int            `json:"pre"`
	Post   int            `json:"post"`
	Mid    string         `json:"mid"`
	Error  string         `json:"error"`
	Fields map[string]any `json:"fields,omitempty"`
}

// A verify case's certificates are entries of the table.
type verifyCase struct {
	Name   string  `json:"name"`
	Certs  []int   `json:"certs"`
	Leaf   int     `json:"leaf"`
	Roots  []int   `json:"roots"`
	Inters []int   `json:"inters"`
	Now    int64   `json:"now"`
	Usages []int   `json:"usages"`
	Error  string  `json:"error"`
	Chains [][]int `json:"chains"`
}

type oracle struct {
	DER    []string     `json:"der"`
	Parse  []parseCase  `json:"parse"`
	Verify []verifyCase `json:"verify"`
}

// The DER table, each entry once.
var table [][]byte

func entry(der []byte) int {
	for i, d := range table {
		if bytes.Equal(d, der) {
			return i
		}
	}
	table = append(table, der)
	return len(table) - 1
}

var t0 = time.Date(2026, 1, 15, 12, 0, 0, 0, time.UTC)

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// --- the fields of a parsed certificate that verification and Sigstore read.

func keyString(k any) string {
	switch k := k.(type) {
	case *rsa.PublicKey:
		return fmt.Sprintf("rsa:%s:%d", k.N.Text(16), k.E)
	case *ecdsa.PublicKey:
		b, err := k.Bytes()
		if err != nil {
			return "ecdsa:bad"
		}
		return fmt.Sprintf("ecdsa:%s:%x", k.Curve.Params().Name, b)
	case ed25519.PublicKey:
		return fmt.Sprintf("ed25519:%x", []byte(k))
	case nil:
		return "none"
	default:
		return "dsa"
	}
}

func oidHex(o x509.OID) string {
	b, _ := o.MarshalBinary()
	return hex.EncodeToString(b)
}

func optInt(v int, zero bool) any {
	if v != 0 || zero {
		return v
	}
	return nil
}

func nets(l []*net.IPNet) []string {
	out := []string{}
	for _, n := range l {
		out = append(out, hex.EncodeToString(n.IP)+"/"+hex.EncodeToString(n.Mask))
	}
	return out
}

func strs(l []string) []string {
	if l == nil {
		return []string{}
	}
	return l
}

func fields(c *x509.Certificate) map[string]any {
	f := map[string]any{
		"version":    c.Version,
		"serial":     c.SerialNumber.String(),
		"sigAlg":     c.SignatureAlgorithm.String(),
		"issuer":     c.Issuer.String(),
		"subject":    c.Subject.String(),
		"notBefore":  c.NotBefore.Unix(),
		"notAfter":   c.NotAfter.Unix(),
		"key":        keyString(c.PublicKey),
		"keyUsage":   int(c.KeyUsage),
		"bc":         c.BasicConstraintsValid,
		"isCA":       c.IsCA,
		"maxPathLen": 0,
		"dns":        strs(c.DNSNames),
		"emails":     strs(c.EmailAddresses),
		"ski":        hex.EncodeToString(c.SubjectKeyId),
		"aki":        hex.EncodeToString(c.AuthorityKeyId),
	}
	if c.BasicConstraintsValid {
		f["maxPathLen"] = c.MaxPathLen
	}
	ekus := []int{}
	for _, e := range c.ExtKeyUsage {
		ekus = append(ekus, int(e))
	}
	f["ekus"] = ekus
	unknown := []string{}
	for _, e := range c.UnknownExtKeyUsage {
		unknown = append(unknown, e.String())
	}
	f["unknownEkus"] = unknown
	ips := []string{}
	for _, ip := range c.IPAddresses {
		ips = append(ips, ip.String())
	}
	f["ips"] = ips
	uris := []string{}
	for _, u := range c.URIs {
		uris = append(uris, u.String())
	}
	f["uris"] = uris
	unhandled := []string{}
	for _, o := range c.UnhandledCriticalExtensions {
		unhandled = append(unhandled, o.String())
	}
	f["unhandled"] = unhandled
	hasNC := false
	for _, e := range c.Extensions {
		if e.Id.Equal(asn1.ObjectIdentifier{2, 5, 29, 30}) {
			hasNC = true
		}
	}
	if hasNC {
		f["nc"] = map[string]any{
			"pDNS": strs(c.PermittedDNSDomains), "xDNS": strs(c.ExcludedDNSDomains),
			"pIP": nets(c.PermittedIPRanges), "xIP": nets(c.ExcludedIPRanges),
			"pEmail": strs(c.PermittedEmailAddresses), "xEmail": strs(c.ExcludedEmailAddresses),
			"pURI": strs(c.PermittedURIDomains), "xURI": strs(c.ExcludedURIDomains),
		}
	}
	pol := []string{}
	for _, p := range c.Policies {
		pol = append(pol, oidHex(p))
	}
	f["policies"] = pol
	maps := [][]string{}
	for _, m := range c.PolicyMappings {
		maps = append(maps, []string{oidHex(m.IssuerDomainPolicy), oidHex(m.SubjectDomainPolicy)})
	}
	f["mappings"] = maps
	f["requireExplicit"] = optInt(c.RequireExplicitPolicy, c.RequireExplicitPolicyZero)
	f["inhibitMapping"] = optInt(c.InhibitPolicyMapping, c.InhibitPolicyMappingZero)
	f["inhibitAny"] = optInt(c.InhibitAnyPolicy, c.InhibitAnyPolicyZero)
	return f
}

func parseOne(name string, der []byte) parseCase {
	return parseFrom(name, der, der)
}

// parseFrom: `der` recorded as its difference from `base`.
func parseFrom(name string, base, der []byte) parseCase {
	pre := 0
	for pre < len(base) && pre < len(der) && base[pre] == der[pre] {
		pre++
	}
	post := 0
	for post < len(base)-pre && post < len(der)-pre && base[len(base)-1-post] == der[len(der)-1-post] {
		post++
	}
	pc := parseCase{Name: name, Base: entry(base), Pre: pre, Post: post, Mid: hex.EncodeToString(der[pre : len(der)-post])}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		pc.Error = err.Error()
		return pc
	}
	pc.Fields = fields(c)
	return pc
}

// --- DER trees, to mutate one element and re-encode the rest around it.

type node struct {
	tag      byte
	content  []byte
	children []*node
	// header, where a mutation fixes it; else derived.
	header []byte
}

func isConstructed(tag byte) bool { return tag&0x20 != 0 }

func lenBytes(n int) []byte {
	if n < 0x80 {
		return []byte{byte(n)}
	}
	var b []byte
	for n > 0 {
		b = append([]byte{byte(n)}, b...)
		n >>= 8
	}
	return append([]byte{0x80 | byte(len(b))}, b...)
}

func parseTLV(b []byte) (*node, []byte, bool) {
	if len(b) < 2 || b[0]&0x1f == 0x1f {
		return nil, nil, false
	}
	tag := b[0]
	l := int(b[1])
	off := 2
	if l&0x80 != 0 {
		n := l & 0x7f
		if n == 0 || n > 3 || len(b) < 2+n {
			return nil, nil, false
		}
		l = 0
		for i := 0; i < n; i++ {
			l = l<<8 | int(b[2+i])
		}
		off = 2 + n
	}
	if len(b) < off+l {
		return nil, nil, false
	}
	c := b[off : off+l]
	nd := &node{tag: tag, content: append([]byte(nil), c...)}
	if isConstructed(tag) {
		rest := c
		for len(rest) > 0 {
			ch, r, ok := parseTLV(rest)
			if !ok {
				nd.children = nil
				break
			}
			nd.children = append(nd.children, ch)
			rest = r
		}
	}
	return nd, b[off+l:], true
}

func (n *node) encode() []byte {
	body := n.content
	if n.children != nil {
		body = nil
		for _, c := range n.children {
			body = append(body, c.encode()...)
		}
	}
	if n.header != nil {
		return append(append([]byte(nil), n.header...), body...)
	}
	return append(append([]byte{n.tag}, lenBytes(len(body))...), body...)
}

func (n *node) clone() *node {
	c := &node{tag: n.tag, content: append([]byte(nil), n.content...), header: n.header}
	for _, ch := range n.children {
		c.children = append(c.children, ch.clone())
	}
	return c
}

func (n *node) body() []byte {
	if n.children == nil {
		return n.content
	}
	var b []byte
	for _, c := range n.children {
		b = append(b, c.encode()...)
	}
	return b
}

func (n *node) at(path []int) *node {
	cur := n
	for _, i := range path {
		cur = cur.children[i]
	}
	return cur
}

type mutation struct {
	name string
	f    func(n *node)
}

func leafMutations(n *node) []mutation {
	var ms []mutation
	content := n.content
	ms = append(ms,
		mutation{"tag+1", func(n *node) { n.tag++ }},
		mutation{"tag^20", func(n *node) { n.tag ^= 0x20; n.children = nil }},
		mutation{"long length", func(n *node) {
			l := len(n.body())
			n.header = []byte{n.tag, 0x83, byte(l >> 16), byte(l >> 8), byte(l)}
		}},
		mutation{"length+1", func(n *node) {
			n.header = append([]byte{n.tag}, lenBytes(len(n.body())+1)...)
		}},
		mutation{"empty", func(n *node) { n.content = nil; n.children = nil }},
	)
	if !isConstructed(n.tag) && len(content) > 0 {
		ms = append(ms,
			mutation{"drop last", func(n *node) { n.content = n.content[:len(n.content)-1] }},
			mutation{"zero prefix", func(n *node) { n.content = append([]byte{0}, n.content...) }},
			mutation{"ff prefix", func(n *node) { n.content = append([]byte{0xff}, n.content...) }},
			mutation{"80 prefix", func(n *node) { n.content = append([]byte{0x80}, n.content...) }},
			mutation{"high byte", func(n *node) { n.content = append(append([]byte(nil), n.content...), 0xc3) }},
			mutation{"control byte", func(n *node) { n.content = append(append([]byte(nil), n.content...), 0x01) }},
			mutation{"flip first", func(n *node) { n.content = append([]byte(nil), n.content...); n.content[0] ^= 0x01 }},
		)
	}
	switch n.tag {
	case 0x17, 0x18:
		for _, v := range []string{"2501010000Z", "250101000000Z", "500101000000Z", "491231235959Z", "490229000000Z", "250101000000+0100", "250101000000+0000", "250101000000", "251301000000Z", "250132000000Z", "250101240000Z", "250101006000Z", "250101000060Z",
			"20250101000000Z", "20250101000000.5Z", "20250101000000.50Z", "20500101000000Z", "20250101000000+0130", "2025010100Z", "20250229000000Z", "20240229000000Z", "99991231235959Z", "00010101000000Z"} {
			v := v
			ms = append(ms, mutation{"time " + v, func(n *node) { n.content = []byte(v) }})
		}
		ms = append(ms, mutation{"generalized", func(n *node) { n.tag = 0x18; n.content = []byte("20250101000000Z") }})
		ms = append(ms, mutation{"utc for 2050", func(n *node) { n.tag = 0x17; n.content = []byte("500101000000Z") }})
	case 0x13, 0x0c, 0x16, 0x14, 0x1e, 0x12:
		for _, tg := range []byte{0x13, 0x0c, 0x16, 0x14, 0x1e, 0x12, 0x1c, 0x1a} {
			tg := tg
			ms = append(ms, mutation{fmt.Sprintf("string tag %02x", tg), func(n *node) { n.tag = tg }})
		}
		for _, v := range []string{"a*b&c", "a@b", " lead", "trail ", "#hash", "a,b+c\"d\\e<f>g;h", "日本", "\x00\x41", "\xd8\x00", "\x00\x41\x00\x00", "12 34", ""} {
			v := v
			ms = append(ms, mutation{fmt.Sprintf("string %q", v), func(n *node) { n.content = []byte(v) }})
		}
	case 0x06:
		ms = append(ms,
			mutation{"oid huge arc", func(n *node) { n.content = append(append([]byte(nil), n.content...), 0x8f, 0xff, 0xff, 0xff, 0x7f) }},
			mutation{"oid 31 bit arc", func(n *node) { n.content = append(append([]byte(nil), n.content...), 0x87, 0xff, 0xff, 0xff, 0x7f) }},
			mutation{"oid truncated", func(n *node) { n.content = append(append([]byte(nil), n.content...), 0x81) }},
		)
	case 0x01:
		ms = append(ms, mutation{"bool 01", func(n *node) { n.content = []byte{1} }}, mutation{"bool 00", func(n *node) { n.content = []byte{0} }})
	case 0x03:
		ms = append(ms,
			mutation{"bits pad 1", func(n *node) { n.content = append([]byte(nil), n.content...); n.content[0] = 1 }},
			mutation{"bits pad 8", func(n *node) { n.content = append([]byte(nil), n.content...); n.content[0] = 8 }},
		)
	case 0x02:
		ms = append(ms,
			mutation{"int negative", func(n *node) { n.content = []byte{0xff} }},
			mutation{"int zero", func(n *node) { n.content = []byte{0} }},
			mutation{"int nine bytes", func(n *node) { n.content = []byte{0, 0x80, 1, 2, 3, 4, 5, 6, 7} }},
			mutation{"int huge", func(n *node) { n.content = bytes.Repeat([]byte{0x7f}, 40) }},
		)
	}
	return ms
}

func mutateAll(name string, der []byte) []parseCase {
	root, _, ok := parseTLV(der)
	if !ok {
		return nil
	}
	var out []parseCase
	type pn struct {
		path []int
		n    *node
	}
	var nodes []pn
	var rec func(path []int, n *node)
	rec = func(path []int, n *node) {
		nodes = append(nodes, pn{append([]int(nil), path...), n})
		for i, c := range n.children {
			rec(append(append([]int(nil), path...), i), c)
		}
	}
	rec(nil, root)
	for _, p := range nodes {
		for _, m := range leafMutations(p.n) {
			r := root.clone()
			m.f(r.at(p.path))
			out = append(out, parseFrom(fmt.Sprintf("%s %v %s", name, p.path, m.name), der, r.encode()))
		}
	}
	return out
}

// --- certificates made here.

type pki struct {
	t      *testing.T
	serial int64
}

func (p *pki) key(kind string) crypto.Signer {
	switch kind {
	case "rsa":
		k, err := rsa.GenerateKey(rand.Reader, 2048)
		if err != nil {
			p.t.Fatal(err)
		}
		return k
	case "p224", "p256", "p384", "p521":
		c := map[string]elliptic.Curve{"p224": elliptic.P224(), "p256": elliptic.P256(), "p384": elliptic.P384(), "p521": elliptic.P521()}[kind]
		k, err := ecdsa.GenerateKey(c, rand.Reader)
		if err != nil {
			p.t.Fatal(err)
		}
		return k
	case "ed25519":
		_, k, err := ed25519.GenerateKey(rand.Reader)
		if err != nil {
			p.t.Fatal(err)
		}
		return k
	}
	p.t.Fatalf("unknown key %s", kind)
	return nil
}

type cert struct {
	der  []byte
	c    *x509.Certificate
	key  crypto.Signer
	tmpl *x509.Certificate
}

func (p *pki) tmpl(cn string, ca bool) *x509.Certificate {
	p.serial++
	t := &x509.Certificate{
		SerialNumber:          big.NewInt(p.serial),
		Subject:               pkix.Name{CommonName: cn, Organization: []string{"shards.test"}},
		NotBefore:             t0.Add(-24 * time.Hour),
		NotAfter:              t0.Add(365 * 24 * time.Hour),
		BasicConstraintsValid: true,
		IsCA:                  ca,
	}
	if ca {
		t.KeyUsage = x509.KeyUsageCertSign | x509.KeyUsageCRLSign
		t.MaxPathLen = -1
	} else {
		t.KeyUsage = x509.KeyUsageDigitalSignature
		t.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning, x509.ExtKeyUsageServerAuth}
	}
	return t
}

func (p *pki) make(t *x509.Certificate, parent *cert, key crypto.Signer) *cert {
	pt, pk := t, key
	if parent != nil {
		pt, pk = parent.c, parent.key
	}
	der, err := x509.CreateCertificate(rand.Reader, t, pt, key.Public(), pk)
	if err != nil {
		p.t.Fatalf("%s: %v", t.Subject.CommonName, err)
	}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		p.t.Fatal(err)
	}
	return &cert{der: der, c: c, key: key, tmpl: t}
}

// resign: the certificate re-signed by `key` with the algorithm `ai` (its DER) over
// hash `h` (0 for none).
func (p *pki) resign(c *cert, key crypto.Signer, ai []byte, h crypto.Hash, opts crypto.SignerOpts) []byte {
	root, _, _ := parseTLV(c.der)
	tbs := root.children[0]
	aiNode, _, _ := parseTLV(ai)
	tbs.children[2] = aiNode
	root.children[1] = aiNode.clone()
	tbsDER := tbs.encode()
	digest := tbsDER
	if h != 0 {
		hh := h.New()
		hh.Write(tbsDER)
		digest = hh.Sum(nil)
	}
	if opts == nil {
		opts = h
	}
	sig, err := key.Sign(rand.Reader, digest, opts)
	if err != nil {
		p.t.Fatal(err)
	}
	root.children[2] = &node{tag: 0x03, content: append([]byte{0}, sig...)}
	return root.encode()
}

func ext(oid asn1.ObjectIdentifier, critical bool, value []byte) pkix.Extension {
	return pkix.Extension{Id: oid, Critical: critical, Value: value}
}

func mustDER(v any) []byte {
	b, err := asn1.Marshal(v)
	if err != nil {
		panic(err)
	}
	return b
}

func seq(parts ...[]byte) []byte {
	var body []byte
	for _, p := range parts {
		body = append(body, p...)
	}
	return append(append([]byte{0x30}, lenBytes(len(body))...), body...)
}

func tlv(tag byte, body []byte) []byte {
	return append(append([]byte{tag}, lenBytes(len(body))...), body...)
}

func oidDER(arcs ...int) []byte { return mustDER(asn1.ObjectIdentifier(arcs)) }

var (
	oidKU   = asn1.ObjectIdentifier{2, 5, 29, 15}
	oidBC   = asn1.ObjectIdentifier{2, 5, 29, 19}
	oidSAN  = asn1.ObjectIdentifier{2, 5, 29, 17}
	oidNC   = asn1.ObjectIdentifier{2, 5, 29, 30}
	oidCRL  = asn1.ObjectIdentifier{2, 5, 29, 31}
	oidAKI  = asn1.ObjectIdentifier{2, 5, 29, 35}
	oidPC   = asn1.ObjectIdentifier{2, 5, 29, 36}
	oidEKU  = asn1.ObjectIdentifier{2, 5, 29, 37}
	oidSKI  = asn1.ObjectIdentifier{2, 5, 29, 14}
	oidCP   = asn1.ObjectIdentifier{2, 5, 29, 32}
	oidPM   = asn1.ObjectIdentifier{2, 5, 29, 33}
	oidIAP  = asn1.ObjectIdentifier{2, 5, 29, 54}
	oidAIA  = asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 1}
	oidPolA = asn1.ObjectIdentifier{1, 2, 3, 4}
	oidPolB = asn1.ObjectIdentifier{1, 2, 3, 5}
	oidAny  = asn1.ObjectIdentifier{2, 5, 29, 32, 0}
)

func policiesDER(oids ...asn1.ObjectIdentifier) []byte {
	var parts [][]byte
	for _, o := range oids {
		parts = append(parts, seq(mustDER(o)))
	}
	return seq(parts...)
}

func mappingsDER(pairs ...[2]asn1.ObjectIdentifier) []byte {
	var parts [][]byte
	for _, p := range pairs {
		parts = append(parts, seq(mustDER(p[0]), mustDER(p[1])))
	}
	return seq(parts...)
}

func pcDER(require, inhibit int) []byte {
	var parts [][]byte
	if require >= 0 {
		parts = append(parts, tlv(0x80, big.NewInt(int64(require)).Bytes()))
		if require == 0 {
			parts[len(parts)-1] = []byte{0x80, 1, 0}
		}
	}
	if inhibit >= 0 {
		b := big.NewInt(int64(inhibit)).Bytes()
		if inhibit == 0 {
			b = []byte{0}
		}
		parts = append(parts, tlv(0x81, b))
	}
	return seq(parts...)
}

// --- the cases.

func extensionVariants() []struct {
	name string
	ext  pkix.Extension
} {
	type v = struct {
		name string
		ext  pkix.Extension
	}
	san := func(parts ...[]byte) []byte { return seq(parts...) }
	nc := func(perm, excl []byte) []byte {
		var parts [][]byte
		if perm != nil {
			parts = append(parts, tlv(0xa0, perm))
		}
		if excl != nil {
			parts = append(parts, tlv(0xa1, excl))
		}
		return seq(parts...)
	}
	sub := func(tag byte, val []byte) []byte { return seq(tlv(tag, val)) }
	return []v{
		{"ku bits", ext(oidKU, true, []byte{0x03, 0x02, 0x01, 0x86})},
		{"ku nine bits", ext(oidKU, true, []byte{0x03, 0x03, 0x07, 0x86, 0x80})},
		{"ku bad padding", ext(oidKU, true, []byte{0x03, 0x02, 0x01, 0x87})},
		{"ku not bitstring", ext(oidKU, true, []byte{0x04, 0x01, 0x00})},
		{"bc ca pathlen 3", ext(oidBC, true, seq([]byte{1, 1, 0xff}, []byte{2, 1, 3}))},
		{"bc pathlen only", ext(oidBC, true, seq([]byte{2, 1, 0}))},
		{"bc negative pathlen", ext(oidBC, true, seq([]byte{1, 1, 0xff}, []byte{2, 1, 0xff}))},
		{"bc huge pathlen", ext(oidBC, true, seq([]byte{1, 1, 0xff}, []byte{2, 9, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff}))},
		{"bc bool 01", ext(oidBC, true, seq([]byte{1, 1, 1}))},
		{"bc empty", ext(oidBC, true, seq())},
		{"bc trailing", ext(oidBC, true, append(seq(), 0))},
		{"san dns email uri ip", ext(oidSAN, false, san(tlv(0x82, []byte("a.example.com")), tlv(0x81, []byte("x@Example.com")), tlv(0x86, []byte("https://h.example.com:8443/p?q#f")), tlv(0x87, []byte{10, 1, 2, 3}), tlv(0x87, net.ParseIP("2001:db8::1")), tlv(0x87, net.ParseIP("::ffff:1.2.3.4"))))},
		{"san ip len 5", ext(oidSAN, false, san(tlv(0x87, []byte{1, 2, 3, 4, 5})))},
		{"san uri bad", ext(oidSAN, false, san(tlv(0x86, []byte("http://[::1"))))},
		{"san uri bad domain", ext(oidSAN, false, san(tlv(0x86, []byte("https://a..b/"))))},
		{"san uri trailing dot", ext(oidSAN, false, san(tlv(0x86, []byte("https://a.b./"))))},
		{"san uri no host", ext(oidSAN, false, san(tlv(0x86, []byte("mailto:x@y"))))},
		{"san dns non ascii", ext(oidSAN, false, san(tlv(0x82, []byte("é.com"))))},
		{"san email non ascii", ext(oidSAN, false, san(tlv(0x81, []byte("é@x.com"))))},
		{"san only othername critical", ext(oidSAN, true, san(tlv(0xa0, append(oidDER(1, 3, 6, 1, 4, 1, 57264, 1, 7), tlv(0xa0, tlv(0x0c, []byte("me")))...))))},
		{"san constructed dns", ext(oidSAN, false, san(tlv(0xa2, []byte{})))},
		{"san empty seq", ext(oidSAN, false, san())},
		{"san not seq", ext(oidSAN, false, []byte{0x04, 0})},
		{"nc dns", ext(oidNC, true, nc(sub(0x82, []byte("example.com")), sub(0x82, []byte(".evil.com"))))},
		{"nc ip", ext(oidNC, true, nc(sub(0x87, []byte{10, 0, 0, 0, 255, 0, 0, 0}), sub(0x87, append(net.ParseIP("2001:db8::"), bytes.Repeat([]byte{0xff}, 4)...))))},
		{"nc ip bad len", ext(oidNC, true, nc(sub(0x87, []byte{10, 0, 0, 0}), nil))},
		{"nc ip bad mask", ext(oidNC, true, nc(sub(0x87, []byte{10, 0, 0, 0, 255, 0, 255, 0}), nil))},
		{"nc email", ext(oidNC, true, nc(sub(0x81, []byte("a@example.com")), sub(0x81, []byte(".example.org"))))},
		{"nc email bad", ext(oidNC, true, nc(sub(0x81, []byte("a@@b")), nil))},
		{"nc uri", ext(oidNC, true, nc(sub(0x86, []byte(".example.com")), nil))},
		{"nc uri ip", ext(oidNC, true, nc(sub(0x86, []byte("10.0.0.1")), nil))},
		{"nc dns bad", ext(oidNC, true, nc(sub(0x82, []byte("a..b")), nil))},
		{"nc dns trailing dot", ext(oidNC, true, nc(sub(0x82, []byte("a.b.")), nil))},
		{"nc dns non ascii", ext(oidNC, true, nc(sub(0x82, []byte("é")), nil))},
		{"nc empty", ext(oidNC, true, seq())},
		{"nc empty lists", ext(oidNC, true, nc([]byte{}, []byte{}))},
		{"nc unknown kind", ext(oidNC, true, nc(sub(0xa4, seq()), nil))},
		{"nc trailing", ext(oidNC, true, append(nc(sub(0x82, []byte("x.com")), nil), 0))},
		{"crl dp", ext(oidCRL, false, seq(seq(tlv(0xa0, tlv(0xa0, tlv(0x86, []byte("http://crl")))))))},
		{"crl dp no name", ext(oidCRL, false, seq(seq()))},
		{"crl dp bad", ext(oidCRL, false, seq(tlv(0x04, nil)))},
		{"aki critical", ext(oidAKI, true, seq(tlv(0x80, []byte{1, 2})))},
		{"aki no id", ext(oidAKI, false, seq(tlv(0x82, []byte{1})))},
		{"aki bad", ext(oidAKI, false, []byte{0x04, 0})},
		{"pc both", ext(oidPC, true, pcDER(2, 1))},
		{"pc zero", ext(oidPC, true, pcDER(0, 0))},
		{"pc negative", ext(oidPC, true, seq([]byte{0x80, 1, 0xff}))},
		{"pc bad", ext(oidPC, true, seq([]byte{0x80, 0}))},
		{"eku unknown", ext(oidEKU, false, seq(oidDER(1, 3, 6, 1, 5, 5, 7, 3, 3), oidDER(1, 2, 3, 99), oidDER(2, 5, 29, 37, 0)))},
		{"eku all known", ext(oidEKU, false, seq(oidDER(1, 3, 6, 1, 5, 5, 7, 3, 1), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 2), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 4), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 5), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 6), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 7), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 8), oidDER(1, 3, 6, 1, 5, 5, 7, 3, 9), oidDER(1, 3, 6, 1, 4, 1, 311, 10, 3, 3), oidDER(2, 16, 840, 1, 113730, 4, 1), oidDER(1, 3, 6, 1, 4, 1, 311, 2, 1, 22), oidDER(1, 3, 6, 1, 4, 1, 311, 61, 1, 1)))},
		{"eku bad", ext(oidEKU, false, seq([]byte{0x02, 1, 1}))},
		{"ski critical", ext(oidSKI, true, tlv(0x04, []byte{1}))},
		{"ski bad", ext(oidSKI, false, []byte{0x02, 1, 1})},
		{"policies", ext(oidCP, false, policiesDER(oidPolA, oidAny))},
		{"policies dup", ext(oidCP, false, policiesDER(oidPolA, oidPolA))},
		{"policies bad oid", ext(oidCP, false, seq(seq([]byte{0x06, 1, 0x80})))},
		{"policies with qualifiers", ext(oidCP, false, seq(seq(mustDER(oidPolA), seq(seq(oidDER(1, 3, 6, 1, 5, 5, 7, 2, 1), tlv(0x16, []byte("http://cps")))))))},
		{"mappings", ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{oidPolA, oidPolB}))},
		{"mappings bad", ext(oidPM, true, seq(seq(mustDER(oidPolA))))},
		{"inhibit any", ext(oidIAP, true, []byte{0x02, 1, 2})},
		{"inhibit any zero", ext(oidIAP, true, []byte{0x02, 1, 0})},
		{"inhibit any big", ext(oidIAP, true, []byte{0x02, 9, 1, 0, 0, 0, 0, 0, 0, 0, 0})},
		{"aia", ext(oidAIA, false, seq(seq(oidDER(1, 3, 6, 1, 5, 5, 7, 48, 1), tlv(0x86, []byte("http://ocsp"))), seq(oidDER(1, 3, 6, 1, 5, 5, 7, 48, 2), tlv(0x82, []byte("x")))))},
		{"aia critical", ext(oidAIA, true, seq())},
		{"aia bad", ext(oidAIA, false, seq(seq([]byte{0x02, 1, 1})))},
		{"unknown critical", ext(asn1.ObjectIdentifier{1, 2, 3, 4, 5}, true, []byte{5, 0})},
		{"unknown 2.5.29 critical", ext(asn1.ObjectIdentifier{2, 5, 29, 99}, true, []byte{5, 0})},
		{"unknown 2.5.29 long arc critical", ext(asn1.ObjectIdentifier{2, 5, 29, 300}, true, []byte{5, 0})},
		{"unknown noncritical", ext(asn1.ObjectIdentifier{1, 2, 3, 4, 5}, false, []byte{5, 0})},
	}
}

func TestShardsX509Oracle(t *testing.T) {
	cryptotest.SetGlobalRandom(t, 105)
	p := &pki{t: t}
	var o oracle
	root := os.Getenv("SHARDS_ROOT")

	// Real certificates: Sigstore's CAs and TSA, and the buildkit leaf.
	var tr struct {
		CAs []struct {
			CertChain struct {
				Certificates []struct {
					RawBytes []byte `json:"rawBytes"`
				} `json:"certificates"`
			} `json:"certChain"`
		} `json:"certificateAuthorities"`
		TSAs []struct {
			CertChain struct {
				Certificates []struct {
					RawBytes []byte `json:"rawBytes"`
				} `json:"certificates"`
			} `json:"certChain"`
		} `json:"timestampAuthorities"`
	}
	dt, err := os.ReadFile(filepath.Join(root, "crates/tuf/roots/sigstore/targets/trusted_root.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(dt, &tr); err != nil {
		t.Fatal(err)
	}
	var real [][]byte
	for _, ca := range append(tr.CAs, tr.TSAs...) {
		for _, c := range ca.CertChain.Certificates {
			real = append(real, c.RawBytes)
		}
	}
	var b struct {
		VM struct {
			Certificate struct {
				RawBytes []byte `json:"rawBytes"`
			} `json:"certificate"`
		} `json:"verificationMaterial"`
	}
	dt, err = os.ReadFile(filepath.Join(root, "crates/sigstore/testdata/real/buildkit-v0.28.1-arm64.bundle.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(dt, &b); err != nil {
		t.Fatal(err)
	}
	real = append(real, b.VM.Certificate.RawBytes)
	for i, r := range real {
		o.Parse = append(o.Parse, parseOne(fmt.Sprintf("real %d", i), r))
		o.Parse = append(o.Parse, parseFrom(fmt.Sprintf("real %d trailing", i), r, append(append([]byte(nil), r...), 0)))
	}
	// Every DER position of the buildkit leaf and one CA, mutated.
	o.Parse = append(o.Parse, mutateAll("leaf", b.VM.Certificate.RawBytes)...)
	o.Parse = append(o.Parse, mutateAll("ca", real[len(real)-2])...)

	// Generated certificates, one per extension variant.
	caKey := p.key("p256")
	ca := p.make(p.tmpl("Variant CA", true), nil, caKey)
	for _, v := range extensionVariants() {
		tm := p.tmpl("variant "+v.name, false)
		tm.ExtraExtensions = []pkix.Extension{v.ext}
		// Templates fill some extensions themselves; leave them off where varied.
		switch {
		case v.ext.Id.Equal(oidKU):
			tm.KeyUsage = 0
		case v.ext.Id.Equal(oidBC):
			tm.BasicConstraintsValid = false
		case v.ext.Id.Equal(oidEKU):
			tm.ExtKeyUsage = nil
		case v.ext.Id.Equal(oidAKI):
			tm.AuthorityKeyId = nil
		}
		k := p.key("p256")
		der, err := x509.CreateCertificate(rand.Reader, tm, ca.c, k.Public(), caKey)
		if err != nil {
			t.Fatalf("%s: %v", v.name, err)
		}
		o.Parse = append(o.Parse, parseOne("ext "+v.name, der))
	}
	// Duplicate extensions.
	{
		tm := p.tmpl("dup", false)
		tm.ExtraExtensions = []pkix.Extension{ext(asn1.ObjectIdentifier{1, 2, 3}, false, []byte{5, 0}), ext(asn1.ObjectIdentifier{1, 2, 3}, false, []byte{5, 0})}
		k := p.key("p256")
		der, err := x509.CreateCertificate(rand.Reader, tm, ca.c, k.Public(), caKey)
		if err != nil {
			t.Fatal(err)
		}
		o.Parse = append(o.Parse, parseOne("ext duplicate", der))
	}
	// Each key type, and names of every kind.
	for _, kind := range []string{"rsa", "p224", "p256", "p384", "p521", "ed25519"} {
		tm := p.tmpl("key "+kind, false)
		tm.Subject = pkix.Name{
			Country: []string{"US", "DE"}, Province: []string{"P"}, Locality: []string{"L"}, StreetAddress: []string{"S"},
			PostalCode: []string{"1"}, Organization: []string{"O, Inc."}, OrganizationalUnit: []string{"#ou "}, CommonName: " cn+x",
			SerialNumber: "42", ExtraNames: nil,
			Names: nil,
		}
		tm.Subject.ExtraNames = []pkix.AttributeTypeAndValue{{Type: asn1.ObjectIdentifier{0, 9, 2342, 19200300, 100, 1, 25}, Value: "dc"}, {Type: asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 9, 1}, Value: "é@x"}}
		k := p.key(kind)
		der, err := x509.CreateCertificate(rand.Reader, tm, ca.c, k.Public(), caKey)
		if err != nil {
			t.Fatal(err)
		}
		o.Parse = append(o.Parse, parseOne("key "+kind, der))
		if kind == "p384" {
			o.Parse = append(o.Parse, mutateAll("names", der)...)
		}
	}

	// --- chains.
	idx := func(all [][]byte, c *x509.Certificate) int {
		for i, r := range all {
			if bytes.Equal(r, c.Raw) {
				return i
			}
		}
		return -1
	}
	verify := func(name string, leaf []byte, roots, inters [][]byte, now time.Time, usages []x509.ExtKeyUsage) {
		var all [][]byte
		vc := verifyCase{Name: name, Now: now.Unix(), Roots: []int{}, Inters: []int{}, Usages: []int{}, Chains: [][]int{}}
		add := func(d []byte) int {
			all = append(all, d)
			vc.Certs = append(vc.Certs, entry(d))
			return len(all) - 1
		}
		vc.Leaf = add(leaf)
		rp, ip := x509.NewCertPool(), x509.NewCertPool()
		for _, r := range roots {
			vc.Roots = append(vc.Roots, add(r))
			c, err := x509.ParseCertificate(r)
			if err != nil {
				t.Fatalf("%s: %v", name, err)
			}
			rp.AddCert(c)
		}
		for _, r := range inters {
			vc.Inters = append(vc.Inters, add(r))
			c, err := x509.ParseCertificate(r)
			if err != nil {
				t.Fatalf("%s: %v", name, err)
			}
			ip.AddCert(c)
		}
		for _, u := range usages {
			vc.Usages = append(vc.Usages, int(u))
		}
		lc, err := x509.ParseCertificate(leaf)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		chains, err := lc.Verify(x509.VerifyOptions{Roots: rp, Intermediates: ip, CurrentTime: now, KeyUsages: usages})
		if err != nil {
			vc.Error = err.Error()
		}
		for _, ch := range chains {
			var ix []int
			for _, c := range ch {
				ix = append(ix, idx(all, c))
			}
			vc.Chains = append(vc.Chains, ix)
		}
		o.Verify = append(o.Verify, vc)
	}
	code := []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning}

	// A plain chain, by key usage and time.
	rk := p.key("p256")
	rootC := p.make(p.tmpl("Root", true), nil, rk)
	ik := p.key("p256")
	inter := p.make(p.tmpl("Inter", true), rootC, ik)
	lk := p.key("p256")
	leaf := p.make(p.tmpl("Leaf", false), inter, lk)
	verify("plain code signing", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, code)
	verify("plain default server auth", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, nil)
	verify("plain any", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, []x509.ExtKeyUsage{x509.ExtKeyUsageAny})
	verify("plain timestamping", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, []x509.ExtKeyUsage{x509.ExtKeyUsageTimeStamping})
	verify("plain two usages", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, []x509.ExtKeyUsage{x509.ExtKeyUsageTimeStamping, x509.ExtKeyUsageCodeSigning})
	verify("no intermediate", leaf.der, [][]byte{rootC.der}, nil, t0, code)
	verify("unknown authority", leaf.der, [][]byte{p.make(p.tmpl("Other", true), nil, p.key("p256")).der}, [][]byte{inter.der}, t0, code)
	verify("leaf expired", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0.Add(400*24*time.Hour), code)
	verify("leaf not yet valid", leaf.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0.Add(-48*time.Hour), code)
	verify("leaf is root", leaf.der, [][]byte{leaf.der}, nil, t0, code)
	{
		tm := p.tmpl("Inter Old", true)
		tm.NotAfter = t0.Add(-time.Hour)
		old := p.make(tm, rootC, ik)
		verify("intermediate expired", leaf.der, [][]byte{rootC.der}, [][]byte{old.der}, t0, code)
		verify("intermediate expired and valid", leaf.der, [][]byte{rootC.der}, [][]byte{old.der, inter.der}, t0, code)
	}
	{
		tm := p.tmpl("Inter", true)
		tm.IsCA = false
		tm.KeyUsage = x509.KeyUsageCertSign
		notCA := p.make(tm, rootC, ik)
		verify("intermediate not a CA", leaf.der, [][]byte{rootC.der}, [][]byte{notCA.der}, t0, code)
		tm2 := p.tmpl("Inter", true)
		tm2.KeyUsage = x509.KeyUsageDigitalSignature
		noSign := p.make(tm2, rootC, ik)
		verify("intermediate without cert sign", leaf.der, [][]byte{rootC.der}, [][]byte{noSign.der}, t0, code)
	}
	{
		tm := p.tmpl("Root", true)
		tm.MaxPathLen = 0
		tm.MaxPathLenZero = true
		r0 := p.make(tm, nil, rk)
		verify("root path length zero", leaf.der, [][]byte{r0.der}, [][]byte{inter.der}, t0, code)
		tm1 := p.tmpl("Root", true)
		tm1.MaxPathLen = 1
		r1 := p.make(tm1, nil, rk)
		verify("root path length one", leaf.der, [][]byte{r1.der}, [][]byte{inter.der}, t0, code)
	}
	{
		// EKUs on the intermediate restricting the leaf.
		for _, set := range [][]x509.ExtKeyUsage{{x509.ExtKeyUsageTimeStamping}, {x509.ExtKeyUsageAny}, {x509.ExtKeyUsageCodeSigning}} {
			tm := p.tmpl("Inter", true)
			tm.ExtKeyUsage = set
			ei := p.make(tm, rootC, ik)
			verify(fmt.Sprintf("intermediate eku %v", set), leaf.der, [][]byte{rootC.der}, [][]byte{ei.der}, t0, code)
		}
		tm := p.tmpl("Inter", true)
		tm.UnknownExtKeyUsage = []asn1.ObjectIdentifier{{1, 2, 3, 4}}
		ui := p.make(tm, rootC, ik)
		verify("intermediate unknown eku", leaf.der, [][]byte{rootC.der}, [][]byte{ui.der}, t0, code)
		tl := p.tmpl("Leaf", false)
		tl.ExtKeyUsage = nil
		tl.UnknownExtKeyUsage = []asn1.ObjectIdentifier{{1, 2, 3, 4}}
		ul := p.make(tl, inter, lk)
		verify("leaf unknown eku", ul.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, code)
		tn := p.tmpl("Leaf", false)
		tn.ExtKeyUsage = nil
		nl := p.make(tn, inter, lk)
		verify("leaf without eku", nl.der, [][]byte{rootC.der}, [][]byte{inter.der}, t0, code)
	}
	{
		// Candidates: two intermediates of one subject, the first with another key.
		other := p.make(p.tmpl("Inter", true), rootC, p.key("p256"))
		verify("candidates other key first", leaf.der, [][]byte{rootC.der}, [][]byte{other.der, inter.der}, t0, code)
		verify("candidates other key only", leaf.der, [][]byte{rootC.der}, [][]byte{other.der}, t0, code)
		// Cross-signed: the intermediate also by a second root.
		rk2 := p.key("p256")
		root2 := p.make(p.tmpl("Root2", true), nil, rk2)
		cross := p.make(p.tmpl("Inter", true), root2, ik)
		verify("two chains", leaf.der, [][]byte{rootC.der, root2.der}, [][]byte{inter.der, cross.der}, t0, code)
		// The same certificate given twice, and the leaf in the intermediates.
		verify("duplicates", leaf.der, [][]byte{rootC.der, rootC.der}, [][]byte{inter.der, inter.der, leaf.der}, t0, code)
		// No AKI/SKI.
		tm := p.tmpl("Inter NoKID", true)
		tm.SubjectKeyId = []byte{}
		nk := p.make(tm, rootC, ik)
		tl := p.tmpl("Leaf NoKID", false)
		nl := p.make(tl, nk, lk)
		verify("no key ids", nl.der, [][]byte{rootC.der}, [][]byte{nk.der}, t0, code)
	}

	// Signature algorithms.
	for _, kind := range []string{"rsa", "p224", "p256", "p384", "p521", "ed25519"} {
		k := p.key(kind)
		r := p.make(p.tmpl("Alg Root "+kind, true), nil, k)
		l := p.make(p.tmpl("Alg Leaf "+kind, false), r, p.key("p256"))
		verify("alg "+kind, l.der, [][]byte{r.der}, nil, t0, code)
		if kind == "rsa" {
			for _, alg := range []x509.SignatureAlgorithm{x509.SHA384WithRSA, x509.SHA512WithRSA, x509.SHA256WithRSAPSS, x509.SHA384WithRSAPSS, x509.SHA512WithRSAPSS} {
				tm := p.tmpl("Alg Leaf "+alg.String(), false)
				tm.SignatureAlgorithm = alg
				l := p.make(tm, r, p.key("p256"))
				verify("alg "+alg.String(), l.der, [][]byte{r.der}, nil, t0, code)
			}
			// SHA-1 and MD5, signed by hand.
			sha1AI := seq(oidDER(1, 2, 840, 113549, 1, 1, 5), []byte{5, 0})
			md5AI := seq(oidDER(1, 2, 840, 113549, 1, 1, 4), []byte{5, 0})
			verify("alg sha1 rsa", p.resign(l, k, sha1AI, crypto.SHA1, nil), [][]byte{r.der}, nil, t0, code)
			verify("alg md5 rsa", p.resign(l, k, md5AI, crypto.MD5, nil), [][]byte{r.der}, nil, t0, code)
			wrong := p.resign(l, p.key("rsa"), seq(oidDER(1, 2, 840, 113549, 1, 1, 11), []byte{5, 0}), crypto.SHA256, nil)
			verify("alg rsa wrong key", wrong, [][]byte{r.der}, nil, t0, code)
		}
		if kind == "p256" {
			ecSHA1 := seq(oidDER(1, 2, 840, 10045, 4, 1))
			verify("alg ecdsa sha1", p.resign(l, k, ecSHA1, crypto.SHA1, nil), [][]byte{r.der}, nil, t0, code)
			ecSHA512 := seq(oidDER(1, 2, 840, 10045, 4, 3, 4))
			verify("alg ecdsa sha512 on p256", p.resign(l, k, ecSHA512, crypto.SHA512, nil), [][]byte{r.der}, nil, t0, code)
			rsaAI := seq(oidDER(1, 2, 840, 113549, 1, 1, 11), []byte{5, 0})
			verify("alg rsa ai over ecdsa key", p.resign(l, k, rsaAI, crypto.SHA256, nil), [][]byte{r.der}, nil, t0, code)
			unknownAI := seq(oidDER(1, 2, 3, 4))
			verify("alg unknown", p.resign(l, k, unknownAI, crypto.SHA256, nil), [][]byte{r.der}, nil, t0, code)
		}
	}
	_ = md5.New
	_ = sha1.New
	_ = sha256.New

	// Name constraints.
	type ncSpec struct {
		name  string
		apply func(t *x509.Certificate)
	}
	ncs := []ncSpec{
		{"permit dns example.com", func(t *x509.Certificate) { t.PermittedDNSDomains = []string{"example.com"} }},
		{"permit dns .example.com", func(t *x509.Certificate) { t.PermittedDNSDomains = []string{".example.com"} }},
		{"permit dns EXAMPLE.com", func(t *x509.Certificate) {
			t.PermittedDNSDomains = []string{"EXAMPLE.com", "other.org", "a.example.com"}
		}},
		{"exclude dns example.com", func(t *x509.Certificate) { t.ExcludedDNSDomains = []string{"example.com"} }},
		{"exclude dns a.example.com", func(t *x509.Certificate) { t.ExcludedDNSDomains = []string{"a.example.com", "b.example.com"} }},
		{"permit dns empty", func(t *x509.Certificate) { t.ExcludedDNSDomains = []string{""} }},
		{"permit ip 10/8", func(t *x509.Certificate) {
			t.PermittedIPRanges = []*net.IPNet{{IP: net.IP{10, 0, 0, 0}, Mask: net.CIDRMask(8, 32)}}
		}},
		{"exclude ip 10.1/16", func(t *x509.Certificate) {
			t.ExcludedIPRanges = []*net.IPNet{{IP: net.IP{10, 1, 0, 0}, Mask: net.CIDRMask(16, 32)}, {IP: net.ParseIP("2001:db8::"), Mask: net.CIDRMask(32, 128)}}
		}},
		{"permit ip v6", func(t *x509.Certificate) {
			t.PermittedIPRanges = []*net.IPNet{{IP: net.ParseIP("2001:db8::"), Mask: net.CIDRMask(32, 128)}}
		}},
		{"permit email domain", func(t *x509.Certificate) { t.PermittedEmailAddresses = []string{"example.com"} }},
		{"permit email .domain", func(t *x509.Certificate) { t.PermittedEmailAddresses = []string{".example.com"} }},
		{"permit email mailbox", func(t *x509.Certificate) { t.PermittedEmailAddresses = []string{"x@EXAMPLE.com"} }},
		{"exclude email", func(t *x509.Certificate) { t.ExcludedEmailAddresses = []string{"x@example.com", "evil.org"} }},
		{"permit uri", func(t *x509.Certificate) { t.PermittedURIDomains = []string{".example.com"} }},
		{"exclude uri", func(t *x509.Certificate) { t.ExcludedURIDomains = []string{"h.example.com"} }},
	}
	sans := []struct {
		name  string
		apply func(t *x509.Certificate)
	}{
		{"dns a.example.com", func(t *x509.Certificate) { t.DNSNames = []string{"a.example.com"} }},
		{"dns example.com", func(t *x509.Certificate) { t.DNSNames = []string{"example.com"} }},
		{"dns A.EXAMPLE.COM", func(t *x509.Certificate) { t.DNSNames = []string{"A.EXAMPLE.COM"} }},
		{"dns evil.com", func(t *x509.Certificate) { t.DNSNames = []string{"evil.com"} }},
		{"dns wildcard", func(t *x509.Certificate) { t.DNSNames = []string{"*.example.com"} }},
		{"dns notexample.com", func(t *x509.Certificate) { t.DNSNames = []string{"notexample.com"} }},
		{"ip 10.1.2.3", func(t *x509.Certificate) { t.IPAddresses = []net.IP{{10, 1, 2, 3}} }},
		{"ip 11.0.0.1", func(t *x509.Certificate) { t.IPAddresses = []net.IP{{11, 0, 0, 1}} }},
		{"ip v6", func(t *x509.Certificate) { t.IPAddresses = []net.IP{net.ParseIP("2001:db8::5")} }},
		{"ip v4 mapped", func(t *x509.Certificate) { t.IPAddresses = []net.IP{net.ParseIP("::ffff:10.1.2.3")} }},
		{"email x@example.com", func(t *x509.Certificate) { t.EmailAddresses = []string{"x@example.com"} }},
		{"email y@a.example.com", func(t *x509.Certificate) { t.EmailAddresses = []string{"y@a.example.com"} }},
		{"email x@EXAMPLE.com", func(t *x509.Certificate) { t.EmailAddresses = []string{"x@EXAMPLE.com"} }},
		{"email bad", func(t *x509.Certificate) { t.EmailAddresses = []string{"no-at-sign"} }},
		{"email quoted", func(t *x509.Certificate) { t.EmailAddresses = []string{"\"q x\"@example.com"} }},
		{"uri h.example.com", func(t *x509.Certificate) { t.URIs = []*url.URL{{Scheme: "https", Host: "h.example.com", Path: "/w"}} }},
		{"uri port", func(t *x509.Certificate) { t.URIs = []*url.URL{{Scheme: "https", Host: "H.example.com:8443"}} }},
		{"uri ip", func(t *x509.Certificate) { t.URIs = []*url.URL{{Scheme: "https", Host: "10.0.0.1"}} }},
		{"uri v6", func(t *x509.Certificate) { t.URIs = []*url.URL{{Scheme: "https", Host: "[::1]:80"}} }},
		{"uri empty host", func(t *x509.Certificate) { t.URIs = []*url.URL{{Scheme: "spiffe", Opaque: "x"}} }},
		{"no san", func(t *x509.Certificate) {}},
	}
	for _, nc := range ncs {
		for _, where := range []string{"inter", "root"} {
			rt := p.tmpl("NC Root", true)
			it := p.tmpl("NC Inter", true)
			if where == "root" {
				nc.apply(rt)
			} else {
				nc.apply(it)
			}
			r := p.make(rt, nil, rk)
			in := p.make(it, r, ik)
			for _, s := range sans {
				lt := p.tmpl("NC Leaf", false)
				s.apply(lt)
				l := p.make(lt, in, lk)
				verify(fmt.Sprintf("nc %s on %s, %s", nc.name, where, s.name), l.der, [][]byte{r.der}, [][]byte{in.der}, t0, code)
			}
		}
	}
	{
		// The intermediate's own SAN against the root's constraints, and two levels.
		rt := p.tmpl("NC Root", true)
		rt.PermittedDNSDomains = []string{"example.com"}
		r := p.make(rt, nil, rk)
		it := p.tmpl("NC Inter", true)
		it.DNSNames = []string{"evil.com"}
		in := p.make(it, r, ik)
		l := p.make(p.tmpl("NC Leaf", false), in, lk)
		verify("nc intermediate san violates root", l.der, [][]byte{r.der}, [][]byte{in.der}, t0, code)
		it2 := p.tmpl("NC Inter", true)
		it2.ExcludedDNSDomains = []string{"b.example.com"}
		in2 := p.make(it2, r, ik)
		for _, d := range []string{"a.example.com", "b.example.com", "x.evil.com"} {
			lt := p.tmpl("NC Leaf", false)
			lt.DNSNames = []string{d}
			l := p.make(lt, in2, lk)
			verify("nc two levels "+d, l.der, [][]byte{r.der}, [][]byte{in2.der}, t0, code)
		}
		// The constraint with a key usage problem too: the hint wins.
		lt := p.tmpl("NC Leaf", false)
		lt.DNSNames = []string{"evil.com"}
		lt.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}
		l2 := p.make(lt, in2, lk)
		verify("nc and incompatible usage", l2.der, [][]byte{r.der}, [][]byte{in2.der}, t0, code)
	}

	// Policies.
	type polSpec struct {
		name string
		root []pkix.Extension
		in   []pkix.Extension
		leaf []pkix.Extension
	}
	cp := func(o ...asn1.ObjectIdentifier) pkix.Extension { return ext(oidCP, false, policiesDER(o...)) }
	pols := []polSpec{
		{"same policy", nil, []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA)}},
		{"leaf without policy", nil, []pkix.Extension{cp(oidPolA)}, nil},
		{"require explicit 0, leaf has", nil, []pkix.Extension{cp(oidPolA), ext(oidPC, true, pcDER(0, -1))}, []pkix.Extension{cp(oidPolA)}},
		{"require explicit 0, leaf lacks", nil, []pkix.Extension{cp(oidPolA), ext(oidPC, true, pcDER(0, -1))}, nil},
		{"require explicit 0, other policy", nil, []pkix.Extension{cp(oidPolA), ext(oidPC, true, pcDER(0, -1))}, []pkix.Extension{cp(oidPolB)}},
		{"require explicit 0, any", nil, []pkix.Extension{cp(oidAny), ext(oidPC, true, pcDER(0, -1))}, []pkix.Extension{cp(oidPolB)}},
		{"require explicit 1", []pkix.Extension{ext(oidPC, true, pcDER(1, -1))}, []pkix.Extension{cp(oidPolA)}, nil},
		{"require explicit on root", []pkix.Extension{ext(oidPC, true, pcDER(0, -1))}, nil, nil},
		{"inhibit any 0", nil, []pkix.Extension{cp(oidAny), ext(oidPC, true, pcDER(0, -1)), ext(oidIAP, true, []byte{2, 1, 0})}, []pkix.Extension{cp(oidPolA)}},
		{"inhibit any 0 leaf any", nil, []pkix.Extension{cp(oidAny), ext(oidPC, true, pcDER(0, -1)), ext(oidIAP, true, []byte{2, 1, 0})}, []pkix.Extension{cp(oidAny)}},
		{"mapping a to b", nil, []pkix.Extension{cp(oidPolA), ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{oidPolA, oidPolB})), ext(oidPC, true, pcDER(0, -1))}, []pkix.Extension{cp(oidPolB)}},
		{"mapping to any", nil, []pkix.Extension{cp(oidPolA), ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{oidPolA, oidAny}))}, []pkix.Extension{cp(oidPolA)}},
		{"inhibit mapping 0", nil, []pkix.Extension{cp(oidPolA), ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{oidPolA, oidPolB})), ext(oidPC, true, pcDER(0, 0))}, []pkix.Extension{cp(oidPolB)}},
		{"inhibit mapping 0 keep a", nil, []pkix.Extension{cp(oidPolA, oidPolB), ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{oidPolA, oidPolB})), ext(oidPC, true, pcDER(0, 0))}, []pkix.Extension{cp(oidPolB)}},
		{"any everywhere", []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidAny)}},
		{"policy on root only", []pkix.Extension{cp(oidPolA), ext(oidPC, true, pcDER(0, -1))}, nil, []pkix.Extension{cp(oidPolA)}},
	}
	for _, ps := range pols {
		rt := p.tmpl("Pol Root", true)
		rt.ExtraExtensions = ps.root
		r := p.make(rt, nil, rk)
		it := p.tmpl("Pol Inter", true)
		it.ExtraExtensions = ps.in
		in := p.make(it, r, ik)
		lt := p.tmpl("Pol Leaf", false)
		lt.ExtraExtensions = ps.leaf
		l := p.make(lt, in, lk)
		verify("policy "+ps.name, l.der, [][]byte{r.der}, [][]byte{in.der}, t0, code)
		// With two intermediates: a self-issued one in the middle.
		it2 := p.tmpl("Pol Inter", true)
		it2.ExtraExtensions = ps.in
		in2 := p.make(it2, in, ik)
		l2 := p.make(lt, in2, lk)
		verify("policy deep "+ps.name, l2.der, [][]byte{r.der}, [][]byte{in.der, in2.der}, t0, code)
		// With a policy problem and an incompatible usage.
		verify("policy with timestamping "+ps.name, l.der, [][]byte{r.der}, [][]byte{in.der}, t0, []x509.ExtKeyUsage{x509.ExtKeyUsageTimeStamping})
	}

	// Policies over three intermediates, each with its own extensions.
	pc := func(require, inhibit int) pkix.Extension { return ext(oidPC, true, pcDER(require, inhibit)) }
	pm := func(a, b asn1.ObjectIdentifier) pkix.Extension {
		return ext(oidPM, true, mappingsDER([2]asn1.ObjectIdentifier{a, b}))
	}
	iap := func(n byte) pkix.Extension { return ext(oidIAP, true, []byte{2, 1, n}) }
	type deepSpec struct {
		name                   string
		top, mid, bottom, leaf []pkix.Extension
	}
	deeps := []deepSpec{
		{"explicit 1 at top, leaf lacks", []pkix.Extension{cp(oidPolA), pc(1, -1)}, []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA)}, nil},
		{"explicit 2 at top, leaf lacks", []pkix.Extension{cp(oidPolA), pc(2, -1)}, []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA)}, nil},
		{"explicit 2 at top, bottom lacks", []pkix.Extension{cp(oidPolA), pc(2, -1)}, []pkix.Extension{cp(oidPolA)}, nil, nil},
		{"explicit 3 at top, leaf lacks", []pkix.Extension{cp(oidPolA), pc(3, -1)}, []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA)}, nil},
		{"explicit 1 at mid", []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA), pc(1, -1)}, []pkix.Extension{cp(oidPolA)}, nil},
		{"inhibit mapping 0 at top, mapping in mid, leaf a", []pkix.Extension{cp(oidPolA, oidPolB), pc(0, 0)}, []pkix.Extension{cp(oidPolA, oidPolB), pm(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolA)}},
		{"inhibit mapping 0 at top, mapping in mid, leaf b", []pkix.Extension{cp(oidPolA, oidPolB), pc(0, 0)}, []pkix.Extension{cp(oidPolA, oidPolB), pm(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolB)}},
		{"inhibit mapping 1 at top", []pkix.Extension{cp(oidPolA, oidPolB), pc(0, 1)}, []pkix.Extension{cp(oidPolA, oidPolB), pm(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolB), pm(oidPolB, oidPolA)}, []pkix.Extension{cp(oidPolA)}},
		{"mapping twice", []pkix.Extension{cp(oidPolA), pc(0, -1)}, []pkix.Extension{cp(oidPolA), pm(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolB), pm(oidPolB, oidPolA)}, []pkix.Extension{cp(oidPolA)}},
		{"mapping from any", []pkix.Extension{cp(oidAny), pc(0, -1)}, []pkix.Extension{cp(oidAny), pm(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolB)}, []pkix.Extension{cp(oidPolB)}},
		{"inhibit any 1 at top", []pkix.Extension{cp(oidAny), pc(0, -1), iap(1)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidPolA)}},
		{"inhibit any 2 at top", []pkix.Extension{cp(oidAny), pc(0, -1), iap(2)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidPolA)}},
		{"any then specific", []pkix.Extension{cp(oidAny), pc(0, -1)}, []pkix.Extension{cp(oidPolA)}, []pkix.Extension{cp(oidPolA, oidPolB)}, []pkix.Extension{cp(oidPolB)}},
		{"mapping to any without policies", []pkix.Extension{pm(oidPolA, oidAny)}, nil, nil, nil},
		{"specific narrowing", []pkix.Extension{cp(oidPolA, oidPolB), pc(0, -1)}, []pkix.Extension{cp(oidPolB)}, []pkix.Extension{cp(oidAny)}, []pkix.Extension{cp(oidPolA)}},
	}
	for _, ds := range deeps {
		r := p.make(p.tmpl("Deep Root", true), nil, rk)
		mk := func(cn string, parent *cert, exts []pkix.Extension, ca bool) *cert {
			tm := p.tmpl(cn, ca)
			tm.ExtraExtensions = exts
			return p.make(tm, parent, ik)
		}
		top := mk("Deep Top", r, ds.top, true)
		mid := mk("Deep Mid", top, ds.mid, true)
		bot := mk("Deep Bottom", mid, ds.bottom, true)
		l := mk("Deep Leaf", bot, ds.leaf, false)
		verify("policy three "+ds.name, l.der, [][]byte{r.der}, [][]byte{top.der, mid.der, bot.der}, t0, code)
	}

	// Two chains failing differently: one by its policies, one by its key usage.
	{
		r1 := p.make(p.tmpl("Mixed Root 1", true), nil, rk)
		rk2 := p.key("p256")
		r2 := p.make(p.tmpl("Mixed Root 2", true), nil, rk2)
		ta := p.tmpl("Mixed Inter", true)
		ta.ExtraExtensions = []pkix.Extension{cp(oidPolA), pc(0, -1)}
		a := p.make(ta, r1, ik)
		tb := p.tmpl("Mixed Inter", true)
		tb.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageTimeStamping}
		b := p.make(tb, r2, ik)
		l := p.make(p.tmpl("Mixed Leaf", false), a, lk)
		verify("two chains, policies and usage", l.der, [][]byte{r1.der, r2.der}, [][]byte{a.der, b.der}, t0, code)
		tc := p.tmpl("Mixed Inter", true)
		tc.PermittedDNSDomains = []string{"example.com"}
		cc := p.make(tc, r2, ik)
		lt := p.tmpl("Mixed Leaf", false)
		lt.DNSNames = []string{"evil.com"}
		l2 := p.make(lt, a, lk)
		verify("two chains, policies and names", l2.der, [][]byte{r1.der, r2.der}, [][]byte{a.der, cc.der}, t0, code)
	}

	// DNS constraints whose order turns on `.` sorting lowest.
	{
		for _, set := range [][]string{
			{"a-c.com", "a.c.com", "c.com", "b.d.com", "b-d.com", "x.example.com", "x-example.com"},
			{"a.c.com", "a-c.com", "z.a-c.com", "a.b.c.com", "*c.com"},
		} {
			for _, excl := range []bool{false, true} {
				it := p.tmpl("Order Inter", true)
				if excl {
					it.ExcludedDNSDomains = set
				} else {
					it.PermittedDNSDomains = set
				}
				in := p.make(it, rootC, ik)
				for _, d := range []string{"q.c.com", "a-c.com", "a.c.com", "z.b-d.com", "z.b.d.com", "d.com", "y.x-example.com", "example.com", "b.c.com", "*.c.com", "*.a.c.com", "q-c.com", "x.a-c.com"} {
					lt := p.tmpl("Order Leaf", false)
					lt.DNSNames = []string{d}
					l := p.make(lt, in, lk)
					verify(fmt.Sprintf("nc order %v excluded=%v %s", set, excl, d), l.der, [][]byte{rootC.der}, [][]byte{in.der}, t0, code)
				}
			}
		}
	}

	// A constrained root's own SAN, and IPv4-mapped addresses given raw.
	{
		rt := p.tmpl("SAN Root", true)
		rt.PermittedDNSDomains = []string{"example.com"}
		rt.DNSNames = []string{"evil.com"}
		r := p.make(rt, nil, rk)
		in := p.make(p.tmpl("SAN Inter", true), r, ik)
		lt := p.tmpl("SAN Leaf", false)
		lt.DNSNames = []string{"a.example.com"}
		l := p.make(lt, in, lk)
		verify("nc root's own san", l.der, [][]byte{r.der}, [][]byte{in.der}, t0, code)

		mapped := append(append(make([]byte, 10), 0xff, 0xff), 10, 0, 0, 0)
		mask104 := append(bytes.Repeat([]byte{0xff}, 13), 0, 0, 0)
		for _, cons := range []struct {
			name string
			val  []byte
		}{
			{"mapped constraint", append(append([]byte(nil), mapped...), mask104...)},
			{"v4 constraint", []byte{10, 0, 0, 0, 255, 0, 0, 0}},
		} {
			for _, excl := range []bool{false, true} {
				tag := byte(0xa0)
				if excl {
					tag = 0xa1
				}
				it := p.tmpl("IP Inter", true)
				it.ExtraExtensions = []pkix.Extension{ext(oidNC, true, seq(tlv(tag, seq(tlv(0x87, cons.val)))))}
				in := p.make(it, rootC, ik)
				for _, san := range []struct {
					name string
					ip   []byte
				}{
					{"mapped 10.1.2.3", append(append(make([]byte, 10), 0xff, 0xff), 10, 1, 2, 3)},
					{"v4 10.1.2.3", []byte{10, 1, 2, 3}},
					{"v4 11.1.2.3", []byte{11, 1, 2, 3}},
				} {
					lt := p.tmpl("IP Leaf", false)
					lt.ExtraExtensions = []pkix.Extension{ext(oidSAN, false, seq(tlv(0x87, san.ip)))}
					l := p.make(lt, in, lk)
					verify(fmt.Sprintf("nc ip %s excluded=%v %s", cons.name, excl, san.name), l.der, [][]byte{rootC.der}, [][]byte{in.der}, t0, code)
				}
			}
		}
	}

	for _, d := range table {
		o.DER = append(o.DER, b64(d))
	}
	out, err := json.MarshalIndent(o, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_X509_OUT"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Logf("%d parse cases, %d verify cases", len(o.Parse), len(o.Verify))
}
