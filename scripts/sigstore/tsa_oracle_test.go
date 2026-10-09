package main

// sigstore-go's answers (as buildx v0.37.1 vendors it) for crates/sigstore/tests/tsa.rs:
// RFC 3161 responses built here with digitorus/pkcs7 and encoding/asn1, valid and broken
// in each way SigstoreTimestampingAuthority.Verify tells apart, and the real Sigstore
// timestamp of the moby/buildkit bundle in crates/sigstore/testdata/real, each verified
// by sigstore-go. `generate-tsa` copies this file into its own directory of buildx's
// cmd and runs it there.

import (
	"bytes"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha512"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/asn1"
	"encoding/base64"
	"encoding/json"
	"math/big"
	"os"
	"path/filepath"
	"regexp"
	"testing"
	"time"

	"github.com/digitorus/pkcs7"
	"github.com/sigstore/sigstore-go/pkg/root"
)

type tsaTime struct {
	Secs  int64 `json:"secs"`
	Nanos int   `json:"nanos"`
}

type tsaAuthority struct {
	URI           string   `json:"uri"`
	Root          string   `json:"root,omitempty"`
	Intermediates []string `json:"intermediates"`
	Leaf          string   `json:"leaf,omitempty"`
	Start         *tsaTime `json:"start,omitempty"`
	End           *tsaTime `json:"end,omitempty"`
}

type tsaCase struct {
	Name      string       `json:"name"`
	Authority tsaAuthority `json:"authority"`
	Token     string       `json:"token"`
	Signature string       `json:"signature"`
	// sigstore-go's answer.
	Error string   `json:"error"`
	Time  *tsaTime `json:"time,omitempty"`
}

var (
	oidTSTInfo   = asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 9, 16, 1, 4}
	oidPolicy    = asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 2}
	oidEKU       = asn1.ObjectIdentifier{2, 5, 29, 37}
	oidSigningT  = asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 9, 5}
	hashOIDs     = map[crypto.Hash]asn1.ObjectIdentifier{crypto.SHA1: {1, 3, 14, 3, 2, 26}, crypto.SHA256: {2, 16, 840, 1, 101, 3, 4, 2, 1}, crypto.SHA384: {2, 16, 840, 1, 101, 3, 4, 2, 2}, crypto.SHA512: {2, 16, 840, 1, 101, 3, 4, 2, 3}}
	pointerRegex = regexp.MustCompile(`0xc[0-9a-f]{6,}`)
)

type node struct {
	cert *x509.Certificate
	key  crypto.Signer
}

func mustEC(t *testing.T, c elliptic.Curve) crypto.Signer {
	k, err := ecdsa.GenerateKey(c, rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return k
}

func date(y int) time.Time { return time.Date(y, 1, 1, 0, 0, 0, 0, time.UTC) }

// issue makes a certificate for key from tmpl, signed by parent (self-signed if nil).
func issue(t *testing.T, tmpl *x509.Certificate, key crypto.Signer, parent *node) *node {
	signer, pc := key, tmpl
	if parent != nil {
		signer, pc = parent.key, parent.cert
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, pc, key.Public(), signer)
	if err != nil {
		t.Fatal(err)
	}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return &node{c, key}
}

func caTemplate(serial int64, cn string, from, to int) *x509.Certificate {
	return &x509.Certificate{
		SerialNumber:          big.NewInt(serial),
		Subject:               pkix.Name{CommonName: cn, Organization: []string{"shards test"}},
		NotBefore:             date(from),
		NotAfter:              date(to),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
	}
}

// leafTemplate: a TSA leaf; critical says whether its EKU extension is critical, ekus
// its usages (none: no extension).
func leafTemplate(serial int64, from, to int, critical bool, ekus ...asn1.ObjectIdentifier) *x509.Certificate {
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(serial),
		Subject:               pkix.Name{CommonName: "shards test tsa"},
		NotBefore:             date(from),
		NotAfter:              date(to),
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageDigitalSignature,
	}
	if len(ekus) > 0 {
		v, err := asn1.Marshal(ekus)
		if err != nil {
			panic(err)
		}
		tmpl.ExtraExtensions = append(tmpl.ExtraExtensions, pkix.Extension{Id: oidEKU, Critical: critical, Value: v})
	}
	return tmpl
}

