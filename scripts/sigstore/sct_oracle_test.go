package zzshardssct

// sigstore-go v1.2.2's answers (as buildx v0.37.1 vendors it) for
// crates/sigstore/tests/sct.rs: Fulcio-like certificates with SCTs embedded by test CT
// logs, and their mutations, each verified by verify.VerifySignedCertificateTimestamp
// over the chains crypto/x509 builds for it. `generate-sct` copies this file into its own
// package of the buildx checkout and runs it there.

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/md5"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha512"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/asn1"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"math/big"
	"net"
	"net/url"
	"os"
	"testing"
	"time"

	ct "github.com/google/certificate-transparency-go"
	ctx509 "github.com/google/certificate-transparency-go/x509"
	"github.com/sigstore/sigstore-go/pkg/root"
	"github.com/sigstore/sigstore-go/pkg/verify"
)

type ctlogJSON struct {
	KeyID string `json:"keyId"`
	Key   string `json:"key"`
	Start *int64 `json:"startUnixNano"`
	End   *int64 `json:"endUnixNano"`
}

type sctCase struct {
	Name          string      `json:"name"`
	Leaf          string      `json:"leaf"`
	Intermediates []string    `json:"intermediates"`
	Roots         []string    `json:"roots"`
	Now           int64       `json:"now"`
	Threshold     int         `json:"threshold"`
	CTLogs        []ctlogJSON `json:"ctlogs"`
	Error         string      `json:"error"`
}

var base = time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

var oidSCT = asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 11129, 2, 4, 2}

type ca struct {
	cert *x509.Certificate
	key  crypto.Signer
}

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func newCA(t *testing.T, name string, parent *ca, extra []pkix.Extension) *ca {
	return newCAWith(t, name, parent, extra, nil)
}

// newCAWith makes a CA whose template `tweak` changes first.
func newCAWith(t *testing.T, name string, parent *ca, extra []pkix.Extension, tweak func(*x509.Certificate)) *ca {
	key := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(int64(len(name)) + 1000),
		Subject:               pkix.Name{CommonName: name, Organization: []string{"shards test"}},
		NotBefore:             base.Add(-time.Hour),
		NotAfter:              base.Add(24 * time.Hour),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning},
		ExtraExtensions:       extra,
	}
	if tweak != nil {
		tweak(tmpl)
	}
	parentCert, parentKey := tmpl, crypto.Signer(key)
	if parent != nil {
		parentCert, parentKey = parent.cert, parent.key
	}
	der := must(x509.CreateCertificate(rand.Reader, tmpl, parentCert, key.Public(), parentKey))
	return &ca{cert: must(x509.ParseCertificate(der)), key: key}
}

// leafTemplate is a Fulcio-like leaf; extra extensions are added before the SCT list.
func leafTemplate(extra []pkix.Extension) (*x509.Certificate, crypto.Signer) {
	key := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	u := must(url.Parse("https://github.com/moby/buildkit/.github/workflows/release.yml@refs/tags/v0.28.1"))
	return &x509.Certificate{
		SerialNumber:    big.NewInt(42),
		NotBefore:       base,
		NotAfter:        base.Add(10 * time.Minute),
		KeyUsage:        x509.KeyUsageDigitalSignature,
		ExtKeyUsage:     []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning},
		URIs:            []*url.URL{u},
		ExtraExtensions: extra,
	}, key
}

// sct is one SCT to embed: its log, the time, its fields, and how to sign it.
type sct struct {
	log       crypto.Signer
	logID     []byte
	ts        uint64
	version   byte
	ext       []byte
	hash      byte
	sigAlg    byte
	issuerKey []byte // the key hashed into the signed input; the issuer's by default
	corrupt   bool
	p1363     bool
	garbage   bool
	raw       []byte // replaces the whole serialized SCT
}

func logID(k crypto.Signer) []byte {
	der := must(x509.MarshalPKIXPublicKey(k.Public()))
	h := sha256.Sum256(der)
	return h[:]
}