var (
	ekuTimeStamping = asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 3, 8}
	ekuCodeSigning  = asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 3, 3}
	ekuServerAuth   = asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 3, 1}
)

type tst struct {
	Version        int
	Policy         asn1.ObjectIdentifier
	MessageImprint struct {
		HashAlgorithm pkix.AlgorithmIdentifier
		HashedMessage []byte
	}
	SerialNumber *big.Int
	Time         asn1.RawValue
}

type statusInfo struct {
	Status       int
	StatusString []string       `asn1:"optional"`
	FailInfo     asn1.BitString `asn1:"optional"`
}

type response struct {
	Status         statusInfo
	TimeStampToken asn1.RawValue `asn1:"optional"`
}

type tokenCfg struct {
	imprintHash crypto.Hash
	imprintOID  asn1.ObjectIdentifier
	message     []byte
	hashed      []byte // overrides the imprint's hash of message
	genTime     string
	utc         bool
	signer      *node
	chain       []*x509.Certificate
	skipCerts   bool
	digest      crypto.Hash // pkcs7's digest; SHA-256 by default
	extraAttrs  []pkcs7.Attribute
}

func hashOf(h crypto.Hash, b []byte) []byte {
	switch h {
	case crypto.SHA1:
		s := sha1.Sum(b)
		return s[:]
	case crypto.SHA384:
		s := sha512.Sum384(b)
		return s[:]
	case crypto.SHA512:
		s := sha512.Sum512(b)
		return s[:]
	}
	s := sha256.Sum256(b)
	return s[:]
}

// token: the TSTInfo of c, signed into a CMS SignedData, its DER and the TSTInfo's.
func token(t *testing.T, c tokenCfg) ([]byte, []byte) {
	var info tst
	info.Version = 1
	info.Policy = oidPolicy
	oid := c.imprintOID
	if oid == nil {
		oid = hashOIDs[c.imprintHash]
	}
	info.MessageImprint.HashAlgorithm = pkix.AlgorithmIdentifier{Algorithm: oid}
	info.MessageImprint.HashedMessage = c.hashed
	if c.hashed == nil {
		info.MessageImprint.HashedMessage = hashOf(c.imprintHash, c.message)
	}
	info.SerialNumber = big.NewInt(4242)
	tag := 24
	if c.utc {
		tag = 23
	}
	info.Time = asn1.RawValue{Tag: tag, Bytes: []byte(c.genTime)}
	tstDER, err := asn1.Marshal(info)
	if err != nil {
		t.Fatal(err)
	}
	sd, err := pkcs7.NewSignedData(tstDER)
	if err != nil {
		t.Fatal(err)
	}
	digest := c.digest
	if digest == 0 {
		digest = crypto.SHA256
	}
	sd.SetDigestAlgorithm(hashOIDs[digest])
	sd.SetContentType(oidTSTInfo)
	sd.GetSignedData().Version = 3
	cfg := pkcs7.SignerInfoConfig{ExtraSignedAttributes: c.extraAttrs, SkipCertificates: c.skipCerts}
	if len(c.chain) > 0 {
		err = sd.AddSignerChain(c.signer.cert, c.signer.key, c.chain, cfg)
	} else {
		err = sd.AddSigner(c.signer.cert, c.signer.key, cfg)
	}
	if err != nil {
		t.Fatal(err)
	}
	der, err := sd.Finish()
	if err != nil {
		t.Fatal(err)
	}
	return der, tstDER
}

func respond(t *testing.T, tok []byte) []byte {
	r := response{TimeStampToken: asn1.RawValue{FullBytes: tok}}
	if tok == nil {
		r.TimeStampToken = asn1.RawValue{}
	}
	b, err := asn1.Marshal(r)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// berify re-encodes DER as BER: constructed elements below the top with indefinite
// lengths, primitives with a long-form length.
func berify(der []byte, top bool) []byte {
	var out []byte
	for len(der) > 0 {
		var raw asn1.RawValue
		rest, err := asn1.Unmarshal(der, &raw)
		if err != nil {
			panic(err)
		}
		hdr := raw.FullBytes[:len(raw.FullBytes)-len(raw.Bytes)]
		tagLen := 1
		if hdr[0]&0x1f == 0x1f {
			for tagLen < len(hdr) && hdr[tagLen]&0x80 != 0 {
				tagLen++
			}
			tagLen++
		}
		tagBytes := hdr[:tagLen]
		switch {
		case raw.IsCompound && top:
			inner := berify(raw.Bytes, false)
			out = append(out, tagBytes...)
			out = append(out, longLen(len(inner))...)
			out = append(out, inner...)
		case raw.IsCompound:
			out = append(out, tagBytes...)
			out = append(out, 0x80)
			out = append(out, berify(raw.Bytes, false)...)
			out = append(out, 0, 0)
		default:
			out = append(out, tagBytes...)
			if len(raw.Bytes) < 128 {
				out = append(out, 0x81, byte(len(raw.Bytes)))
			} else {
				out = append(out, longLen(len(raw.Bytes))...)
			}
			out = append(out, raw.Bytes...)
		}
		der = rest
	}
	return out
}

func longLen(n int) []byte {
	if n < 128 {
		return []byte{byte(n)}
	}
	var b []byte
	for v := n; v > 0; v >>= 8 {
		b = append([]byte{byte(v)}, b...)
	}
	return append([]byte{0x80 | byte(len(b))}, b...)
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

func certsB64(cs []*x509.Certificate) []string {
	out := []string{}
	for _, c := range cs {
		out = append(out, b64(c.Raw))
	}
	return out
}

func stamp(t *time.Time) *tsaTime {
	if t == nil || t.IsZero() {
		return nil
	}
	return &tsaTime{t.Unix(), t.Nanosecond()}
}

func run(name string, a *root.SigstoreTimestampingAuthority, tok, sig []byte) tsaCase {
	c := tsaCase{
		Name:      name,
		Token:     b64(tok),
		Signature: b64(sig),
		Authority: tsaAuthority{
			URI:           a.URI,
			Intermediates: certsB64(a.Intermediates),
			Start:         stamp(&a.ValidityPeriodStart),
			End:           stamp(&a.ValidityPeriodEnd),
		},
	}
	if a.Root != nil {
		c.Authority.Root = b64(a.Root.Raw)
	}
	if a.Leaf != nil {
		c.Authority.Leaf = b64(a.Leaf.Raw)
	}
	ts, err := a.Verify(tok, sig)
	if err != nil {
		c.Error = pointerRegex.ReplaceAllString(err.Error(), "(ptr)")
	} else {
		c.Time = &tsaTime{ts.Time.Unix(), ts.Time.Nanosecond()}
	}
	return c
}

func TestShardsTSAOracle(t *testing.T) {
	out := os.Getenv("SHARDS_TSA_OUT")
	repo := os.Getenv("SHARDS_ROOT")
	if out == "" || repo == "" {
		t.Skip("SHARDS_TSA_OUT and SHARDS_ROOT name the output and the shards checkout")
	}
	const uri = "https://tsa.shards.test/api/v1/timestamp"
	rootN := issue(t, caTemplate(1, "shards test root", 2000, 2099), mustEC(t, elliptic.P384()), nil)
	inter := issue(t, caTemplate(2, "shards test intermediate", 2000, 2099), mustEC(t, elliptic.P384()), rootN)
	leaf := issue(t, leafTemplate(3, 2005, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P256()), inter)
	other := issue(t, leafTemplate(4, 2005, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P256()), inter)
	rsaKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	rsaLeaf := issue(t, leafTemplate(5, 2005, 2090, true, ekuTimeStamping), rsaKey, inter)
	p384Leaf := issue(t, leafTemplate(6, 2005, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P384()), inter)
	noEKU := issue(t, leafTemplate(7, 2005, 2090, true), mustEC(t, elliptic.P256()), inter)
	softEKU := issue(t, leafTemplate(8, 2005, 2090, false, ekuTimeStamping), mustEC(t, elliptic.P256()), inter)
	twoEKU := issue(t, leafTemplate(9, 2005, 2090, true, ekuTimeStamping, ekuCodeSigning), mustEC(t, elliptic.P256()), inter)
	codeEKU := issue(t, leafTemplate(10, 2005, 2090, true, ekuCodeSigning), mustEC(t, elliptic.P256()), inter)
	late := issue(t, leafTemplate(11, 2020, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P256()), inter)
	serverInterT := caTemplate(12, "shards test server intermediate", 2000, 2099)
	serverInterT.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}
	serverInter := issue(t, serverInterT, mustEC(t, elliptic.P384()), rootN)
	underServer := issue(t, leafTemplate(13, 2005, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P256()), serverInter)
	anyInterT := caTemplate(14, "shards test any intermediate", 2000, 2099)
	anyInterT.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageAny}
	anyInter := issue(t, anyInterT, mustEC(t, elliptic.P384()), rootN)
	underAny := issue(t, leafTemplate(15, 2005, 2090, true, ekuTimeStamping), mustEC(t, elliptic.P256()), anyInter)
	otherRoot := issue(t, caTemplate(16, "shards test other root", 2000, 2099), mustEC(t, elliptic.P384()), nil)
	_ = ekuServerAuth

	sig := []byte("the signature a timestamp vouches for")
	auth := func(mods ...func(*root.SigstoreTimestampingAuthority)) *root.SigstoreTimestampingAuthority {
		a := &root.SigstoreTimestampingAuthority{Root: rootN.cert, Intermediates: []*x509.Certificate{inter.cert}, URI: uri}
		for _, m := range mods {
			m(a)
		}
		return a
	}
	base := func(signer *node, chain ...*x509.Certificate) tokenCfg {
		return tokenCfg{imprintHash: crypto.SHA256, message: sig, genTime: "20250801120000Z", signer: signer, chain: chain}
	}
	var cases []tsaCase
	add := func(name string, a *root.SigstoreTimestampingAuthority, resp []byte, s []byte) {
		cases = append(cases, run(name, a, resp, s))
	}
	tok := func(c tokenCfg) []byte { d, _ := token(t, c); return respond(t, d) }

	good := base(leaf, inter.cert)
	add("valid ECDSA chain, certificates in the token", auth(), tok(good), sig)
	add("valid, intermediates only in the token", auth(func(a *root.SigstoreTimestampingAuthority) { a.Intermediates = nil }), tok(good), sig)
	c := good
	c.skipCerts = true
	add("valid, the leaf only in the authority", auth(func(a *root.SigstoreTimestampingAuthority) { a.Leaf = leaf.cert }), tok(c), sig)
	add("valid, the leaf in both", auth(func(a *root.SigstoreTimestampingAuthority) { a.Leaf = leaf.cert }), tok(good), sig)
	add("no certificates and no leaf", auth(), tok(c), sig)
	add("authority leaf differs from the signer", auth(func(a *root.SigstoreTimestampingAuthority) { a.Leaf = other.cert }), tok(good), sig)
	add("valid RSA leaf", auth(), tok(base(rsaLeaf, inter.cert)), sig)
	add("valid P-384 leaf", auth(), tok(base(p384Leaf, inter.cert)), sig)
	for _, h := range []crypto.Hash{crypto.SHA384, crypto.SHA512} {
		c := good
		c.imprintHash = h
		add("valid "+h.String()+" imprint", auth(), tok(c), sig)
		c = good
		c.digest = h
		add("valid "+h.String()+" CMS digest", auth(), tok(c), sig)
	}
	c = good
	c.imprintHash = crypto.SHA1
	add("SHA-1 imprint", auth(), tok(c), sig)
	c = good
	c.digest = crypto.SHA1
	add("SHA-1 CMS digest", auth(), tok(c), sig)
	c = good
	c.genTime = "20250801120000.123Z"
	add("fractional genTime", auth(), tok(c), sig)
	c = good
	c.genTime = "20250801120000.120Z"
	add("fractional genTime with a trailing zero", auth(), tok(c), sig)
	c = good
	c.genTime = "250801120000Z"
	c.utc = true
	add("UTCTime genTime", auth(), tok(c), sig)
	c = good
	c.genTime = "2508011200Z"
	c.utc = true
	add("UTCTime genTime without seconds", auth(), tok(c), sig)
	c = good
	c.genTime = "20950801120000+0130"
	add("genTime with an offset, after the leaf expires", auth(), tok(c), sig)
	c = good
	c.genTime = "20950801120000Z"
	add("genTime after the leaf expires", auth(), tok(c), sig)
	c = base(late, inter.cert)
	c.genTime = "20150801120000Z"
	add("genTime before the leaf is valid", auth(), tok(c), sig)
	c = good
	c.genTime = "2025080112000Z"
	add("malformed genTime", auth(), tok(c), sig)
	c = good
	c.genTime = "20250801120000+0000"
	add("genTime with +0000", auth(), tok(c), sig)
	add("wrong message", auth(), tok(good), []byte("another signature"))
	resp := tok(good)
	bad := append([]byte{}, resp...)
	bad[len(bad)-1] ^= 1
	add("bad CMS signature", auth(), bad, sig)
	der, tstDER := token(t, good)
	i := bytes.Index(der, tstDER)
	mut := append([]byte{}, der...)
	mut[i+len(tstDER)-20] ^= 0x40
	add("TSTInfo altered after signing", auth(), respond(t, mut), sig)
	add("leaf without EKU", auth(), tok(base(noEKU, inter.cert)), sig)
	add("leaf with a non-critical EKU", auth(), tok(base(softEKU, inter.cert)), sig)
	add("leaf with two EKUs", auth(), tok(base(twoEKU, inter.cert)), sig)
	add("leaf for code signing", auth(), tok(base(codeEKU, inter.cert)), sig)
	add("intermediate for server auth", auth(func(a *root.SigstoreTimestampingAuthority) { a.Intermediates = []*x509.Certificate{serverInter.cert} }), tok(base(underServer, serverInter.cert)), sig)
	add("intermediate for any usage", auth(func(a *root.SigstoreTimestampingAuthority) { a.Intermediates = []*x509.Certificate{anyInter.cert} }), tok(base(underAny, anyInter.cert)), sig)
	add("wrong root", auth(func(a *root.SigstoreTimestampingAuthority) { a.Root = otherRoot.cert }), tok(good), sig)
	add("validity starts after the time", auth(func(a *root.SigstoreTimestampingAuthority) { a.ValidityPeriodStart = date(2026) }), tok(good), sig)
	add("validity ends before the time", auth(func(a *root.SigstoreTimestampingAuthority) { a.ValidityPeriodEnd = date(2024) }), tok(good), sig)
	add("validity around the time", auth(func(a *root.SigstoreTimestampingAuthority) {
		a.ValidityPeriodStart = date(2024)
		a.ValidityPeriodEnd = date(2026)
	}), tok(good), sig)
	add("no root", auth(func(a *root.SigstoreTimestampingAuthority) { a.Root = nil }), tok(good), sig)
	add("no root and no URI", auth(func(a *root.SigstoreTimestampingAuthority) {
		a.Root = nil
		a.URI = ""
	}), tok(good), sig)
	c = good
	c.imprintOID = asn1.ObjectIdentifier{2, 16, 840, 1, 101, 3, 4, 2, 4}
	c.hashed = make([]byte, 28)
	add("unknown imprint hash", auth(), tok(c), sig)
	c = good
	c.hashed = []byte{}
	add("empty imprint", auth(), tok(c), sig)
	signingTime, err := asn1.Marshal(time.Date(2001, 1, 1, 0, 0, 0, 0, time.UTC))
	if err != nil {
		t.Fatal(err)
	}
	c = good
	c.extraAttrs = []pkcs7.Attribute{{Type: oidSigningT, Value: asn1.RawValue{FullBytes: signingTime}}}
	add("signing time outside the leaf's validity", auth(), tok(c), sig)

	der, _ = token(t, good)
	add("BER token", auth(), respond(t, berify(der, true)), sig)
	ber := berify(der, true)
	j := bytes.Index(ber, []byte{0x81, 0x01})
	if j > 0 {
		zero := append(append(append([]byte{}, ber[:j]...), 0x82, 0x00, 0x01), ber[j+2:]...)
		add("BER length with a leading zero", auth(), respond(t, zero), sig)
	}
	add("BER token cut short", auth(), respond(t, ber[:len(ber)-1]), sig)
	// Tokens whose own header is DER, so encoding/asn1 hands them to pkcs7's ber2der.
	berTok := func(content ...byte) []byte {
		return respond(t, append(append([]byte{0x30}, longLen(len(content))...), content...))
	}
	add("BER primitive with an indefinite length", auth(), berTok(0x04, 0x80, 0, 0, 0, 0), sig)
	add("BER length too long", auth(), berTok(0x04, 0x85, 1, 2, 3, 4, 5), sig)
	add("BER length with a leading zero, one octet", auth(), berTok(0x04, 0x81, 0x00), sig)
	add("BER length with a leading zero, two octets", auth(), berTok(0x04, 0x82, 0x00, 0x05, 1, 2, 3, 4, 5), sig)
	add("BER length negative", auth(), berTok(0x04, 0x84, 0x80, 0, 0, 0), sig)
	add("BER element past the end", auth(), berTok(0x04), sig)
	add("BER length past the data", auth(), berTok(0x04, 0x05, 0x01), sig)
	add("BER long length past the data", auth(), berTok(0x04, 0x82, 0x01), sig)
	add("BER indefinite element", auth(), berTok(0x30, 0x80, 0x04, 0x01, 0x41, 0, 0), sig)
	add("BER indefinite element unterminated", auth(), berTok(0x30, 0x80, 0x04, 0x01, 0x41), sig)
	add("BER high tag number", auth(), berTok(0x1f, 0x81, 0x01, 0x01, 0x41), sig)
	add("BER high tag number cut short", auth(), berTok(0x1f, 0x81), sig)
	add("BER empty definite", auth(), berTok(0x30, 0x00, 0x06, 0x01, 0x01), sig)
	add("BER long-form short length", auth(), berTok(0x06, 0x81, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02), sig)
	add("BER token of one octet", auth(), respond(t, []byte{0x30}), sig)
	add("not signed data", auth(), respond(t, mustMarshal(t, struct {
		T asn1.ObjectIdentifier
		C asn1.RawValue `asn1:"explicit,tag:0"`
	}{asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 7, 1}, asn1.RawValue{Tag: 4, Bytes: []byte("x")}})), sig)
	add("enveloped data", auth(), respond(t, mustMarshal(t, struct {
		T asn1.ObjectIdentifier
		C asn1.RawValue `asn1:"explicit,tag:0"`
	}{asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 7, 3}, asn1.RawValue{FullBytes: mustMarshal(t, []int{1})}})), sig)
	add("signed data of garbage", auth(), respond(t, mustMarshal(t, struct {
		T asn1.ObjectIdentifier
		C asn1.RawValue `asn1:"explicit,tag:0"`
	}{asn1.ObjectIdentifier{1, 2, 840, 113549, 1, 7, 2}, asn1.RawValue{FullBytes: []byte{0x02, 0x01, 0x05}}})), sig)
	add("token with trailing data", auth(), respond(t, append(append([]byte{}, der...), 0x05, 0x00)), sig)

	resp = tok(good)
	add("response with trailing data", auth(), append(append([]byte{}, resp...), 0), sig)
	add("response cut short", auth(), resp[:len(resp)/2], sig)
	add("empty response", auth(), []byte{}, sig)
	add("garbage", auth(), []byte("not a timestamp"), sig)
	add("an INTEGER", auth(), []byte{0x02, 0x01, 0x00}, sig)
	add("a SEQUENCE of an INTEGER", auth(), []byte{0x30, 0x03, 0x02, 0x01, 0x00}, sig)
	add("granted without a token", auth(), respond(t, nil), sig)
	rejected, err := asn1.Marshal(response{Status: statusInfo{Status: 2, StatusString: []string{"no", "way"}, FailInfo: asn1.BitString{Bytes: []byte{0x80}, BitLength: 1}}})
	if err != nil {
		t.Fatal(err)
	}
	add("rejected", auth(), rejected, sig)
	waiting, err := asn1.Marshal(response{Status: statusInfo{Status: 9}})
	if err != nil {
		t.Fatal(err)
	}
	add("unknown status", auth(), waiting, sig)
	sysfail, err := asn1.Marshal(response{Status: statusInfo{Status: 2, FailInfo: asn1.BitString{Bytes: []byte{0, 0, 0, 0x40}, BitLength: 26}}})
	if err != nil {
		t.Fatal(err)
	}
	add("rejected for system failure", auth(), sysfail, sig)

	// The real Sigstore timestamp of moby/buildkit v0.28.1's arm64 attestation.
	rootJSON, err := os.ReadFile(filepath.Join(repo, "crates/tuf/roots/sigstore/targets/trusted_root.json"))
	if err != nil {
		t.Fatal(err)
	}
	tr, err := root.NewTrustedRootFromJSON(rootJSON)
	if err != nil {
		t.Fatal(err)
	}
	bundleJSON, err := os.ReadFile(filepath.Join(repo, "crates/sigstore/testdata/real/buildkit-v0.28.1-arm64.bundle.json"))
	if err != nil {
		t.Fatal(err)
	}
	var b struct {
		VerificationMaterial struct {
			TimestampVerificationData struct {
				Rfc3161Timestamps []struct {
					SignedTimestamp []byte `json:"signedTimestamp"`
				} `json:"rfc3161Timestamps"`
			} `json:"timestampVerificationData"`
		} `json:"verificationMaterial"`
		DsseEnvelope struct {
			Signatures []struct {
				Sig []byte `json:"sig"`
			} `json:"signatures"`
		} `json:"dsseEnvelope"`
	}
	if err := json.Unmarshal(bundleJSON, &b); err != nil {
		t.Fatal(err)
	}
	real := tr.TimestampingAuthorities()[0].(*root.SigstoreTimestampingAuthority)
	realTS := b.VerificationMaterial.TimestampVerificationData.Rfc3161Timestamps[0].SignedTimestamp
	realSig := b.DsseEnvelope.Signatures[0].Sig
	add("Sigstore's timestamp of moby/buildkit v0.28.1", real, realTS, realSig)
	add("Sigstore's timestamp over another signature", real, realTS, sig)
	add("Sigstore's timestamp against the test root", auth(), realTS, realSig)

	data, err := json.MarshalIndent(cases, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func mustMarshal(t *testing.T, v any) []byte {
	b, err := asn1.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return b
}