func hashOf(alg byte, data []byte) ([]byte, crypto.Hash) {
	switch alg {
	case 1:
		h := md5.Sum(data)
		return h[:], crypto.MD5
	case 2:
		h := sha1.Sum(data)
		return h[:], crypto.SHA1
	case 3:
		h := sha256.Sum224(data)
		return h[:], crypto.SHA224
	case 5:
		h := sha512.Sum384(data)
		return h[:], crypto.SHA384
	case 6:
		h := sha512.Sum512(data)
		return h[:], crypto.SHA512
	default:
		h := sha256.Sum256(data)
		return h[:], crypto.SHA256
	}
}

func be16(n int) []byte { return []byte{byte(n >> 8), byte(n)} }

// serialize builds the SCT, its signature over the precertificate `tbs`.
func (s sct) serialize(tbs []byte, issuerSPKI []byte) []byte {
	if s.raw != nil {
		return s.raw
	}
	keyHash := sha256.Sum256(issuerSPKI)
	if s.issuerKey != nil {
		keyHash = sha256.Sum256(s.issuerKey)
	}
	var input []byte
	input = append(input, s.version, 0)
	input = binary.BigEndian.AppendUint64(input, s.ts)
	input = append(input, 0, 1)
	input = append(input, keyHash[:]...)
	input = append(input, byte(len(tbs)>>16), byte(len(tbs)>>8), byte(len(tbs)))
	input = append(input, tbs...)
	input = append(input, be16(len(s.ext))...)
	input = append(input, s.ext...)
	digest, h := hashOf(s.hash, input)
	var sig []byte
	switch k := s.log.(type) {
	case *ecdsa.PrivateKey:
		if s.p1363 {
			r, ss := must2(ecdsa.Sign(rand.Reader, k, digest))
			sig = append(r.FillBytes(make([]byte, 32)), ss.FillBytes(make([]byte, 32))...)
		} else {
			sig = must(ecdsa.SignASN1(rand.Reader, k, digest))
		}
	case *rsa.PrivateKey:
		sig = must(rsa.SignPKCS1v15(rand.Reader, k, h, digest))
	case ed25519.PrivateKey:
		sig = ed25519.Sign(k, input)
	}
	if s.garbage {
		sig = append(sig, 0xde, 0xad)
	}
	if s.corrupt {
		sig[len(sig)/2] ^= 0x40
	}
	id := s.logID
	if id == nil {
		id = logID(s.log)
	}
	var out []byte
	out = append(out, s.version)
	out = append(out, id...)
	out = binary.BigEndian.AppendUint64(out, s.ts)
	out = append(out, be16(len(s.ext))...)
	out = append(out, s.ext...)
	out = append(out, s.hash, s.sigAlg)
	out = append(out, be16(len(sig))...)
	return append(out, sig...)
}

func must2[A, B any](a A, b B, err error) (A, B) {
	if err != nil {
		panic(err)
	}
	return a, b
}

// sctListExt wraps serialized SCTs into the extension's value.
func sctListExt(scts [][]byte) []byte {
	var list []byte
	for _, s := range scts {
		list = append(list, be16(len(s))...)
		list = append(list, s...)
	}
	return must(asn1.Marshal(append(be16(len(list)), list...)))
}

// issue makes the leaf with SCTs signed over its precertificate TBS (the leaf as
// RemoveSCTList rebuilds it), then `value`, if set, replacing the SCT list's value.
func issue(t *testing.T, issuer *ca, extra []pkix.Extension, scts []sct, value func([][]byte) []byte) *x509.Certificate {
	tmpl, key := leafTemplate(extra)
	placeholder := pkix.Extension{Id: oidSCT, Value: sctListExt([][]byte{{0}})}
	tmpl.ExtraExtensions = append(append([]pkix.Extension{}, extra...), placeholder)
	der := must(x509.CreateCertificate(rand.Reader, tmpl, issuer.cert, key.Public(), issuer.key))
	pre := must(ctx509.RemoveSCTList(must(x509.ParseCertificate(der)).RawTBSCertificate))
	var serialized [][]byte
	for _, s := range scts {
		serialized = append(serialized, s.serialize(pre, issuer.cert.RawSubjectPublicKeyInfo))
	}
	v := sctListExt(serialized)
	if value != nil {
		v = value(serialized)
	}
	tmpl.ExtraExtensions = append(append([]pkix.Extension{}, extra...), pkix.Extension{Id: oidSCT, Value: v})
	if scts == nil && value == nil {
		tmpl.ExtraExtensions = append([]pkix.Extension{}, extra...)
	}
	der = must(x509.CreateCertificate(rand.Reader, tmpl, issuer.cert, key.Public(), issuer.key))
	return must(x509.ParseCertificate(der))
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

type logSpec struct {
	key        crypto.Signer
	start, end *time.Time
}

func run(t *testing.T, name string, leaf *x509.Certificate, inters []*x509.Certificate, roots []*x509.Certificate, threshold int, logs []logSpec) sctCase {
	rootPool, interPool := x509.NewCertPool(), x509.NewCertPool()
	c := sctCase{Name: name, Leaf: b64(leaf.Raw), Now: base.Add(time.Minute).Unix(), Threshold: threshold}
	for _, r := range roots {
		rootPool.AddCert(r)
		c.Roots = append(c.Roots, b64(r.Raw))
	}
	for _, i := range inters {
		interPool.AddCert(i)
		c.Intermediates = append(c.Intermediates, b64(i.Raw))
	}
	ctlogs := map[string]*root.TransparencyLog{}
	for _, l := range logs {
		id := logID(l.key)
		tl := &root.TransparencyLog{ID: id, PublicKey: l.key.Public(), HashFunc: crypto.SHA256}
		j := ctlogJSON{KeyID: hex.EncodeToString(id), Key: b64(must(x509.MarshalPKIXPublicKey(l.key.Public())))}
		if l.start != nil {
			tl.ValidityPeriodStart = *l.start
			n := l.start.UnixNano()
			j.Start = &n
		}
		if l.end != nil {
			tl.ValidityPeriodEnd = *l.end
			n := l.end.UnixNano()
			j.End = &n
		}
		ctlogs[hex.EncodeToString(id)] = tl
		c.CTLogs = append(c.CTLogs, j)
	}
	chains, err := leaf.Verify(x509.VerifyOptions{
		Roots: rootPool, Intermediates: interPool, CurrentTime: base.Add(time.Minute),
		KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning},
	})
	if err != nil {
		t.Fatalf("%s: chain: %v", name, err)
	}
	tr := must(root.NewTrustedRoot(root.TrustedRootMediaType01, nil, ctlogs, nil, nil))
	if err := verify.VerifySignedCertificateTimestamp(chains, threshold, tr); err != nil {
		c.Error = err.Error()
	}
	return c
}

func TestShardsSCTOracle(t *testing.T) {
	out := os.Getenv("SHARDS_SCT_OUT")
	if out == "" {
		t.Skip("SHARDS_SCT_OUT names the output")
	}
	rootCA := newCA(t, "test root", nil, nil)
	inter := newCA(t, "test intermediate", rootCA, nil)
	p256 := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	p256b := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	p384 := must(ecdsa.GenerateKey(elliptic.P384(), rand.Reader))
	rsa2048 := must(rsa.GenerateKey(rand.Reader, 2048))
	rsa1024 := must(rsa.GenerateKey(rand.Reader, 1024))
	_, ed := must2(ed25519.GenerateKey(rand.Reader))
	ts := uint64(base.Add(30 * time.Second).UnixMilli())
	ok := func(k crypto.Signer) sct { return sct{log: k, ts: ts, hash: 4, sigAlg: 3} }
	okRSA := func(k crypto.Signer) sct { return sct{log: k, ts: ts, hash: 4, sigAlg: 1} }
	logs := []logSpec{{key: p256}}
	chain := []*x509.Certificate{inter.cert}
	roots := []*x509.Certificate{rootCA.cert}
	var cases []sctCase
	add := func(name string, leaf *x509.Certificate, threshold int, logs []logSpec) {
		cases = append(cases, run(t, name, leaf, chain, roots, threshold, logs))
	}
	tm := func(d time.Duration) *time.Time { x := base.Add(30*time.Second + d); return &x }

	add("one log, threshold 1", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, logs)
	add("one log, threshold 2", issue(t, inter, nil, []sct{ok(p256)}, nil), 2, logs)
	add("two logs, threshold 2", issue(t, inter, nil, []sct{ok(p256), ok(p256b)}, nil), 2, []logSpec{{key: p256}, {key: p256b}})
	add("one log twice, threshold 2", issue(t, inter, nil, []sct{ok(p256), ok(p256)}, nil), 2, logs)
	add("a bad SCT, then a good one from its log", issue(t, inter, nil, func() []sct { b := ok(p256); b.corrupt = true; return []sct{b, ok(p256)} }(), nil), 1, logs)
	add("unknown log", issue(t, inter, nil, []sct{ok(p256b)}, nil), 1, logs)
	add("no SCT extension", issue(t, inter, nil, nil, nil), 1, logs)
	add("before the log's start", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, []logSpec{{key: p256, start: tm(time.Millisecond)}})
	add("at the log's start", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, []logSpec{{key: p256, start: tm(0)}})
	add("a nanosecond after the log's start", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, []logSpec{{key: p256, start: tm(time.Nanosecond)}})
	add("after the log's end", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, []logSpec{{key: p256, end: tm(-time.Nanosecond)}})
	add("at the log's end", issue(t, inter, nil, []sct{ok(p256)}, nil), 1, []logSpec{{key: p256, end: tm(0)}})
	add("bad signature", issue(t, inter, nil, func() []sct { b := ok(p256); b.corrupt = true; return []sct{b} }(), nil), 1, logs)
	add("wrong issuer key hashed", issue(t, inter, nil, func() []sct { b := ok(p256); b.issuerKey = rootCA.cert.RawSubjectPublicKeyInfo; return []sct{b} }(), nil), 1, logs)
	add("P-384 log", issue(t, inter, nil, []sct{ok(p384)}, nil), 1, []logSpec{{key: p384}})
	add("RSA 2048 log", issue(t, inter, nil, []sct{okRSA(rsa2048)}, nil), 1, []logSpec{{key: rsa2048}})
	add("RSA 1024 log", issue(t, inter, nil, []sct{okRSA(rsa1024)}, nil), 1, []logSpec{{key: rsa1024}})
	add("Ed25519 log", issue(t, inter, nil, []sct{{log: ed, ts: ts, hash: 4, sigAlg: 7}}, nil), 1, []logSpec{{key: ed}})
	add("RSA signature claimed by an ECDSA log", issue(t, inter, nil, []sct{{log: p256, ts: ts, hash: 4, sigAlg: 1}}, nil), 1, logs)
	add("version 2", issue(t, inter, nil, func() []sct { b := ok(p256); b.version = 1; return []sct{b} }(), nil), 1, logs)
	add("with extensions", issue(t, inter, nil, func() []sct { b := ok(p256); b.ext = []byte{1, 2, 3}; return []sct{b} }(), nil), 1, logs)
	for _, h := range []byte{1, 2, 3, 5, 6} {
		b := ok(p256)
		b.hash = h
		add("hash "+string(rune('0'+h)), issue(t, inter, nil, []sct{b}, nil), 1, logs)
	}
	add("hash 0", issue(t, inter, nil, []sct{{log: p256, ts: ts, hash: 0, sigAlg: 3}}, nil), 1, logs)
	add("hash 7", issue(t, inter, nil, []sct{{log: p256, ts: ts, hash: 7, sigAlg: 3}}, nil), 1, logs)
	add("RSA over SHA-1", issue(t, inter, nil, []sct{{log: rsa2048, ts: ts, hash: 2, sigAlg: 1}}, nil), 1, []logSpec{{key: rsa2048}})
	add("P1363 ECDSA signature", issue(t, inter, nil, func() []sct { b := ok(p256); b.p1363 = true; return []sct{b} }(), nil), 1, logs)
	add("ECDSA signature with garbage after it", issue(t, inter, nil, func() []sct { b := ok(p256); b.garbage = true; return []sct{b} }(), nil), 1, logs)
	add("timestamp 0", issue(t, inter, nil, []sct{{log: p256, ts: 0, hash: 4, sigAlg: 3}}, nil), 1, logs)
	add("timestamp max", issue(t, inter, nil, []sct{{log: p256, ts: ^uint64(0), hash: 4, sigAlg: 3}}, nil), 1, []logSpec{{key: p256, start: &base}})

	// The SCT list's encodings.
	octets := func(b []byte) []byte { return must(asn1.Marshal(b)) }
	list := func(f func([][]byte) []byte) *x509.Certificate { return issue(t, inter, nil, []sct{ok(p256)}, f) }
	add("list not an OCTET STRING", list(func(s [][]byte) []byte { return must(asn1.Marshal([]int{1, 2})) }), 1, logs)
	add("list OCTET STRING then trailing data", list(func(s [][]byte) []byte { return append(sctListExt(s), 0) }), 1, logs)
	add("empty TLS list", list(func(s [][]byte) []byte { return octets([]byte{0, 0}) }), 1, logs)
	add("TLS list truncated length", list(func(s [][]byte) []byte { return octets([]byte{0}) }), 1, logs)
	add("TLS list longer than its octets", list(func(s [][]byte) []byte { return octets([]byte{0, 9, 0, 1, 0}) }), 1, logs)
	add("TLS list too long for its maximum", list(func(s [][]byte) []byte { return octets(append([]byte{0xff, 0x40}, make([]byte, 0xff40)...)) }), 1, logs)
	add("TLS list then trailing data", list(func(s [][]byte) []byte {
		l := []byte{}
		for _, x := range s {
			l = append(append(l, be16(len(x))...), x...)
		}
		return octets(append(append(be16(len(l)), l...), 7))
	}), 1, logs)
	add("empty SCT in the list", list(func(s [][]byte) []byte { return octets([]byte{0, 2, 0, 0}) }), 1, logs)
	add("SCT Val truncated", list(func(s [][]byte) []byte { return octets([]byte{0, 3, 0, 9, 1}) }), 1, logs)
	sctRaw := func(name string, raw []byte) {
		add(name, issue(t, inter, nil, []sct{{raw: raw}}, nil), 1, logs)
	}
	good := sct{log: p256, ts: ts, hash: 4, sigAlg: 3}.serialize([]byte{1}, nil)
	sctRaw("SCT truncated in its version", []byte{})
	sctRaw("SCT truncated in its log ID", good[:10])
	sctRaw("SCT truncated in its timestamp", good[:36])
	sctRaw("SCT truncated in its extensions' length", good[:42])
	sctRaw("SCT truncated in its extensions", append(append([]byte{}, good[:41]...), 0, 5, 1))
	sctRaw("SCT truncated in its hash", good[:43])
	sctRaw("SCT truncated in its signature algorithm", good[:44])
	sctRaw("SCT truncated in its signature's length", good[:45])
	sctRaw("SCT truncated in its signature", good[:len(good)-1])
	sctRaw("SCT with data after it", append(append([]byte{}, good...), 1, 2))
	add("the second SCT malformed", issue(t, inter, nil, []sct{ok(p256), {raw: good[:20]}}, nil), 1, logs)

	// What CT's x509 makes of the leaf and its issuer.
	ext := func(oid asn1.ObjectIdentifier, v []byte) []pkix.Extension {
		return []pkix.Extension{{Id: oid, Value: v}}
	}
	oidAIA := asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 1}
	oidSIA := asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 11}
	emptySeq := must(asn1.Marshal([]asn1.RawValue{}))
	leafWith := func(name string, extra []pkix.Extension) {
		add(name, issue(t, inter, extra, []sct{ok(p256)}, nil), 1, logs)
	}
	leafWith("leaf: empty AuthorityInfoAccess", ext(oidAIA, emptySeq))
	leafWith("leaf: AuthorityInfoAccess without a location", ext(oidAIA, must(asn1.Marshal([]struct{ M asn1.ObjectIdentifier }{{M: asn1.ObjectIdentifier{1, 2, 3}}}))))
	leafWith("leaf: AuthorityInfoAccess then trailing data", ext(oidAIA, append(must(asn1.Marshal([]struct {
		M asn1.ObjectIdentifier
		L asn1.RawValue
	}{{M: asn1.ObjectIdentifier{1, 2, 3}, L: asn1.RawValue{Class: 2, Tag: 6, Bytes: []byte("http://x")}}})), 0)))
	leafWith("leaf: empty SubjectInfoAccess", ext(oidSIA, emptySeq))
	leafWith("leaf: SubjectInfoAccess not a sequence", ext(oidSIA, []byte{4, 0}))
	leafWith("leaf: KeyUsage then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 15}, []byte{3, 2, 7, 0x80, 0}))
	leafWith("leaf: ExtendedKeyUsage then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 37}, append(must(asn1.Marshal([]asn1.ObjectIdentifier{{1, 3, 6, 1, 5, 5, 7, 3, 3}})), 5, 0)))
	leafWith("leaf: BasicConstraints then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 19}, []byte{0x30, 0, 0}))
	leafWith("leaf: SubjectKeyId then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 14}, []byte{4, 1, 9, 0}))
	leafWith("leaf: AuthorityKeyId then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 35}, append(must(asn1.Marshal(struct {
		ID []byte `asn1:"optional,tag:0"`
	}{ID: inter.cert.SubjectKeyId})), 0)))
	leafWith("leaf: certificate policies then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 32}, append(must(asn1.Marshal([]struct{ P asn1.ObjectIdentifier }{{P: asn1.ObjectIdentifier{1, 2, 3}}})), 0)))
	leafWith("leaf: SAN with a universal OID element", []pkix.Extension{{Id: asn1.ObjectIdentifier{2, 5, 29, 17}, Value: must(asn1.Marshal([]asn1.RawValue{
		{Class: 2, Tag: 6, Bytes: []byte("https://github.com/x")},
		{Class: 0, Tag: 6, Bytes: []byte("%zz")},
	}))}})
	leafWith("leaf: SAN with a universal tag-7 element", []pkix.Extension{{Id: asn1.ObjectIdentifier{2, 5, 29, 17}, Value: must(asn1.Marshal([]asn1.RawValue{
		{Class: 2, Tag: 6, Bytes: []byte("https://github.com/x")},
		{Class: 0, Tag: 7, Bytes: []byte("abc")},
	}))}})
	leafWith("leaf: SAN then trailing data", []pkix.Extension{{Id: asn1.ObjectIdentifier{2, 5, 29, 17}, Value: append(must(asn1.Marshal([]asn1.RawValue{
		{Class: 2, Tag: 6, Bytes: []byte("https://github.com/x")},
	})), 0)}})
	leafWith("leaf: CRL distribution points then trailing data", ext(asn1.ObjectIdentifier{2, 5, 29, 31}, append(emptySeq, 0)))
	leafWith("leaf: CRL distribution point with a bad reason", ext(asn1.ObjectIdentifier{2, 5, 29, 31}, []byte{0x30, 4, 0x30, 2, 0x81, 0}))
	leafWith("leaf: empty IP address blocks", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 7}, emptySeq))
	leafWith("leaf: IP address block of a bad family", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 7}, []byte{0x30, 7, 0x30, 5, 4, 1, 1, 5, 0}))
	leafWith("leaf: IP address block of an unexpected type", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 7}, []byte{0x30, 11, 0x30, 9, 4, 2, 0, 1, 0x30, 3, 2, 1, 5}))
	leafWith("leaf: IP address blocks not a sequence", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 7}, []byte{4, 0}))
	leafWith("leaf: AS identifiers not a sequence", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 8}, []byte{4, 0}))
	leafWith("leaf: AS identifiers of an unexpected type", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 8}, []byte{0x30, 7, 0xa0, 5, 0x30, 3, 1, 1, 0xff}))
	leafWith("leaf: AS identifiers inheriting", ext(asn1.ObjectIdentifier{1, 3, 6, 1, 5, 5, 7, 1, 8}, []byte{0x30, 4, 0xa0, 2, 5, 0}))
	leafWith("leaf: AIA and empty SIA, two non-fatal errors", append(ext(oidAIA, emptySeq), ext(oidSIA, emptySeq)...))

	// Name constraints, in the issuer and in the leaf, as CT's x509 reads them.
	constrained := func(name string, extra []pkix.Extension, tweak func(*x509.Certificate)) {
		issuer := newCAWith(t, "constrained intermediate", rootCA, extra, tweak)
		leaf := issue(t, issuer, nil, []sct{ok(p256)}, nil)
		cases = append(cases, run(t, name, leaf, []*x509.Certificate{issuer.cert}, roots, 1, logs))
	}
	_, ipNet4 := must2(net.ParseCIDR("10.0.0.0/8"))
	_, ipNet6 := must2(net.ParseCIDR("2001:db8::/32"))
	constrained("issuer: permitted URI domain", nil, func(c *x509.Certificate) { c.PermittedURIDomains = []string{"github.com"} })
	constrained("issuer: permitted URI subdomains", nil, func(c *x509.Certificate) { c.PermittedURIDomains = []string{".com", "github.com"} })
	constrained("issuer: excluded URI domain", nil, func(c *x509.Certificate) { c.ExcludedURIDomains = []string{".example.com"} })
	constrained("issuer: DNS domains, critical", nil, func(c *x509.Certificate) {
		c.PermittedDNSDomainsCritical = true
		c.PermittedDNSDomains = []string{"example.com", ".example.org"}
		c.ExcludedDNSDomains = []string{"bad.example.com"}
	})
	constrained("issuer: email constraints", nil, func(c *x509.Certificate) {
		c.PermittedEmailAddresses = []string{"a@example.com", "example.org", ".example.net", "\"a b\"@example.com", "a\\.b@example.com"}
	})
	constrained("issuer: IP ranges", nil, func(c *x509.Certificate) {
		c.PermittedIPRanges = []*net.IPNet{ipNet4}
		c.ExcludedIPRanges = []*net.IPNet{ipNet6}
	})
	// A directoryName ([4]) subtree, which neither parser reads.
	dirName := must(asn1.Marshal(pkix.Name{CommonName: "x"}.ToRDNSequence()))
	ncDir := []byte{0x30, byte(6 + len(dirName)), 0xa0, byte(4 + len(dirName)), 0x30, byte(2 + len(dirName)), 0xa4, byte(len(dirName))}
	ncDir = append(ncDir, dirName...)
	constrained("issuer: a directoryName subtree", []pkix.Extension{{Id: asn1.ObjectIdentifier{2, 5, 29, 30}, Value: ncDir}}, nil)
	leafWith("leaf: name constraints of its own", []pkix.Extension{{Id: asn1.ObjectIdentifier{2, 5, 29, 30}, Value: []byte{
		0x30, 0x0f, 0xa0, 0x0d, 0x30, 0x0b, 0x86, 0x09, 'g', 'i', 't', 'h', 'u', 'b', '.', 'i', 'o',
	}}})

	// An issuer CT's x509 refuses skips its chain.
	badInter := &ca{key: inter.key}
	badInter.cert = func() *x509.Certificate {
		tmpl := *inter.cert
		tmpl.ExtraExtensions = ext(oidAIA, emptySeq)
		der := must(x509.CreateCertificate(rand.Reader, &tmpl, rootCA.cert, inter.key.Public(), rootCA.key))
		return must(x509.ParseCertificate(der))
	}()
	leaf := issue(t, inter, nil, []sct{ok(p256)}, nil)
	cases = append(cases, run(t, "issuer with an empty AIA", leaf, []*x509.Certificate{badInter.cert}, roots, 1, logs))
	cases = append(cases, run(t, "two issuers, the first refused", leaf, []*x509.Certificate{badInter.cert, inter.cert}, roots, 1, logs))
	cases = append(cases, run(t, "two issuers, the second refused", leaf, []*x509.Certificate{inter.cert, badInter.cert}, roots, 1, logs))

	data := must(json.MarshalIndent(cases, "", "  "))
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	_ = ct.V1
	_ = ctx509.ParseCertificate
}
