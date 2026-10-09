package zzshardssigstore

// sigstore-go v1.2.2's answers (as buildx v0.37.1 vendors it) for
// crates/sigstore/tests/oracle.rs: Sigstore bundles made here against a virtual Sigstore
// (Fulcio-like CA, CT log, Rekor v1 and v2 logs, timestamping authority) built with only
// what buildx vendors, and the real moby/buildkit v0.28.1 attestation bundle against
// Sigstore's own trusted root; each verified as BuildKit's policy helpers verify
// (moby/policy-helpers verifier.go: VerifyArtifact, VerifyImage, and its DHI material),
// the bundle loaded as their loadBundle loads it. `generate` copies this file into
// buildx's cmd/zz_shards_sigstore and runs it there, with TZ=UTC.
//
// Construction:
//   - Every key is derived from a fixed seed (ECDSA scalars and Ed25519 seeds from
//     SHA-256 of a name; the RSA key from a seeded reader), and crypto's randomness is
//     fixed by testing/cryptotest.SetGlobalRandom, so ECDSA signatures, certificates and
//     TSA serials repeat run to run. The CMS signingTime digitorus/pkcs7 adds to an RFC
//     3161 token is time.Now() and is the one part that changes per run.
//   - Times are fixed: T0 (2026-01-15T12:00:00Z) is every entry's integrated time and
//     every timestamp's time unless a case moves it; certificates are valid around it.
//   - Rekor v1 bodies are made by rekor's own types (types.UnmarshalEntry of a proposed
//     entry, then Canonicalize), SETs over the JCS of {body, integratedTime, logID,
//     logIndex} as rekor signs them, inclusion proofs from an RFC 6962 tree of 7 leaves,
//     checkpoints by rekor's util.CreateAndSignCheckpoint. Rekor v2 bodies are
//     rekor-tiles' Entry (protojson, JCS), their leaves hashedrekord.ToEntryHash's, the
//     checkpoint a c2sp note signed by rekor-tiles' note signer. SCTs are signed over
//     certificate-transparency-go's MerkleTreeLeafForEmbeddedSCT of the leaf before its
//     SCT list is added; tokens are digitorus/timestamp's.

import (
	"bytes"
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/sha512"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/asn1"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"math/big"
	"net/url"
	"os"
	"sort"
	"strings"
	"testing"
	"testing/cryptotest"
	"time"

	"github.com/cyberphone/json-canonicalization/go/src/webpki.org/jsoncanonicalizer"
	"github.com/digitorus/timestamp"
	"github.com/go-openapi/strfmt"
	ct "github.com/google/certificate-transparency-go"
	cttls "github.com/google/certificate-transparency-go/tls"
	ctx509 "github.com/google/certificate-transparency-go/x509"
	"github.com/google/certificate-transparency-go/x509util"
	"github.com/secure-systems-lab/go-securesystemslib/dsse"
	protobundle "github.com/sigstore/protobuf-specs/gen/pb-go/bundle/v1"
	protocommon "github.com/sigstore/protobuf-specs/gen/pb-go/common/v1"
	prototrustroot "github.com/sigstore/protobuf-specs/gen/pb-go/trustroot/v1"
	rekortilespb "github.com/sigstore/rekor-tiles/v2/pkg/generated/protobuf"
	tilesnote "github.com/sigstore/rekor-tiles/v2/pkg/note"
	"github.com/sigstore/rekor-tiles/v2/pkg/types/hashedrekord"
	"github.com/sigstore/rekor/pkg/generated/models"
	rekortypes "github.com/sigstore/rekor/pkg/types"
	_ "github.com/sigstore/rekor/pkg/types/dsse/v0.0.1"
	hashedrekord001 "github.com/sigstore/rekor/pkg/types/hashedrekord/v0.0.1"
	_ "github.com/sigstore/rekor/pkg/types/intoto/v0.0.2"
	rekorutil "github.com/sigstore/rekor/pkg/util"
	"github.com/sigstore/sigstore-go/pkg/bundle"
	"github.com/sigstore/sigstore-go/pkg/fulcio/certificate"
	"github.com/sigstore/sigstore-go/pkg/root"
	"github.com/sigstore/sigstore-go/pkg/verify"
	"github.com/sigstore/sigstore/pkg/signature"
	"github.com/transparency-dev/merkle/rfc6962"
	sumdbnote "golang.org/x/mod/sumdb/note"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/types/known/timestamppb"
)

// ---------------------------------------------------------------- the case file

type ocConfig struct {
	Tlog       int  `json:"tlog"`
	Observer   int  `json:"observer"`
	SCT        int  `json:"sct"`
	Signed     int  `json:"signed"`
	Integrated int  `json:"integrated"`
	NoObserver bool `json:"noObserver"`
}

type ocDigest struct {
	Alg string `json:"alg"`
	Hex string `json:"hex"`
}

type ocPolicy struct {
	Digest   *ocDigest `json:"digest"`
	Identity string    `json:"identity"` // "any" or "unsafe"
}

type ocKey struct {
	PEM       string `json:"pem"`
	ValidFrom int64  `json:"validFrom"`
}

type ocTimestamp struct {
	Type  string `json:"type"`
	URI   string `json:"uri"`
	Secs  int64  `json:"secs"`
	Nanos int    `json:"nanos"`
}

type ocSubject struct {
	Name    string      `json:"name"`
	Digests [][2]string `json:"digests"`
}

type ocStatement struct {
	PredicateType string      `json:"predicateType"`
	Subjects      []ocSubject `json:"subjects"`
}

type ocResult struct {
	Certificate map[string]string `json:"certificate"`
	PublicKeyID *string           `json:"publicKeyId"`
	Timestamps  []ocTimestamp     `json:"timestamps"`
	Statement   *ocStatement      `json:"statement"`
}

type ocCase struct {
	Name        string    `json:"name"`
	Root        string    `json:"root"` // a key of the file's roots
	trustedRoot string    // the root itself
	Bundle      string    `json:"bundle"`
	Config      ocConfig  `json:"config"`
	Policy      ocPolicy  `json:"policy"`
	Key         *ocKey    `json:"key"`
	Fulcio      bool      `json:"fulcio"`
	Now         int64     `json:"now"`
	Error       string    `json:"error"`
	Result      *ocResult `json:"result"`
}

// The policy helpers' verifier options.
var (
	cfgArtifact = ocConfig{Tlog: 1, Observer: 1, SCT: 1}
	cfgDHINoObs = ocConfig{NoObserver: true}
	cfgDHITlog  = ocConfig{Tlog: 1, Observer: 1}
)

// ---------------------------------------------------------------- the virtual Sigstore

var t0 = time.Date(2026, 1, 15, 12, 0, 0, 0, time.UTC)

func seed(name string) []byte {
	s := sha256.Sum256([]byte("shards sigstore oracle " + name))
	return s[:]
}

func ecKey(t *testing.T, name string, c elliptic.Curve) *ecdsa.PrivateKey {
	n := c.Params().N
	d := new(big.Int).SetBytes(seed(name))
	d.Mod(d, new(big.Int).Sub(n, big.NewInt(1)))
	d.Add(d, big.NewInt(1))
	b := make([]byte, (c.Params().BitSize+7)/8)
	d.FillBytes(b)
	k, err := ecdsa.ParseRawPrivateKey(c, b)
	if err != nil {
		t.Fatal(err)
	}
	return k
}

func edKey(name string) ed25519.PrivateKey {
	return ed25519.NewKeyFromSeed(seed(name))
}

var rsaOnce *rsa.PrivateKey

func rsaKey(t *testing.T) *rsa.PrivateKey {
	if rsaOnce == nil {
		k, err := rsa.GenerateKey(rand.Reader, 2048)
		if err != nil {
			t.Fatal(err)
		}
		rsaOnce = k
	}
	return rsaOnce
}

func pkix_(t *testing.T, pub crypto.PublicKey) []byte {
	b, err := x509.MarshalPKIXPublicKey(pub)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func keyPEM(t *testing.T, pub crypto.PublicKey) []byte {
	return pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: pkix_(t, pub)})
}

func certPEM(c *x509.Certificate) []byte {
	return pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: c.Raw})
}

func sha(b []byte) []byte {
	s := sha256.Sum256(b)
	return s[:]
}

var serialN int64 = 1000

func mkcert(t *testing.T, tmpl *x509.Certificate, parent *x509.Certificate, pub crypto.PublicKey, priv crypto.Signer) *x509.Certificate {
	serialN++
	if tmpl.SerialNumber == nil {
		tmpl.SerialNumber = big.NewInt(serialN)
	}
	if parent == nil {
		parent = tmpl
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, parent, pub, priv)
	if err != nil {
		t.Fatal(err)
	}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

type ca struct {
	root, inter       *x509.Certificate
	rootKey, interKey crypto.Signer
}

// A Fulcio-like CA: a root and an intermediate, CodeSigning in the intermediate's EKU
// as Fulcio's has it.
func newCA(t *testing.T, name string, notBefore, notAfter time.Time) ca {
	rk := ecKey(t, name+" root", elliptic.P384())
	ik := ecKey(t, name+" intermediate", elliptic.P384())
	root := mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{Organization: []string{"shards.test"}, CommonName: name},
		NotBefore:             notBefore,
		NotAfter:              notAfter,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		BasicConstraintsValid: true,
		IsCA:                  true,
		MaxPathLen:            1,
	}, nil, rk.Public(), rk)
	inter := mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{Organization: []string{"shards.test"}, CommonName: name + "-intermediate"},
		NotBefore:             notBefore,
		NotAfter:              notAfter,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning},
		BasicConstraintsValid: true,
		IsCA:                  true,
		MaxPathLenZero:        true,
	}, root, ik.Public(), rk)
	return ca{root: root, inter: inter, rootKey: rk, interKey: ik}
}

type world struct {
	t            *testing.T
	fulcio       ca
	rogue        ca
	ctKey        *ecdsa.PrivateKey
	ctOther      *ecdsa.PrivateKey
	rekor        *ecdsa.PrivateKey
	rekorV2      ed25519.PrivateKey
	tsaRoot      *x509.Certificate
	tsaLeaf      *x509.Certificate
	tsaKey       *ecdsa.PrivateKey
	rogueTSALeaf *x509.Certificate
	rogueTSAKey  *ecdsa.PrivateKey
	rootJSON     string
}

const (
	rekorURL   = "https://rekor.shards.test"
	rekorV2URL = "https://log2026.rekor.shards.test"
	tsaURL     = "https://tsa.shards.test/api/v1/timestamp"
	fulcioURL  = "https://fulcio.shards.test"
	ctURL      = "https://ct.shards.test"
	rekorTree  = int64(1193050959748727045)
)

func newWorld(t *testing.T) *world {
	w := &world{t: t}
	w.fulcio = newCA(t, "shards-fulcio", t0.AddDate(-1, 0, 0), t0.AddDate(4, 0, 0))
	w.rogue = newCA(t, "rogue-fulcio", t0.AddDate(-1, 0, 0), t0.AddDate(4, 0, 0))
	w.ctKey = ecKey(t, "ct", elliptic.P256())
	w.ctOther = ecKey(t, "ct other", elliptic.P256())
	w.rekor = ecKey(t, "rekor", elliptic.P256())
	w.rekorV2 = edKey("rekor v2")
	tsaRootKey := ecKey(t, "tsa root", elliptic.P384())
	w.tsaKey = ecKey(t, "tsa leaf", elliptic.P256())
	w.tsaRoot = mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{Organization: []string{"shards.test"}, CommonName: "shards-tsa-root"},
		NotBefore:             t0.AddDate(-1, 0, 0),
		NotAfter:              t0.AddDate(10, 0, 0),
		KeyUsage:              x509.KeyUsageCertSign,
		BasicConstraintsValid: true,
		IsCA:                  true,
	}, nil, tsaRootKey.Public(), tsaRootKey)
	w.tsaLeaf = mkcert(t, tsaLeafTemplate("shards-tsa"), w.tsaRoot, w.tsaKey.Public(), tsaRootKey)
	rogueRootKey := ecKey(t, "rogue tsa root", elliptic.P384())
	rogueRoot := mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{CommonName: "rogue-tsa-root"},
		NotBefore:             t0.AddDate(-1, 0, 0),
		NotAfter:              t0.AddDate(10, 0, 0),
		KeyUsage:              x509.KeyUsageCertSign,
		BasicConstraintsValid: true,
		IsCA:                  true,
	}, nil, rogueRootKey.Public(), rogueRootKey)
	w.rogueTSAKey = ecKey(t, "rogue tsa leaf", elliptic.P256())
	w.rogueTSALeaf = mkcert(t, tsaLeafTemplate("rogue-tsa"), rogueRoot, w.rogueTSAKey.Public(), rogueRootKey)
	w.rootJSON = w.trustedRoot(nil)
	return w
}

func tsaLeafTemplate(cn string) *x509.Certificate {
	return &x509.Certificate{
		Subject:   pkix.Name{Organization: []string{"shards.test"}, CommonName: cn},
		NotBefore: t0.AddDate(-1, 0, 0),
		NotAfter:  t0.AddDate(10, 0, 0),
		KeyUsage:  x509.KeyUsageDigitalSignature,
		ExtraExtensions: []pkix.Extension{{
			Id:       asn1.ObjectIdentifier{2, 5, 29, 37},
			Critical: true,
			Value:    mustASN1([]asn1.ObjectIdentifier{{1, 3, 6, 1, 5, 5, 7, 3, 8}}),
		}},
	}
}

func mustASN1(v any) []byte {
	b, err := asn1.Marshal(v)
	if err != nil {
		panic(err)
	}
	return b
}

func tr(start time.Time, end *time.Time) *protocommon.TimeRange {
	r := &protocommon.TimeRange{Start: timestamppb.New(start)}
	if end != nil {
		r.End = timestamppb.New(*end)
	}
	return r
}

// rootEdit lets a case change the trusted root before it is written.
type rootEdit func(r *prototrustroot.TrustedRoot)

func (w *world) trustedRoot(edit rootEdit) string {
	t := w.t
	start := t0.AddDate(-1, 0, 0)
	r := &prototrustroot.TrustedRoot{
		MediaType: root.TrustedRootMediaType01,
		Tlogs: []*prototrustroot.TransparencyLogInstance{{
			BaseUrl:       rekorURL,
			HashAlgorithm: protocommon.HashAlgorithm_SHA2_256,
			PublicKey: &protocommon.PublicKey{
				RawBytes:   pkix_(t, w.rekor.Public()),
				KeyDetails: protocommon.PublicKeyDetails_PKIX_ECDSA_P256_SHA_256,
				ValidFor:   tr(start, nil),
			},
			LogId: &protocommon.LogId{KeyId: sha(pkix_(t, w.rekor.Public()))},
		}, {
			BaseUrl:       rekorV2URL,
			HashAlgorithm: protocommon.HashAlgorithm_SHA2_256,
			PublicKey: &protocommon.PublicKey{
				RawBytes:   pkix_(t, w.rekorV2.Public()),
				KeyDetails: protocommon.PublicKeyDetails_PKIX_ED25519,
				ValidFor:   tr(start, nil),
			},
			LogId: &protocommon.LogId{KeyId: sha(pkix_(t, w.rekorV2.Public()))},
		}},
		CertificateAuthorities: []*prototrustroot.CertificateAuthority{{
			Subject: &protocommon.DistinguishedName{Organization: "shards.test", CommonName: "shards-fulcio"},
			Uri:     fulcioURL,
			CertChain: &protocommon.X509CertificateChain{Certificates: []*protocommon.X509Certificate{
				{RawBytes: w.fulcio.inter.Raw}, {RawBytes: w.fulcio.root.Raw},
			}},
			ValidFor: tr(start, nil),
		}},
		Ctlogs: []*prototrustroot.TransparencyLogInstance{{
			BaseUrl:       ctURL,
			HashAlgorithm: protocommon.HashAlgorithm_SHA2_256,
			PublicKey: &protocommon.PublicKey{
				RawBytes:   pkix_(t, w.ctKey.Public()),
				KeyDetails: protocommon.PublicKeyDetails_PKIX_ECDSA_P256_SHA_256,
				ValidFor:   tr(start, nil),
			},
			LogId: &protocommon.LogId{KeyId: sha(pkix_(t, w.ctKey.Public()))},
		}},
		TimestampAuthorities: []*prototrustroot.CertificateAuthority{{
			Subject: &protocommon.DistinguishedName{Organization: "shards.test", CommonName: "shards-tsa"},
			Uri:     tsaURL,
			CertChain: &protocommon.X509CertificateChain{Certificates: []*protocommon.X509Certificate{
				{RawBytes: w.tsaLeaf.Raw}, {RawBytes: w.tsaRoot.Raw},
			}},
			ValidFor: tr(start, nil),
		}},
	}
	if edit != nil {
		edit(r)
	}
	b, err := protojson.Marshal(r)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

// ---------------------------------------------------------------- signers and leaves

type signer struct {
	priv crypto.Signer
	pub  crypto.PublicKey
	kind string // p256, p384, ed25519, rsa
}

func (w *world) signer(kind string) signer {
	switch kind {
	case "p384":
		k := ecKey(w.t, "leaf p384", elliptic.P384())
		return signer{k, k.Public(), kind}
	case "ed25519":
		k := edKey("leaf ed25519")
		return signer{k, k.Public(), kind}
	case "rsa":
		k := rsaKey(w.t)
		return signer{k, k.Public(), kind}
	case "p256-other":
		k := ecKey(w.t, "leaf other", elliptic.P256())
		return signer{k, k.Public(), "p256"}
	}
	k := ecKey(w.t, "leaf p256", elliptic.P256())
	return signer{k, k.Public(), "p256"}
}

// sign signs a message as the bundle's signer would: ECDSA and RSA over SHA-256
// (hashP384 picks SHA-384 for P-384, the default verifier's hash), Ed25519 over the
// message, or Ed25519ph over its SHA-512 where ph.
func (s signer) sign(t *testing.T, msg []byte, ph bool, hashP384 bool) []byte {
	switch k := s.priv.(type) {
	case ed25519.PrivateKey:
		if ph {
			d := sha512.Sum512(msg)
			sig, err := k.Sign(rand.Reader, d[:], &ed25519.Options{Hash: crypto.SHA512})
			if err != nil {
				t.Fatal(err)
			}
			return sig
		}
		return ed25519.Sign(k, msg)
	case *rsa.PrivateKey:
		d := sha256.Sum256(msg)
		sig, err := rsa.SignPKCS1v15(rand.Reader, k, crypto.SHA256, d[:])
		if err != nil {
			t.Fatal(err)
		}
		return sig
	case *ecdsa.PrivateKey:
		var d []byte
		if hashP384 && s.kind == "p384" {
			x := sha512.Sum384(msg)
			d = x[:]
		} else {
			d = sha(msg)
		}
		sig, err := ecdsa.SignASN1(rand.Reader, k, d)
		if err != nil {
			t.Fatal(err)
		}
		return sig
	}
	t.Fatal("unknown signer")
	return nil
}

// leafSpec: what a Fulcio-like leaf certificate carries.
type leafSpec struct {
	ca           *ca
	notBefore    time.Time
	notAfter     time.Time
	san          string // "uri", "email", "none"
	noEKU        bool
	extsV1       bool   // the raw-string v1 extensions
	sct          string // "ok", "none", "badsig", "unknownlog", "late"
	badDERIssuer bool
}

var (
	oidIssuerV1  = asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 1}
	oidSCTList   = asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 11129, 2, 4, 2}
	fulcioV2Exts = []struct {
		n int
		v string
	}{
		{8, "https://token.actions.githubusercontent.com"},
		{9, "https://github.com/moby/buildkit/.github/workflows/build.yml@refs/tags/v0.28.1"},
		{10, "0123456789abcdef0123456789abcdef01234567"},
		{11, "github-hosted"},
		{12, "https://github.com/moby/buildkit"},
		{13, "89abcdef0123456789abcdef0123456789abcdef"},
		{14, "refs/tags/v0.28.1"},
		{15, "12345"},
		{16, "https://github.com/moby"},
		{17, "678"},
		{18, "https://github.com/moby/buildkit/.github/workflows/build.yml@refs/tags/v0.28.1"},
		{19, "0123456789abcdef0123456789abcdef01234567"},
		{20, "push"},
		{21, "https://github.com/moby/buildkit/actions/runs/1/attempts/1"},
		{22, "public"},
	}
)

func (w *world) leaf(s signer, spec leafSpec) *x509.Certificate {
	t := w.t
	c := spec.ca
	if c == nil {
		c = &w.fulcio
	}
	if spec.notBefore.IsZero() {
		spec.notBefore = t0.Add(-time.Minute)
	}
	if spec.notAfter.IsZero() {
		spec.notAfter = t0.Add(9 * time.Minute)
	}
	tmpl := &x509.Certificate{
		NotBefore: spec.notBefore,
		NotAfter:  spec.notAfter,
		KeyUsage:  x509.KeyUsageDigitalSignature,
	}
	if !spec.noEKU {
		tmpl.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning}
	}
	switch spec.san {
	case "email":
		tmpl.EmailAddresses = []string{"builder@shards.test"}
	case "othername":
		// A critical SAN of only an otherName, which crypto/x509 leaves unhandled and
		// sigstore-go then clears.
		other := mustASN1(struct {
			ID    asn1.ObjectIdentifier
			Value string `asn1:"utf8,explicit,tag:0"`
		}{asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 7}, "builder@shards.test"})
		other[0] = 0xa0
		tmpl.ExtraExtensions = append(tmpl.ExtraExtensions, pkix.Extension{
			Id: asn1.ObjectIdentifier{2, 5, 29, 17}, Critical: true, Value: mustASN1(asn1.RawValue{Tag: 16, IsCompound: true, Bytes: other}),
		})
	case "none":
	default:
		u, _ := url.Parse("https://github.com/moby/buildkit/.github/workflows/build.yml@refs/tags/v0.28.1")
		tmpl.URIs = []*url.URL{u}
	}
	if spec.extsV1 {
		tmpl.ExtraExtensions = append(tmpl.ExtraExtensions,
			pkix.Extension{Id: oidIssuerV1, Value: []byte("https://accounts.shards.test")},
			pkix.Extension{Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 2}, Value: []byte("push")},
			pkix.Extension{Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 3}, Value: []byte("abc123")},
			pkix.Extension{Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 4}, Value: []byte("build")},
			pkix.Extension{Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 5}, Value: []byte("moby/buildkit")},
			pkix.Extension{Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, 6}, Value: []byte("refs/tags/v0.28.1")},
		)
	} else {
		for _, e := range fulcioV2Exts {
			v := mustASN1(e.v)
			if spec.badDERIssuer && e.n == 8 {
				v = []byte(e.v)
			}
			tmpl.ExtraExtensions = append(tmpl.ExtraExtensions, pkix.Extension{
				Id: asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 1, e.n}, Value: v,
			})
		}
	}
	serialN++
	tmpl.SerialNumber = big.NewInt(serialN)
	if spec.sct == "none" {
		return mkcertSerial(t, tmpl, c.inter, s.pub, c.interKey)
	}
	// The SCT over the precertificate's TBS: this certificate with its SCT list taken
	// out, which MerkleTreeLeafForEmbeddedSCT takes out of one made with a placeholder.
	tmpl.ExtraExtensions = append(tmpl.ExtraExtensions, pkix.Extension{Id: oidSCTList, Value: mustASN1([]byte{0})})
	pre := mkcertSerial(t, tmpl, c.inter, s.pub, c.interKey)
	tmpl.ExtraExtensions = tmpl.ExtraExtensions[:len(tmpl.ExtraExtensions)-1]
	ctPre, err := ctx509.ParseCertificate(pre.Raw)
	if ctx509.IsFatal(err) {
		t.Fatal(err)
	}
	ctIssuer, err := ctx509.ParseCertificate(c.inter.Raw)
	if err != nil {
		t.Fatal(err)
	}
	logKey := w.ctKey
	if spec.sct == "unknownlog" {
		logKey = w.ctOther
	}
	ts := uint64(t0.UnixMilli())
	if spec.sct == "late" {
		ts = uint64(t0.AddDate(20, 0, 0).UnixMilli())
	}
	leafEntry, err := ct.MerkleTreeLeafForEmbeddedSCT([]*ctx509.Certificate{ctPre, ctIssuer}, ts)
	if err != nil {
		t.Fatal(err)
	}
	var id [32]byte
	copy(id[:], sha(pkix_(t, logKey.Public())))
	sct := ct.SignedCertificateTimestamp{
		SCTVersion: ct.V1,
		LogID:      ct.LogID{KeyID: id},
		Timestamp:  ts,
	}
	input, err := ct.SerializeSCTSignatureInput(sct, ct.LogEntry{Leaf: *leafEntry})
	if err != nil {
		t.Fatal(err)
	}
	if spec.sct == "badsig" {
		input = append(input, 'x')
	}
	sig, err := ecdsa.SignASN1(rand.Reader, logKey, sha(input))
	if err != nil {
		t.Fatal(err)
	}
	sct.Signature = ct.DigitallySigned{
		Algorithm: cttls.SignatureAndHashAlgorithm{Hash: cttls.SHA256, Signature: cttls.ECDSA},
		Signature: sig,
	}
	list, err := x509util.MarshalSCTsIntoSCTList([]*ct.SignedCertificateTimestamp{&sct})
	if err != nil {
		t.Fatal(err)
	}
	tlsList, err := cttls.Marshal(*list)
	if err != nil {
		t.Fatal(err)
	}
	tmpl.ExtraExtensions = append(tmpl.ExtraExtensions, pkix.Extension{Id: oidSCTList, Value: mustASN1(tlsList)})
	return mkcertSerial(t, tmpl, c.inter, s.pub, c.interKey)
}

func mkcertSerial(t *testing.T, tmpl, parent *x509.Certificate, pub crypto.PublicKey, priv crypto.Signer) *x509.Certificate {
	der, err := x509.CreateCertificate(rand.Reader, tmpl, parent, pub, priv)
	if err != nil {
		t.Fatal(err)
	}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// ---------------------------------------------------------------- transparency logs

// An RFC 6962 tree of seven leaves, ours at `index`, the rest fixed.
func tree(leaf []byte, index int) (root []byte, proof [][]byte) {
	leaves := make([][]byte, 7)
	for i := range leaves {
		if i == index {
			leaves[i] = leaf
		} else {
			leaves[i] = rfc6962.DefaultHasher.HashLeaf([]byte(fmt.Sprintf("other leaf %d", i)))
		}
	}
	return mth(leaves), path(index, leaves)
}

func mth(l [][]byte) []byte {
	if len(l) == 1 {
		return l[0]
	}
	k := split(len(l))
	return rfc6962.DefaultHasher.HashChildren(mth(l[:k]), mth(l[k:]))
}

func split(n int) int {
	k := 1
	for k*2 < n {
		k *= 2
	}
	return k
}

func path(m int, l [][]byte) [][]byte {
	if len(l) <= 1 {
		return nil
	}
	k := split(len(l))
	if m < k {
		return append(path(m, l[:k]), mth(l[k:]))
	}
	return append(path(m-k, l[k:]), mth(l[:k]))
}

// entrySpec: one transparency log entry.
type entrySpec struct {
	kind           string // "dsse", "intoto", "hashedrekord", "v2"
	noPromise      bool
	noProof        bool
	badSET         bool
	badProof       bool
	badRoot        bool // proof's root hash not the checkpoint's
	badCheckpoint  bool
	unknownLog     bool
	integrated     time.Time
	logIndex       int64
	sigOverride    []byte // the entry's signature, not the bundle's
	keyOverride    crypto.PublicKey
	digestOverride []byte
	kindVersion    [2]string
	v2NoBaseURL    bool
}

// The material an entry is made from.
type signed struct {
	signer   signer
	cert     *x509.Certificate // nil for a public key
	keyHint  string
	envelope *dsse.Envelope // DSSE content
	digest   []byte         // message signature content: the artifact's SHA-256
	sig      []byte
}

func (w *world) pubPEM(s signed, override crypto.PublicKey) []byte {
	if override != nil {
		return keyPEM(w.t, override)
	}
	if s.cert != nil {
		return certPEM(s.cert)
	}
	return keyPEM(w.t, s.signer.pub)
}

func (w *world) body(s signed, e entrySpec) []byte {
	t := w.t
	ctx := context.Background()
	verifier := w.pubPEM(s, e.keyOverride)
	switch e.kind {
	case "hashedrekord":
		sig := s.sig
		if e.sigOverride != nil {
			sig = e.sigOverride
		}
		digest := s.digest
		if s.envelope != nil {
			payload, err := base64.StdEncoding.DecodeString(s.envelope.Payload)
			if err != nil {
				t.Fatal(err)
			}
			digest = sha(dsse.PAE(s.envelope.PayloadType, payload))
		}
		if e.digestOverride != nil {
			digest = e.digestOverride
		}
		alg := "sha256"
		h := hex.EncodeToString(digest)
		entry := &hashedrekord001.V001Entry{HashedRekordObj: models.HashedrekordV001Schema{
			Data: &models.HashedrekordV001SchemaData{Hash: &models.HashedrekordV001SchemaDataHash{Algorithm: &alg, Value: &h}},
			Signature: &models.HashedrekordV001SchemaSignature{
				Content:   sig,
				PublicKey: &models.HashedrekordV001SchemaSignaturePublicKey{Content: verifier},
			},
		}}
		b, err := entry.Canonicalize(ctx)
		if err != nil {
			t.Fatalf("%s: hashedrekord: %v", currentCase, err)
		}
		return b
	case "dsse":
		env := *s.envelope
		if e.sigOverride != nil {
			env.Signatures = []dsse.Signature{{Sig: base64.StdEncoding.EncodeToString(e.sigOverride)}}
		}
		envJSON, err := json.Marshal(env)
		if err != nil {
			t.Fatal(err)
		}
		envStr := string(envJSON)
		v := "0.0.1"
		pe := &models.DSSE{APIVersion: &v, Spec: &models.DSSEV001Schema{ProposedContent: &models.DSSEV001SchemaProposedContent{
			Envelope: &envStr, Verifiers: []strfmt.Base64{verifier},
		}}}
		impl, err := rekortypes.UnmarshalEntry(pe)
		if err != nil {
			t.Fatalf("%s: dsse: %v", currentCase, err)
		}
		b, err := impl.Canonicalize(ctx)
		if err != nil {
			t.Fatal(err)
		}
		return b
	case "intoto":
		env := *s.envelope
		envJSON, err := json.Marshal(env)
		if err != nil {
			t.Fatal(err)
		}
		v := "0.0.2"
		pt := env.PayloadType
		sigText := strfmt.Base64(env.Signatures[0].Sig)
		pk := strfmt.Base64(verifier)
		sha256s := "sha256"
		envHash := hex.EncodeToString(sha(envJSON))
		pe := &models.Intoto{APIVersion: &v, Spec: &models.IntotoV002Schema{Content: &models.IntotoV002SchemaContent{
			Envelope: &models.IntotoV002SchemaContentEnvelope{
				Payload:     strfmt.Base64(env.Payload),
				PayloadType: &pt,
				Signatures:  []*models.IntotoV002SchemaContentEnvelopeSignaturesItems0{{Sig: &sigText, PublicKey: &pk}},
			},
			Hash: &models.IntotoV002SchemaContentHash{Algorithm: &sha256s, Value: &envHash},
		}}}
		impl, err := rekortypes.UnmarshalEntry(pe)
		if err != nil {
			t.Fatalf("%s: intoto: %v", currentCase, err)
		}
		b, err := impl.Canonicalize(ctx)
		if err != nil {
			t.Fatal(err)
		}
		return b
	}
	t.Fatalf("unknown kind %s", e.kind)
	return nil
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// The v1 entry as the bundle carries it.
func (w *world) v1Entry(s signed, e entrySpec) map[string]any {
	t := w.t
	body := w.body(s, e)
	if e.integrated.IsZero() {
		e.integrated = t0
	}
	if e.logIndex == 0 {
		e.logIndex = 1180749977
	}
	logKey := w.rekor
	if e.unknownLog {
		logKey = ecKey(t, "rekor unknown", elliptic.P256())
	}
	logID := sha(pkix_(t, logKey.Public()))
	kv := e.kindVersion
	if kv[0] == "" {
		switch e.kind {
		case "intoto":
			kv = [2]string{"intoto", "0.0.2"}
		case "hashedrekord":
			kv = [2]string{"hashedrekord", "0.0.1"}
		default:
			kv = [2]string{"dsse", "0.0.1"}
		}
	}
	out := map[string]any{
		"logIndex":          fmt.Sprint(e.logIndex),
		"logId":             map[string]any{"keyId": b64(logID)},
		"kindVersion":       map[string]any{"kind": kv[0], "version": kv[1]},
		"integratedTime":    fmt.Sprint(e.integrated.Unix()),
		"canonicalizedBody": b64(body),
	}
	if !e.noPromise {
		payload := tlogPayload{Body: b64(body), IntegratedTime: e.integrated.Unix(), LogIndex: e.logIndex, LogID: hex.EncodeToString(logID)}
		j, err := json.Marshal(payload)
		if err != nil {
			t.Fatal(err)
		}
		canon, err := jsoncanonicalizer.Transform(j)
		if err != nil {
			t.Fatal(err)
		}
		if e.badSET {
			canon = append(canon, ' ')
		}
		set, err := ecdsa.SignASN1(rand.Reader, logKey, sha(canon))
		if err != nil {
			t.Fatal(err)
		}
		out["inclusionPromise"] = map[string]any{"signedEntryTimestamp": b64(set)}
	}
	if !e.noProof {
		const index = 3
		leaf := rfc6962.DefaultHasher.HashLeaf(body)
		rootHash, proof := tree(leaf, index)
		if e.badProof {
			proof[0] = sha([]byte("not a sibling"))
		}
		cpRoot := rootHash
		if e.badRoot {
			cpRoot = sha([]byte("another root"))
		}
		s, err := signature.LoadECDSASigner(logKey, crypto.SHA256)
		if err != nil {
			t.Fatal(err)
		}
		cp, err := rekorutil.CreateAndSignCheckpoint(context.Background(), "rekor.shards.test", rekorTree, 7, cpRoot, s)
		if err != nil {
			t.Fatal(err)
		}
		if e.badCheckpoint {
			cp = bytes.Replace(cp, []byte("\n7\n"), []byte("\n8\n"), 1)
		}
		hashes := []any{}
		for _, h := range proof {
			hashes = append(hashes, b64(h))
		}
		out["inclusionProof"] = map[string]any{
			"logIndex":   fmt.Sprint(index),
			"rootHash":   b64(rootHash),
			"treeSize":   "7",
			"hashes":     hashes,
			"checkpoint": map[string]any{"envelope": string(cp)},
		}
	}
	return out
}

type tlogPayload struct {
	Body           any    `json:"body"`
	IntegratedTime int64  `json:"integratedTime"`
	LogIndex       int64  `json:"logIndex"`
	LogID          string `json:"logID"` //nolint:tagliatelle
}

// A Rekor v2 entry: a hashedrekord v0.0.2 of the digest the signature is over.
func (w *world) v2Entry(s signed, e entrySpec) map[string]any {
	t := w.t
	var details protocommon.PublicKeyDetails
	var digest []byte
	switch s.signer.kind {
	case "ed25519":
		details = protocommon.PublicKeyDetails_PKIX_ED25519_PH
	case "p384":
		details = protocommon.PublicKeyDetails_PKIX_ECDSA_P384_SHA_384
	default:
		details = protocommon.PublicKeyDetails_PKIX_ECDSA_P256_SHA_256
	}
	if s.envelope != nil {
		payload, err := base64.StdEncoding.DecodeString(s.envelope.Payload)
		if err != nil {
			t.Fatal(err)
		}
		pae := dsse.PAE(s.envelope.PayloadType, payload)
		switch s.signer.kind {
		case "ed25519":
			d := sha512.Sum512(pae)
			digest = d[:]
		case "p384":
			d := sha512.Sum384(pae)
			digest = d[:]
		default:
			digest = sha(pae)
		}
	} else {
		digest = s.digest
	}
	if e.digestOverride != nil {
		digest = e.digestOverride
	}
	v := &rekortilespb.Verifier{KeyDetails: details}
	if s.cert != nil {
		v.Verifier = &rekortilespb.Verifier_X509Certificate{X509Certificate: &protocommon.X509Certificate{RawBytes: s.cert.Raw}}
	} else {
		v.Verifier = &rekortilespb.Verifier_PublicKey{PublicKey: &rekortilespb.PublicKey{RawBytes: pkix_(t, s.signer.pub)}}
	}
	sigContent := s.sig
	if e.sigOverride != nil {
		sigContent = e.sigOverride
	}
	pbSig := &rekortilespb.Signature{Content: sigContent, Verifier: v}
	leaf, err := hashedrekord.ToEntryHash(digest, pbSig)
	if err != nil {
		t.Fatal(err)
	}
	algDetails, err := signature.GetAlgorithmDetails(details)
	if err != nil {
		t.Fatal(err)
	}
	entry := &rekortilespb.Entry{Kind: "hashedrekord", ApiVersion: "0.0.2", Spec: &rekortilespb.Spec{Spec: &rekortilespb.Spec_HashedRekordV002{
		HashedRekordV002: &rekortilespb.HashedRekordLogEntryV002{
			Data:      &protocommon.HashOutput{Digest: digest, Algorithm: algDetails.GetProtoHashType()},
			Signature: pbSig,
		},
	}}}
	pj, err := protojson.Marshal(entry)
	if err != nil {
		t.Fatal(err)
	}
	body, err := jsoncanonicalizer.Transform(pj)
	if err != nil {
		t.Fatal(err)
	}
	index := int64(3)
	rootHash, proof := tree(leaf, int(index))
	if e.badProof {
		proof[0] = sha([]byte("not a sibling"))
	}
	ns, err := signature.LoadED25519Signer(w.rekorV2)
	if err != nil {
		t.Fatal(err)
	}
	origin := "log2026.rekor.shards.test"
	nsigner, err := tilesnote.NewNoteSigner(context.Background(), origin, ns)
	if err != nil {
		t.Fatal(err)
	}
	cpRoot := rootHash
	if e.badRoot {
		cpRoot = sha([]byte("another root"))
	}
	text := fmt.Sprintf("%s\n7\n%s\n", origin, b64(cpRoot))
	cp, err := sumdbnote.Sign(&sumdbnote.Note{Text: text}, nsigner)
	if err != nil {
		t.Fatal(err)
	}
	if e.badCheckpoint {
		cp = bytes.Replace(cp, []byte("\n7\n"), []byte("\n8\n"), 1)
	}
	hashes := []any{}
	for _, h := range proof {
		hashes = append(hashes, b64(h))
	}
	logID := sha(pkix_(t, w.rekorV2.Public()))
	return map[string]any{
		"logIndex":          fmt.Sprint(index),
		"logId":             map[string]any{"keyId": b64(logID)},
		"kindVersion":       map[string]any{"kind": "hashedrekord", "version": "0.0.2"},
		"canonicalizedBody": b64(body),
		"inclusionProof": map[string]any{
			"logIndex":   fmt.Sprint(index),
			"rootHash":   b64(rootHash),
			"treeSize":   "7",
			"hashes":     hashes,
			"checkpoint": map[string]any{"envelope": string(cp)},
		},
	}
}

// ---------------------------------------------------------------- timestamps

type tsaSpec struct {
	rogue   bool
	at      time.Time
	overSig []byte // the signature the token is over, where not the bundle's
	sha1    bool
	noCerts bool
}

func (w *world) token(sig []byte, s tsaSpec) string {
	t := w.t
	over := sig
	if s.overSig != nil {
		over = s.overSig
	}
	h := crypto.SHA256
	if s.sha1 {
		h = crypto.SHA384
	}
	hh := h.New()
	hh.Write(over)
	at := s.at
	if at.IsZero() {
		at = t0.Add(2 * time.Second)
	}
	ts := timestamp.Timestamp{
		HashAlgorithm:     h,
		HashedMessage:     hh.Sum(nil),
		Time:              at,
		Policy:            asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 2},
		AddTSACertificate: !s.noCerts,
	}
	leaf, key := w.tsaLeaf, w.tsaKey
	if s.rogue {
		leaf, key = w.rogueTSALeaf, w.rogueTSAKey
	}
	der, err := ts.CreateResponseWithOpts(leaf, key, crypto.SHA256)
	if err != nil {
		t.Fatal(err)
	}
	// The bundle carries the token: the response's TimeStampToken.
	var resp struct {
		Status asn1.RawValue
		Token  asn1.RawValue
	}
	if _, err := asn1.Unmarshal(der, &resp); err != nil {
		t.Fatal(err)
	}
	_ = resp
	return b64(der)
}

// ---------------------------------------------------------------- bundles

type bundleSpec struct {
	version           string // "0.1", "0.2", "0.3"
	mediaType         string // overrides version's
	material          string // "cert", "chain", "pubkey"
	content           string // "dsse", "msg"
	signerKind        string
	leaf              leafSpec
	hashP384          bool // P-384 signs over SHA-384 (else SHA-256: the compat path)
	ph                bool // Ed25519 message signatures over SHA-512 (Ed25519ph)
	entries           []entrySpec
	tsas              []tsaSpec
	predicate         string
	subjects          []map[string]any
	payloadType       string
	badSig            bool
	twoSigs           bool
	keyid             string // a DSSE signature's keyid; "fingerprint" for the key's own
	artifact          []byte
	msgDigestOverride []byte
	msgAlg            string
	chainExtra        []*x509.Certificate
	hint              string
	rawPayload        []byte    // the DSSE payload as given, not a statement
	cfg               *ocConfig // the verifier's options, where not VerifyArtifact's
}

type built struct {
	json   map[string]any
	signed signed
	digest []byte // the artifact's SHA-256
}

var artifactBytes = []byte("shards oracle artifact\n")

func (w *world) bundle(b bundleSpec) built {
	t := w.t
	if b.version == "" {
		b.version = "0.3"
	}
	if b.content == "" {
		b.content = "dsse"
	}
	if b.material == "" {
		b.material = "cert"
	}
	art := artifactBytes
	if b.artifact != nil {
		art = b.artifact
	}
	digest := sha(art)
	s := signed{signer: w.signer(b.signerKind), digest: digest}
	if b.material != "pubkey" {
		s.cert = w.leaf(s.signer, b.leaf)
	}
	out := map[string]any{}
	switch b.version {
	case "0.1", "0.2":
		out["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=" + b.version
	default:
		out["mediaType"] = "application/vnd.dev.sigstore.bundle.v" + b.version + "+json"
	}
	if b.mediaType != "" {
		out["mediaType"] = b.mediaType
	}
	if b.content == "dsse" {
		subjects := b.subjects
		if subjects == nil {
			subjects = []map[string]any{{"name": "artifact", "digest": map[string]any{"sha256": hex.EncodeToString(digest)}}}
		}
		pred := b.predicate
		if pred == "" {
			pred = "https://slsa.dev/provenance/v1"
		}
		stmt := map[string]any{
			"_type":         "https://in-toto.io/Statement/v1",
			"subject":       subjects,
			"predicateType": pred,
			"predicate":     map[string]any{"buildDefinition": map[string]any{"buildType": "https://mobyproject.org/buildkit@v1"}},
		}
		payload, err := json.Marshal(stmt)
		if err != nil {
			t.Fatal(err)
		}
		if b.rawPayload != nil {
			payload = b.rawPayload
		}
		pt := b.payloadType
		if pt == "" {
			pt = "application/vnd.in-toto+json"
		}
		sig := s.signer.sign(t, dsse.PAE(pt, payload), false, b.hashP384)
		if b.badSig {
			sig = s.signer.sign(t, []byte("something else"), false, b.hashP384)
		}
		s.sig = sig
		s.envelope = &dsse.Envelope{PayloadType: pt, Payload: b64(payload), Signatures: []dsse.Signature{{Sig: b64(sig)}}}
		sigs := []any{map[string]any{"sig": b64(sig)}}
		switch b.keyid {
		case "":
		case "fingerprint":
			id, err := dsse.SHA256KeyID(s.signer.pub)
			if err != nil {
				t.Fatal(err)
			}
			sigs[0].(map[string]any)["keyid"] = id
		default:
			sigs[0].(map[string]any)["keyid"] = b.keyid
		}
		if b.twoSigs {
			sigs = append(sigs, map[string]any{"sig": b64(sig), "keyid": "second"})
		}
		out["dsseEnvelope"] = map[string]any{"payload": b64(payload), "payloadType": pt, "signatures": sigs}
	} else {
		var sig []byte
		if s.signer.kind == "ed25519" {
			sig = s.signer.sign(t, art, b.ph, false)
		} else {
			sig = s.signer.sign(t, art, false, b.hashP384)
		}
		if b.badSig {
			sig = s.signer.sign(t, []byte("something else"), b.ph, b.hashP384)
		}
		s.sig = sig
		md := digest
		if b.msgDigestOverride != nil {
			md = b.msgDigestOverride
		}
		alg := b.msgAlg
		if alg == "" {
			alg = "SHA2_256"
		}
		out["messageSignature"] = map[string]any{
			"messageDigest": map[string]any{"algorithm": alg, "digest": b64(md)},
			"signature":     b64(sig),
		}
	}
	vm := map[string]any{}
	switch b.material {
	case "pubkey":
		vm["publicKey"] = map[string]any{"hint": b.hint}
	case "chain":
		certs := []any{map[string]any{"rawBytes": b64(s.cert.Raw)}}
		extra := b.chainExtra
		if extra == nil {
			extra = []*x509.Certificate{w.fulcio.inter}
		}
		for _, c := range extra {
			certs = append(certs, map[string]any{"rawBytes": b64(c.Raw)})
		}
		vm["x509CertificateChain"] = map[string]any{"certificates": certs}
	default:
		vm["certificate"] = map[string]any{"rawBytes": b64(s.cert.Raw)}
	}
	var entries []any
	for _, e := range b.entries {
		if e.kind == "v2" {
			entries = append(entries, w.v2Entry(s, e))
		} else {
			entries = append(entries, w.v1Entry(s, e))
		}
	}
	if entries != nil {
		vm["tlogEntries"] = entries
	}
	if len(b.tsas) > 0 {
		var toks []any
		for _, ts := range b.tsas {
			toks = append(toks, map[string]any{"signedTimestamp": w.token(s.sig, ts)})
		}
		vm["timestampVerificationData"] = map[string]any{"rfc3161Timestamps": toks}
	}
	out["verificationMaterial"] = vm
	return built{json: out, signed: s, digest: digest}
}

// ---------------------------------------------------------------- verification

// dhiMaterial: a key trusted for any hint from a time on, with the Fulcio root's TSAs
// and Rekor logs and none of its CAs or CT logs (policy-helpers roots/dhi).
type dhiMaterial struct {
	v         signature.Verifier
	validFrom int64
	fulcio    root.TrustedMaterial
}

type dhiVerifier struct {
	signature.Verifier
	validFrom int64
}

func (d *dhiVerifier) ValidAtTime(t time.Time) bool { return t.Unix() >= d.validFrom }

func (d *dhiMaterial) PublicKeyVerifier(string) (root.TimeConstrainedVerifier, error) {
	return &dhiVerifier{d.v, d.validFrom}, nil
}
func (d *dhiMaterial) TimestampingAuthorities() []root.TimestampingAuthority {
	return d.fulcio.TimestampingAuthorities()
}
func (d *dhiMaterial) FulcioCertificateAuthorities() []root.CertificateAuthority { return nil }
func (d *dhiMaterial) RekorLogs() map[string]*root.TransparencyLog               { return d.fulcio.RekorLogs() }
func (d *dhiMaterial) CTLogs() map[string]*root.TransparencyLog                  { return nil }

func run(t *testing.T, c *ocCase, bundleJSON []byte) {
	tr, err := root.NewTrustedRootFromJSON([]byte(c.trustedRoot))
	if err != nil {
		t.Fatalf("%s: trusted root: %v", c.Name, err)
	}
	var tm root.TrustedMaterial = tr
	if c.Key != nil {
		blk, _ := pem.Decode([]byte(c.Key.PEM))
		pub, err := x509.ParsePKIXPublicKey(blk.Bytes)
		if err != nil {
			t.Fatal(err)
		}
		v, err := signature.LoadVerifierWithOpts(pub)
		if err != nil {
			t.Fatal(err)
		}
		tm = &dhiMaterial{v: v, validFrom: c.Key.ValidFrom, fulcio: tr}
	}
	answer := func() (*verify.VerificationResult, error) {
		b := &bundle.Bundle{Bundle: new(protobundle.Bundle)}
		if err := b.UnmarshalJSON(bundleJSON); err != nil {
			return nil, err
		}
		var opts []verify.VerifierOption
		cf := c.Config
		if cf.NoObserver {
			opts = append(opts, verify.WithNoObserverTimestamps())
		}
		if cf.Observer > 0 {
			opts = append(opts, verify.WithObserverTimestamps(cf.Observer))
		}
		if cf.Tlog > 0 {
			opts = append(opts, verify.WithTransparencyLog(cf.Tlog))
		}
		if cf.SCT > 0 {
			opts = append(opts, verify.WithSignedCertificateTimestamps(cf.SCT))
		}
		if cf.Signed > 0 {
			opts = append(opts, verify.WithSignedTimestamps(cf.Signed))
		}
		if cf.Integrated > 0 {
			opts = append(opts, verify.WithIntegratedTimestamps(cf.Integrated))
		}
		gv, err := verify.NewVerifier(tm, opts...)
		if err != nil {
			return nil, err
		}
		var artifact verify.ArtifactPolicyOption
		if c.Policy.Digest != nil {
			d, err := hex.DecodeString(c.Policy.Digest.Hex)
			if err != nil {
				t.Fatal(err)
			}
			artifact = verify.WithArtifactDigest(c.Policy.Digest.Alg, d)
		} else {
			artifact = verify.WithoutArtifactUnsafe()
		}
		var identity verify.PolicyOption
		if c.Policy.Identity == "unsafe" {
			identity = verify.WithoutIdentitiesUnsafe()
		} else {
			san, err := verify.NewSANMatcher("", ".*")
			if err != nil {
				t.Fatal(err)
			}
			iss, err := verify.NewIssuerMatcher("", ".*")
			if err != nil {
				t.Fatal(err)
			}
			id, err := verify.NewCertificateIdentity(san, iss, certificate.Extensions{})
			if err != nil {
				t.Fatal(err)
			}
			identity = verify.WithCertificateIdentity(id)
		}
		return gv.Verify(b, verify.NewPolicy(artifact, identity))
	}
	res, err := answer()
	if err != nil {
		c.Error = err.Error()
		return
	}
	r := &ocResult{Timestamps: []ocTimestamp{}}
	if res.Signature != nil {
		if s := res.Signature.Certificate; s != nil {
			r.Certificate = summary(s)
		}
		if res.Signature.PublicKeyID != nil {
			id := string(*res.Signature.PublicKeyID)
			r.PublicKeyID = &id
		}
	}
	for _, ts := range res.VerifiedTimestamps {
		r.Timestamps = append(r.Timestamps, ocTimestamp{Type: ts.Type, URI: ts.URI, Secs: ts.Timestamp.Unix(), Nanos: ts.Timestamp.Nanosecond()})
	}
	if st := res.Statement; st != nil {
		os := &ocStatement{PredicateType: st.GetPredicateType(), Subjects: []ocSubject{}}
		for _, sub := range st.GetSubject() {
			s := ocSubject{Name: sub.GetName(), Digests: [][2]string{}}
			for alg, d := range sub.GetDigest() {
				s.Digests = append(s.Digests, [2]string{alg, d})
			}
			sort.Slice(s.Digests, func(i, j int) bool { return s.Digests[i][0] < s.Digests[j][0] })
			os.Subjects = append(os.Subjects, s)
		}
		r.Statement = os
	}
	c.Result = r
}

func summary(s *certificate.Summary) map[string]string {
	b, err := json.Marshal(s)
	if err != nil {
		panic(err)
	}
	m := map[string]string{}
	if err := json.Unmarshal(b, &m); err != nil {
		panic(err)
	}
	// Summary's JSON leaves empty extensions out; the harness wants them all.
	for _, k := range summaryKeys {
		if _, ok := m[k]; !ok {
			m[k] = ""
		}
	}
	return m
}

var summaryKeys = []string{
	"certificateIssuer", "subjectAlternativeName", "issuer", "githubWorkflowTrigger", "githubWorkflowSHA",
	"githubWorkflowName", "githubWorkflowRepository", "githubWorkflowRef", "buildSignerURI", "buildSignerDigest",
	"runnerEnvironment", "sourceRepositoryURI", "sourceRepositoryDigest", "sourceRepositoryRef",
	"sourceRepositoryIdentifier", "sourceRepositoryOwnerURI", "sourceRepositoryOwnerIdentifier", "buildConfigURI",
	"buildConfigDigest", "buildTrigger", "runInvocationURI", "sourceRepositoryVisibilityAtSigning",
}

// ---------------------------------------------------------------- JSON edits

func clone(m map[string]any) map[string]any {
	b, err := json.Marshal(m)
	if err != nil {
		panic(err)
	}
	var out map[string]any
	if err := json.Unmarshal(b, &out); err != nil {
		panic(err)
	}
	return out
}

func get(m map[string]any, path ...string) map[string]any {
	cur := m
	for _, p := range path {
		cur = cur[p].(map[string]any)
	}
	return cur
}

func entryAt(m map[string]any, i int) map[string]any {
	return get(m, "verificationMaterial")["tlogEntries"].([]any)[i].(map[string]any)
}

func enc(m map[string]any) []byte {
	b, err := json.Marshal(m)
	if err != nil {
		panic(err)
	}
	return b
}

// ---------------------------------------------------------------- the cases

func TestShardsSigstoreOracle(t *testing.T) {
	out := os.Getenv("SHARDS_SIGSTORE_OUT")
	if out == "" {
		t.Skip("SHARDS_SIGSTORE_OUT not set")
	}
	cryptotest.SetGlobalRandom(t, 1)
	w := newWorld(t)
	var cases []*ocCase
	roots := map[string]string{}

	add := func(name string, root string, body []byte, cfg ocConfig, pol ocPolicy, key *ocKey) {
		id := hex.EncodeToString(sha([]byte(root)))[:16]
		roots[id] = root
		c := &ocCase{Name: name, Root: id, trustedRoot: root, Bundle: b64(body), Config: cfg, Policy: pol, Key: key, Fulcio: key == nil, Now: t0.Unix()}
		run(t, c, body)
		cases = append(cases, c)
	}
	artifactPolicy := func(d []byte) ocPolicy {
		return ocPolicy{Digest: &ocDigest{Alg: "sha256", Hex: hex.EncodeToString(d)}, Identity: "any"}
	}
	full := []entrySpec{{kind: "dsse"}}
	stdTSA := []tsaSpec{{}}

	// A case from a spec: verified as VerifyArtifact verifies.
	spec := func(name string, b bundleSpec, edit func(m map[string]any)) {
		currentCase = name
		bb := w.bundle(b)
		m := bb.json
		if edit != nil {
			m = clone(m)
			edit(m)
		}
		cfg := cfgArtifact
		if b.cfg != nil {
			cfg = *b.cfg
		}
		add(name, w.rootJSON, enc(m), cfg, artifactPolicy(bb.digest), nil)
	}
	// The verifier's options for bundles no log could have taken: a timestamp, no tlog.
	noTlog := &ocConfig{Observer: 1, SCT: 1}
	raw := func(name string, b bundleSpec, mutate func([]byte) []byte) {
		currentCase = name
		bb := w.bundle(b)
		add(name, w.rootJSON, mutate(enc(bb.json)), cfgArtifact, artifactPolicy(bb.digest), nil)
	}

	// ---- valid forms
	spec("valid v0.3 dsse cert dsse-entry tsa", bundleSpec{entries: full, tsas: stdTSA}, nil)
	spec("valid v0.3 dsse cert no tsa", bundleSpec{entries: full}, nil)
	spec("valid v0.3 dsse cert intoto-entry", bundleSpec{entries: []entrySpec{{kind: "intoto"}}}, nil)
	spec("valid v0.3 msg cert hashedrekord", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}, tsas: stdTSA}, nil)
	spec("valid v0.2 dsse cert", bundleSpec{version: "0.2", entries: full}, nil)
	spec("valid v0.2 dsse chain", bundleSpec{version: "0.2", material: "chain", entries: full}, nil)
	spec("valid v0.1 chain promise only", bundleSpec{version: "0.1", material: "chain", entries: []entrySpec{{kind: "dsse", noProof: true}}}, nil)
	spec("valid v0.1 cert promise and proof", bundleSpec{version: "0.1", entries: full}, nil)
	spec("valid v0.3 extensions v1", bundleSpec{leaf: leafSpec{extsV1: true}, entries: full}, nil)
	spec("valid v0.3 email san", bundleSpec{leaf: leafSpec{san: "email"}, entries: full}, nil)
	spec("valid v0.3 critical othername san", bundleSpec{leaf: leafSpec{san: "othername"}, entries: full}, nil)
	spec("dsse keyid the key's fingerprint", bundleSpec{keyid: "fingerprint", tsas: stdTSA, cfg: noTlog}, nil)
	spec("dsse keyid the ed25519 key's fingerprint", bundleSpec{signerKind: "ed25519", keyid: "fingerprint", tsas: stdTSA, cfg: noTlog}, nil)
	spec("dsse keyid the rsa key's fingerprint", bundleSpec{signerKind: "rsa", keyid: "fingerprint", tsas: stdTSA, cfg: noTlog}, nil)
	spec("dsse keyid the p384 key's fingerprint", bundleSpec{signerKind: "p384", keyid: "fingerprint", tsas: stdTSA, cfg: noTlog}, nil)
	spec("dsse keyid another key's", bundleSpec{keyid: "SHA256:another", tsas: stdTSA, cfg: noTlog}, nil)
	spec("valid v0.3 p384 sha384 rekor v2", bundleSpec{signerKind: "p384", hashP384: true, entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, nil)
	spec("p384 sha384 dsse no tlog", bundleSpec{signerKind: "p384", hashP384: true, tsas: stdTSA, cfg: noTlog}, nil)
	spec("valid v0.3 p384 compat sha256 dsse", bundleSpec{signerKind: "p384", entries: full}, nil)
	spec("valid v0.3 p384 compat sha256 msg", bundleSpec{signerKind: "p384", content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}}, nil)
	spec("valid v0.3 rsa dsse", bundleSpec{signerKind: "rsa", entries: full}, nil)
	spec("valid v0.3 rsa msg", bundleSpec{signerKind: "rsa", content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}}, nil)
	spec("valid v0.3 ed25519 dsse", bundleSpec{signerKind: "ed25519", entries: full}, nil)
	spec("ed25519 msg by digest refused", bundleSpec{signerKind: "ed25519", ph: true, content: "msg", tsas: stdTSA, cfg: noTlog}, nil)
	spec("ed25519 v2 msg by digest refused", bundleSpec{signerKind: "ed25519", ph: true, content: "msg", entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, nil)
	spec("valid v0.3 rekor v2 dsse tsa", bundleSpec{entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, nil)
	spec("valid v0.3 rekor v2 msg tsa", bundleSpec{content: "msg", entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, nil)
	spec("rekor v2 without tsa: no observer timestamp", bundleSpec{entries: []entrySpec{{kind: "v2"}}}, nil)
	spec("valid v1 and v2 entries", bundleSpec{entries: []entrySpec{{kind: "dsse"}, {kind: "v2"}}, tsas: stdTSA}, nil)
	spec("two tsa tokens same authority", bundleSpec{entries: full, tsas: []tsaSpec{{}, {at: t0.Add(5 * time.Second)}}}, nil)
	spec("two entries same log", bundleSpec{entries: []entrySpec{{kind: "dsse"}, {kind: "dsse", logIndex: 42}}}, nil)

	// ---- media types and versions
	spec("v0.4 refused", bundleSpec{entries: full, mediaType: "application/vnd.dev.sigstore.bundle.v0.4+json"}, nil)
	spec("media type unknown", bundleSpec{entries: full, mediaType: "application/json"}, nil)
	spec("media type empty", bundleSpec{entries: full, mediaType: ""}, func(m map[string]any) { m["mediaType"] = "" })
	spec("media type v0.3.1", bundleSpec{entries: full, mediaType: "application/vnd.dev.sigstore.bundle.v0.3.1+json"}, nil)
	spec("media type v0.0.9", bundleSpec{entries: full, mediaType: "application/vnd.dev.sigstore.bundle.v0.0.9+json"}, nil)
	spec("media type invalid semver", bundleSpec{entries: full, mediaType: "application/vnd.dev.sigstore.bundle.vx+json"}, nil)
	spec("media type v0.3 old form", bundleSpec{entries: full, mediaType: "application/vnd.dev.sigstore.bundle+json;version=0.3"}, nil)
	spec("media type v0.2 new form", bundleSpec{version: "0.2", entries: full, mediaType: "application/vnd.dev.sigstore.bundle.v0.2+json"}, nil)
	spec("v0.3 chain refused", bundleSpec{material: "chain", entries: full}, nil)
	spec("v0.1 without promise", bundleSpec{version: "0.1", entries: []entrySpec{{kind: "dsse", noPromise: true}}}, nil)
	spec("v0.2 without proof", bundleSpec{version: "0.2", entries: []entrySpec{{kind: "dsse", noProof: true}}}, nil)

	// ---- protojson shapes
	spec("unknown top-level field", bundleSpec{entries: full}, func(m map[string]any) { m["extra"] = 1 })
	spec("unknown nested field", bundleSpec{entries: full}, func(m map[string]any) { get(m, "verificationMaterial")["extra"] = "x" })
	spec("both contents", bundleSpec{entries: full}, func(m map[string]any) {
		m["messageSignature"] = map[string]any{"signature": "AA=="}
	})
	spec("both materials", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial")["publicKey"] = map[string]any{"hint": "x"}
	})
	spec("no content", bundleSpec{entries: full}, func(m map[string]any) { delete(m, "dsseEnvelope") })
	spec("null content", bundleSpec{entries: full}, func(m map[string]any) { m["dsseEnvelope"] = nil })
	spec("no verification material", bundleSpec{entries: full}, func(m map[string]any) { delete(m, "verificationMaterial") })
	spec("material without content", bundleSpec{entries: full}, func(m map[string]any) {
		delete(get(m, "verificationMaterial"), "certificate")
	})
	spec("proto field names", bundleSpec{entries: full}, func(m map[string]any) {
		m["media_type"] = m["mediaType"]
		delete(m, "mediaType")
		m["dsse_envelope"] = m["dsseEnvelope"]
		delete(m, "dsseEnvelope")
		vm := get(m, "verificationMaterial")
		vm["tlog_entries"] = vm["tlogEntries"]
		delete(vm, "tlogEntries")
	})
	raw("duplicate field via proto name", bundleSpec{entries: full}, func(b []byte) []byte {
		return bytes.Replace(b, []byte(`{"dsseEnvelope"`), []byte(`{"media_type":"x","dsseEnvelope"`), 1)
	})
	raw("trailing data", bundleSpec{entries: full}, func(b []byte) []byte { return append(b, []byte(" {}")...) })
	raw("truncated json", bundleSpec{entries: full}, func(b []byte) []byte { return b[:len(b)/2] })
	raw("not json", bundleSpec{entries: full}, func(b []byte) []byte { return []byte("bundle") })
	raw("empty input", bundleSpec{entries: full}, func(b []byte) []byte { return []byte("") })
	raw("array top", bundleSpec{entries: full}, func(b []byte) []byte { return []byte("[]") })
	spec("logIndex as number", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = 1180749977 })
	spec("logIndex exponent", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = "1.180749977e9" })
	spec("logIndex fraction", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = 1.5 })
	spec("logIndex negative", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = "-1" })
	spec("logIndex padded string", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = " 5" })
	spec("bad base64 body", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["canonicalizedBody"] = "!!!" })
	spec("url base64 cert", bundleSpec{entries: full}, func(m map[string]any) {
		c := get(m, "verificationMaterial", "certificate")
		s := c["rawBytes"].(string)
		s = strings.TrimRight(strings.NewReplacer("+", "-", "/", "_").Replace(s), "=")
		c["rawBytes"] = s
	})
	spec("unpadded base64 sig", bundleSpec{entries: full}, func(m map[string]any) {
		sig := get(m, "dsseEnvelope")["signatures"].([]any)[0].(map[string]any)
		sig["sig"] = strings.TrimRight(sig["sig"].(string), "=")
	})
	spec("enum by number", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}}, func(m map[string]any) {
		get(m, "messageSignature", "messageDigest")["algorithm"] = 1
	})
	spec("enum unknown name", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}}, func(m map[string]any) {
		get(m, "messageSignature", "messageDigest")["algorithm"] = "SHA9"
	})
	spec("string field given number", bundleSpec{entries: full}, func(m map[string]any) { m["mediaType"] = 3 })
	spec("repeated field given object", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial")["tlogEntries"] = map[string]any{}
	})
	spec("null tlog entries", bundleSpec{entries: full}, func(m map[string]any) { get(m, "verificationMaterial")["tlogEntries"] = nil })

	// ---- material
	spec("cert empty raw bytes", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial", "certificate")["rawBytes"] = ""
	})
	spec("cert garbage", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial", "certificate")["rawBytes"] = b64([]byte("not a certificate"))
	})
	spec("cert empty object", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial")["certificate"] = map[string]any{}
	})
	spec("chain empty", bundleSpec{version: "0.2", material: "chain", entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial", "x509CertificateChain")["certificates"] = []any{}
	})
	spec("chain with root (self-signed) in v0.2", bundleSpec{version: "0.2", material: "chain", chainExtra: []*x509.Certificate{w.fulcio.inter, w.fulcio.root}, entries: full}, nil)
	spec("chain garbage second cert v0.2", bundleSpec{version: "0.2", material: "chain", entries: full}, func(m map[string]any) {
		certs := get(m, "verificationMaterial", "x509CertificateChain")["certificates"].([]any)
		certs[1] = map[string]any{"rawBytes": b64([]byte("junk"))}
	})
	spec("chain leaf garbage v0.2", bundleSpec{version: "0.2", material: "chain", entries: full}, func(m map[string]any) {
		certs := get(m, "verificationMaterial", "x509CertificateChain")["certificates"].([]any)
		certs[0] = map[string]any{"rawBytes": b64([]byte("junk"))}
	})
	spec("public key bundle with fulcio root", bundleSpec{material: "pubkey", content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}, tsas: stdTSA}, nil)

	// ---- tlog entries
	spec("no tlog entries", bundleSpec{tsas: stdTSA}, nil)
	spec("no proof (v0.3)", bundleSpec{entries: []entrySpec{{kind: "dsse", noProof: true}}}, nil)
	spec("proof only", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: stdTSA}, nil)
	spec("proof only no tsa", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}}, nil)
	spec("bad SET", bundleSpec{entries: []entrySpec{{kind: "dsse", badSET: true}}, tsas: stdTSA}, nil)
	spec("bad SET only entry", bundleSpec{entries: []entrySpec{{kind: "dsse", badSET: true}}}, nil)
	spec("bad inclusion proof", bundleSpec{entries: []entrySpec{{kind: "dsse", badProof: true}}}, nil)
	spec("proof root not checkpoint's", bundleSpec{entries: []entrySpec{{kind: "dsse", badRoot: true}}}, nil)
	spec("checkpoint tampered", bundleSpec{entries: []entrySpec{{kind: "dsse", badCheckpoint: true}}}, nil)
	spec("unknown log skipped", bundleSpec{entries: []entrySpec{{kind: "dsse", unknownLog: true}}, tsas: stdTSA}, nil)
	spec("unknown log beside known", bundleSpec{entries: []entrySpec{{kind: "dsse", unknownLog: true}, {kind: "dsse"}}}, nil)
	// Another signature over the same digest (ECDSA's are randomized): the entry's and the
	// bundle's differ.
	otherSig := func() []byte {
		s := w.signer("p256")
		sig, err := ecdsa.SignASN1(rand.Reader, s.priv.(*ecdsa.PrivateKey), sha(artifactBytes))
		if err != nil {
			t.Fatal(err)
		}
		return sig
	}()
	spec("entry signature mismatch", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord", sigOverride: otherSig}}}, nil)
	spec("entry key not the certificate", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord", keyOverride: w.signer("p256").pub}}}, nil)
	spec("dsse entry key not the certificate", bundleSpec{entries: []entrySpec{{kind: "dsse", keyOverride: w.signer("p256").pub}}}, nil)
	spec("integrated time before cert", bundleSpec{entries: []entrySpec{{kind: "dsse", integrated: t0.Add(-time.Hour)}}}, nil)
	spec("integrated time after cert", bundleSpec{entries: []entrySpec{{kind: "dsse", integrated: t0.Add(time.Hour)}}}, nil)
	spec("integrated time before rekor key", bundleSpec{entries: []entrySpec{{kind: "dsse", integrated: t0.AddDate(-2, 0, 0)}}}, nil)
	spec("kind version mismatch", bundleSpec{entries: []entrySpec{{kind: "dsse", kindVersion: [2]string{"dsse", "0.0.2"}}}}, nil)
	spec("kind mismatch", bundleSpec{entries: []entrySpec{{kind: "dsse", kindVersion: [2]string{"intoto", "0.0.1"}}}}, nil)
	spec("dsse content hashedrekord entry", bundleSpec{entries: []entrySpec{{kind: "hashedrekord"}}}, nil)
	spec("hashedrekord digest mismatch", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}, msgDigestOverride: sha([]byte("other"))}, nil)
	spec("entry missing logId", bundleSpec{entries: full}, func(m map[string]any) { delete(entryAt(m, 0), "logId") })
	spec("entry empty logId", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logId"] = map[string]any{} })
	spec("entry missing kindVersion", bundleSpec{entries: full}, func(m map[string]any) { delete(entryAt(m, 0), "kindVersion") })
	spec("entry missing body", bundleSpec{entries: full}, func(m map[string]any) { delete(entryAt(m, 0), "canonicalizedBody") })
	spec("entry empty body", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["canonicalizedBody"] = "" })
	spec("entry body not json", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["canonicalizedBody"] = b64([]byte("x")) })
	spec("entry body unknown kind", bundleSpec{entries: full}, func(m map[string]any) {
		entryAt(m, 0)["canonicalizedBody"] = b64([]byte(`{"apiVersion":"0.0.1","kind":"rekord","spec":{}}`))
	})
	spec("entry body no kind", bundleSpec{entries: full}, func(m map[string]any) {
		entryAt(m, 0)["canonicalizedBody"] = b64([]byte(`{"apiVersion":"0.0.1","spec":{}}`))
	})
	spec("entry negative index", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["logIndex"] = "-5" })
	spec("proof without checkpoint", bundleSpec{entries: full}, func(m map[string]any) { delete(get(entryAt(m, 0), "inclusionProof"), "checkpoint") })
	spec("proof empty checkpoint", bundleSpec{entries: full}, func(m map[string]any) {
		get(entryAt(m, 0), "inclusionProof")["checkpoint"] = map[string]any{"envelope": ""}
	})
	spec("checkpoint garbage", bundleSpec{entries: full}, func(m map[string]any) {
		get(entryAt(m, 0), "inclusionProof")["checkpoint"] = map[string]any{"envelope": "garbage\n"}
	})
	spec("empty promise", bundleSpec{entries: full}, func(m map[string]any) { entryAt(m, 0)["inclusionPromise"] = map[string]any{} })
	spec("duplicate entries", bundleSpec{entries: full}, func(m map[string]any) {
		vm := get(m, "verificationMaterial")
		es := vm["tlogEntries"].([]any)
		vm["tlogEntries"] = append(es, es[0])
	})
	spec("33 entries", bundleSpec{entries: full}, func(m map[string]any) {
		vm := get(m, "verificationMaterial")
		es := vm["tlogEntries"].([]any)
		var more []any
		for i := 0; i < 33; i++ {
			e := clone(es[0].(map[string]any))
			e["logIndex"] = fmt.Sprint(1000 + i)
			more = append(more, e)
		}
		vm["tlogEntries"] = more
	})
	spec("rekor v2 bad proof", bundleSpec{entries: []entrySpec{{kind: "v2", badProof: true}}, tsas: stdTSA}, nil)
	spec("rekor v2 bad checkpoint", bundleSpec{entries: []entrySpec{{kind: "v2", badCheckpoint: true}}, tsas: stdTSA}, nil)
	spec("rekor v2 wrong root", bundleSpec{entries: []entrySpec{{kind: "v2", badRoot: true}}, tsas: stdTSA}, nil)
	spec("rekor v2 digest not the signature's", bundleSpec{content: "msg", entries: []entrySpec{{kind: "v2", digestOverride: sha([]byte("other"))}}, tsas: stdTSA}, nil)
	spec("rekor v2 without proof", bundleSpec{entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, func(m map[string]any) {
		delete(entryAt(m, 0), "inclusionProof")
	})
	spec("rekor v2 promise added", bundleSpec{entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA}, func(m map[string]any) {
		entryAt(m, 0)["inclusionPromise"] = map[string]any{"signedEntryTimestamp": b64([]byte("set"))}
	})

	// ---- timestamps
	spec("tsa only, no tlog required? tlog missing", bundleSpec{tsas: stdTSA}, nil)
	spec("rogue tsa", bundleSpec{entries: full, tsas: []tsaSpec{{rogue: true}}}, nil)
	spec("rogue tsa no tlog timestamp", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{rogue: true}}}, nil)
	spec("tsa over other signature", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{overSig: []byte("other")}}}, nil)
	spec("tsa sha384", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{sha1: true}}}, nil)
	spec("tsa without certs", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{noCerts: true}}}, nil)
	spec("tsa time after cert", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{at: t0.Add(time.Hour)}}}, nil)
	spec("tsa time before authority", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{at: t0.AddDate(-2, 0, 0)}}}, nil)
	spec("tsa garbage", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: stdTSA}, func(m map[string]any) {
		get(m, "verificationMaterial", "timestampVerificationData")["rfc3161Timestamps"] = []any{map[string]any{"signedTimestamp": b64([]byte("junk"))}}
	})
	spec("tsa duplicate authority", bundleSpec{entries: []entrySpec{{kind: "dsse", noPromise: true}}, tsas: []tsaSpec{{}, {at: t0.Add(3 * time.Second)}}}, nil)
	spec("33 timestamps", bundleSpec{entries: full, tsas: stdTSA}, func(m map[string]any) {
		td := get(m, "verificationMaterial", "timestampVerificationData")
		ts := td["rfc3161Timestamps"].([]any)
		var more []any
		for i := 0; i < 33; i++ {
			more = append(more, ts[0])
		}
		td["rfc3161Timestamps"] = more
	})
	spec("empty timestamp data", bundleSpec{entries: full}, func(m map[string]any) {
		get(m, "verificationMaterial")["timestampVerificationData"] = map[string]any{}
	})

	// ---- certificates
	spec("cert expired at integrated time", bundleSpec{leaf: leafSpec{notBefore: t0.Add(-time.Hour), notAfter: t0.Add(-time.Minute)}, entries: full}, nil)
	spec("cert not yet valid", bundleSpec{leaf: leafSpec{notBefore: t0.Add(time.Minute), notAfter: t0.Add(time.Hour)}, entries: full}, nil)
	spec("cert from rogue ca", bundleSpec{leaf: leafSpec{ca: &w.rogue}, entries: full}, nil)
	spec("cert without eku", bundleSpec{leaf: leafSpec{noEKU: true}, entries: full}, nil)
	spec("cert without san", bundleSpec{leaf: leafSpec{san: "none"}, entries: full}, nil)
	spec("cert without sct", bundleSpec{leaf: leafSpec{sct: "none"}, entries: full}, nil)
	spec("sct bad signature", bundleSpec{leaf: leafSpec{sct: "badsig"}, entries: full}, nil)
	spec("sct unknown log", bundleSpec{leaf: leafSpec{sct: "unknownlog"}, entries: full}, nil)
	spec("sct issuer v2 not DER", bundleSpec{leaf: leafSpec{badDERIssuer: true}, entries: full}, nil)
	spec("cert chain v0.2 rogue intermediate", bundleSpec{version: "0.2", material: "chain", chainExtra: []*x509.Certificate{w.rogue.inter}, entries: full}, nil)

	// ---- signatures
	spec("bad dsse signature", bundleSpec{badSig: true, tsas: stdTSA, cfg: noTlog}, nil)
	spec("two dsse signatures", bundleSpec{twoSigs: true, entries: full}, nil)
	spec("no dsse signatures", bundleSpec{entries: full}, func(m map[string]any) { get(m, "dsseEnvelope")["signatures"] = []any{} })
	spec("dsse payload type not in-toto", bundleSpec{payloadType: "application/json", entries: full}, nil)
	spec("dsse payload not a statement", bundleSpec{rawPayload: []byte(`{"hello":"world"}`), entries: full}, nil)
	spec("dsse payload not json", bundleSpec{rawPayload: []byte("hello"), entries: full}, nil)
	spec("dsse statement unknown field", bundleSpec{rawPayload: []byte(`{"_type":"https://in-toto.io/Statement/v1","subject":[{"digest":{"sha256":"` + hex.EncodeToString(sha(artifactBytes)) + `"}}],"predicateType":"x","extra":1}`), entries: full}, nil)
	spec("subject digest other algorithm", bundleSpec{subjects: []map[string]any{{"name": "a", "digest": map[string]any{"sha512": strings.Repeat("ab", 64)}}}, entries: full}, nil)
	spec("subject digest not hex", bundleSpec{subjects: []map[string]any{{"name": "a", "digest": map[string]any{"sha256": "zz"}}}, entries: full}, nil)
	spec("subject digest wrong", bundleSpec{subjects: []map[string]any{{"name": "a", "digest": map[string]any{"sha256": strings.Repeat("00", 32)}}}, entries: full}, nil)
	spec("two subjects, second matches", bundleSpec{subjects: []map[string]any{
		{"name": "a", "digest": map[string]any{"sha256": strings.Repeat("00", 32)}},
		{"name": "b", "digest": map[string]any{"sha256": hex.EncodeToString(sha(artifactBytes)), "sha512": strings.Repeat("11", 64)}},
	}, entries: full}, nil)
	spec("no subjects", bundleSpec{subjects: []map[string]any{}, entries: full}, nil)
	spec("predicate cosign", bundleSpec{predicate: "https://sigstore.dev/cosign/sign/v1", entries: full}, nil)
	spec("bad message signature", bundleSpec{content: "msg", badSig: true, tsas: stdTSA, cfg: noTlog}, nil)
	spec("message digest not the artifact's", bundleSpec{content: "msg", msgDigestOverride: sha([]byte("x")), tsas: stdTSA, cfg: noTlog}, nil)
	spec("valid message signature no tlog", bundleSpec{content: "msg", tsas: stdTSA, cfg: noTlog}, nil)
	spec("valid dsse no tlog", bundleSpec{tsas: stdTSA, cfg: noTlog}, nil)
	spec("message digest sha384 algorithm", bundleSpec{content: "msg", msgAlg: "SHA2_384", entries: []entrySpec{{kind: "hashedrekord"}}}, nil)
	spec("message without digest", bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}}, func(m map[string]any) {
		delete(get(m, "messageSignature"), "messageDigest")
	})

	// ---- policy variations on a valid bundle
	{
		bb := w.bundle(bundleSpec{entries: full, tsas: stdTSA})
		add("policy wrong digest", w.rootJSON, enc(bb.json), cfgArtifact, artifactPolicy(sha([]byte("other"))), nil)
		add("policy digest sha512", w.rootJSON, enc(bb.json), cfgArtifact, ocPolicy{Digest: &ocDigest{Alg: "sha512", Hex: strings.Repeat("ab", 64)}, Identity: "any"}, nil)
		add("policy no artifact unsafe", w.rootJSON, enc(bb.json), cfgArtifact, ocPolicy{Identity: "any"}, nil)
		add("policy unsafe identity", w.rootJSON, enc(bb.json), cfgArtifact, ocPolicy{Digest: artifactPolicy(bb.digest).Digest, Identity: "unsafe"}, nil)
		add("config signed timestamps 1", w.rootJSON, enc(bb.json), ocConfig{Tlog: 1, Signed: 1, SCT: 1}, artifactPolicy(bb.digest), nil)
		add("config signed timestamps 2", w.rootJSON, enc(bb.json), ocConfig{Tlog: 1, Signed: 2}, artifactPolicy(bb.digest), nil)
		add("config integrated 1", w.rootJSON, enc(bb.json), ocConfig{Tlog: 1, Integrated: 1}, artifactPolicy(bb.digest), nil)
		add("config tlog 2", w.rootJSON, enc(bb.json), ocConfig{Tlog: 2, Observer: 1}, artifactPolicy(bb.digest), nil)
		add("config observer 3", w.rootJSON, enc(bb.json), ocConfig{Tlog: 1, Observer: 3}, artifactPolicy(bb.digest), nil)
		add("config none", w.rootJSON, enc(bb.json), ocConfig{}, artifactPolicy(bb.digest), nil)
		add("config no observer with tlog", w.rootJSON, enc(bb.json), ocConfig{NoObserver: true, Tlog: 1}, artifactPolicy(bb.digest), nil)
		add("config no observer cert", w.rootJSON, enc(bb.json), ocConfig{NoObserver: true}, artifactPolicy(bb.digest), nil)
		add("config sct only tlog", w.rootJSON, enc(bb.json), ocConfig{Tlog: 1, Integrated: 1, SCT: 2}, artifactPolicy(bb.digest), nil)
		// Trusted root variations.
		noTSA := w.trustedRoot(func(r *prototrustroot.TrustedRoot) { r.TimestampAuthorities = nil })
		add("root without tsa", noTSA, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		noCT := w.trustedRoot(func(r *prototrustroot.TrustedRoot) { r.Ctlogs = nil })
		add("root without ct logs", noCT, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		caLate := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			r.CertificateAuthorities[0].ValidFor = tr(t0.Add(time.Hour), nil)
		})
		add("root ca valid from later", caLate, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		caEnded := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			e := t0.Add(-time.Hour)
			r.CertificateAuthorities[0].ValidFor = tr(t0.AddDate(-1, 0, 0), &e)
		})
		add("root ca ended", caEnded, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		rekorEnded := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			e := t0.Add(-time.Hour)
			r.Tlogs[0].PublicKey.ValidFor = tr(t0.AddDate(-1, 0, 0), &e)
		})
		add("root rekor key ended", rekorEnded, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		tsaEnded := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			e := t0.Add(-time.Hour)
			r.TimestampAuthorities[0].ValidFor = tr(t0.AddDate(-1, 0, 0), &e)
		})
		add("root tsa ended", tsaEnded, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		ctLate := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			r.Ctlogs[0].PublicKey.ValidFor = tr(t0.Add(time.Hour), nil)
		})
		add("root ct log valid from later", ctLate, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
		v2NoURL := w.trustedRoot(func(r *prototrustroot.TrustedRoot) { r.Tlogs[1].BaseUrl = "" })
		bv2 := w.bundle(bundleSpec{entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA})
		add("root rekor v2 without base url", v2NoURL, enc(bv2.json), cfgArtifact, artifactPolicy(bv2.digest), nil)
		v2Origin := w.trustedRoot(func(r *prototrustroot.TrustedRoot) { r.Tlogs[1].BaseUrl = "https://other.shards.test" })
		add("root rekor v2 other origin", v2Origin, enc(bv2.json), cfgArtifact, artifactPolicy(bv2.digest), nil)
		tsaIntermediate := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
			r.TimestampAuthorities[0].CertChain.Certificates = []*protocommon.X509Certificate{{RawBytes: w.tsaRoot.Raw}}
		})
		add("root tsa chain root only", tsaIntermediate, enc(bb.json), cfgArtifact, artifactPolicy(bb.digest), nil)
	}

	// ---- DHI-like key material
	{
		ks := w.signer("p256")
		key := &ocKey{PEM: string(keyPEM(t, ks.pub)), ValidFrom: t0.Add(-time.Hour).Unix()}
		other := &ocKey{PEM: string(keyPEM(t, w.signer("p256-other").pub)), ValidFrom: t0.Add(-time.Hour).Unix()}
		late := &ocKey{PEM: key.PEM, ValidFrom: t0.Add(time.Hour).Unix()}
		unsafeDigest := func(d []byte) ocPolicy {
			return ocPolicy{Digest: &ocDigest{Alg: "sha256", Hex: hex.EncodeToString(d)}, Identity: "unsafe"}
		}
		bare := w.bundle(bundleSpec{material: "pubkey", content: "msg"})
		add("dhi no observer valid", w.rootJSON, enc(bare.json), cfgDHINoObs, unsafeDigest(bare.digest), key)
		add("dhi no observer other key", w.rootJSON, enc(bare.json), cfgDHINoObs, unsafeDigest(bare.digest), other)
		add("dhi no observer wrong digest", w.rootJSON, enc(bare.json), cfgDHINoObs, unsafeDigest(sha([]byte("x"))), key)
		add("dhi no observer any identity", w.rootJSON, enc(bare.json), cfgDHINoObs, artifactPolicy(bare.digest), key)
		hinted := w.bundle(bundleSpec{material: "pubkey", content: "msg", hint: "dhi-key"})
		add("dhi hinted", w.rootJSON, enc(hinted.json), cfgDHINoObs, unsafeDigest(hinted.digest), key)
		withLog := w.bundle(bundleSpec{material: "pubkey", content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}})
		add("dhi tlog valid", w.rootJSON, enc(withLog.json), cfgDHITlog, unsafeDigest(withLog.digest), key)
		add("dhi tlog key valid later", w.rootJSON, enc(withLog.json), cfgDHITlog, unsafeDigest(withLog.digest), late)
		add("dhi tlog other key", w.rootJSON, enc(withLog.json), cfgDHITlog, unsafeDigest(withLog.digest), other)
		add("dhi tlog bundle without entries", w.rootJSON, enc(bare.json), cfgDHITlog, unsafeDigest(bare.digest), key)
		withTSA := w.bundle(bundleSpec{material: "pubkey", content: "msg", entries: []entrySpec{{kind: "hashedrekord", noPromise: true}}, tsas: stdTSA})
		add("dhi tlog proof and tsa", w.rootJSON, enc(withTSA.json), cfgDHITlog, unsafeDigest(withTSA.digest), key)
		dsseKey := w.bundle(bundleSpec{material: "pubkey", content: "dsse", entries: []entrySpec{{kind: "dsse"}}})
		add("dhi dsse tlog", w.rootJSON, enc(dsseKey.json), cfgDHITlog, unsafeDigest(dsseKey.digest), key)
		v2Key := w.bundle(bundleSpec{material: "pubkey", content: "msg", entries: []entrySpec{{kind: "v2"}}, tsas: stdTSA})
		add("dhi rekor v2", w.rootJSON, enc(v2Key.json), cfgDHITlog, unsafeDigest(v2Key.digest), key)
		certB := w.bundle(bundleSpec{content: "msg", entries: []entrySpec{{kind: "hashedrekord"}}})
		add("dhi given a cert bundle no observer", w.rootJSON, enc(certB.json), cfgDHINoObs, unsafeDigest(certB.digest), key)
		add("dhi given a cert bundle tlog", w.rootJSON, enc(certB.json), cfgDHITlog, unsafeDigest(certB.digest), key)
	}

	// ---- the real moby/buildkit v0.28.1 attestation bundle, against Sigstore's root
	{
		rootPath := os.Getenv("SHARDS_SIGSTORE_ROOT")
		realPath := os.Getenv("SHARDS_SIGSTORE_REAL")
		rootJSON, err := os.ReadFile(rootPath)
		if err != nil {
			t.Fatal(err)
		}
		realJSON, err := os.ReadFile(realPath)
		if err != nil {
			t.Fatal(err)
		}
		d, _ := hex.DecodeString("8bfad6cf4b6e1042a48c4f151a10da5fce0db3f2dbb768448c13b904514ab898")
		var m map[string]any
		if err := json.Unmarshal(realJSON, &m); err != nil {
			t.Fatal(err)
		}
		addReal := func(name string, body []byte, pol ocPolicy) {
			add(name, string(rootJSON), body, cfgArtifact, pol, nil)
		}
		addReal("real buildkit", realJSON, artifactPolicy(d))
		addReal("real buildkit wrong digest", realJSON, artifactPolicy(sha([]byte("x"))))
		edit := func(name string, f func(m map[string]any)) {
			c := clone(m)
			f(c)
			addReal(name, enc(c), artifactPolicy(d))
		}
		edit("real without tsa", func(m map[string]any) { delete(get(m, "verificationMaterial"), "timestampVerificationData") })
		edit("real without tlog", func(m map[string]any) { delete(get(m, "verificationMaterial"), "tlogEntries") })
		edit("real without promise", func(m map[string]any) { delete(entryAt(m, 0), "inclusionPromise") })
		edit("real without promise or tsa", func(m map[string]any) {
			delete(entryAt(m, 0), "inclusionPromise")
			delete(get(m, "verificationMaterial"), "timestampVerificationData")
		})
		edit("real integrated time moved", func(m map[string]any) { entryAt(m, 0)["integratedTime"] = "1774446093" })
		edit("real log index moved", func(m map[string]any) { entryAt(m, 0)["logIndex"] = "1180749978" })
		edit("real proof index moved", func(m map[string]any) { get(entryAt(m, 0), "inclusionProof")["logIndex"] = "1058845716" })
		edit("real proof hash flipped", func(m map[string]any) {
			hs := get(entryAt(m, 0), "inclusionProof")["hashes"].([]any)
			b, _ := base64.StdEncoding.DecodeString(hs[0].(string))
			b[0] ^= 1
			hs[0] = b64(b)
		})
		edit("real root hash flipped", func(m map[string]any) {
			p := get(entryAt(m, 0), "inclusionProof")
			b, _ := base64.StdEncoding.DecodeString(p["rootHash"].(string))
			b[0] ^= 1
			p["rootHash"] = b64(b)
		})
		edit("real checkpoint signature flipped", func(m map[string]any) {
			p := get(get(entryAt(m, 0), "inclusionProof"), "checkpoint")
			env := p["envelope"].(string)
			i := strings.LastIndex(env, " ")
			b, _ := base64.StdEncoding.DecodeString(strings.TrimSpace(env[i+1:]))
			b[len(b)-1] ^= 1
			p["envelope"] = env[:i+1] + b64(b) + "\n"
		})
		edit("real SET flipped", func(m map[string]any) {
			p := get(entryAt(m, 0), "inclusionPromise")
			b, _ := base64.StdEncoding.DecodeString(p["signedEntryTimestamp"].(string))
			b[len(b)-1] ^= 1
			p["signedEntryTimestamp"] = b64(b)
		})
		edit("real dsse signature flipped", func(m map[string]any) {
			s := get(m, "dsseEnvelope")["signatures"].([]any)[0].(map[string]any)
			b, _ := base64.StdEncoding.DecodeString(s["sig"].(string))
			b[len(b)-1] ^= 1
			s["sig"] = b64(b)
		})
		edit("real payload changed", func(m map[string]any) {
			e := get(m, "dsseEnvelope")
			b, _ := base64.StdEncoding.DecodeString(e["payload"].(string))
			b = append(b, ' ')
			e["payload"] = b64(b)
		})
		edit("real cert byte flipped", func(m map[string]any) {
			c := get(m, "verificationMaterial", "certificate")
			b, _ := base64.StdEncoding.DecodeString(c["rawBytes"].(string))
			b[len(b)-5] ^= 1
			c["rawBytes"] = b64(b)
		})
		edit("real tsa byte flipped", func(m map[string]any) {
			ts := get(m, "verificationMaterial", "timestampVerificationData")["rfc3161Timestamps"].([]any)[0].(map[string]any)
			b, _ := base64.StdEncoding.DecodeString(ts["signedTimestamp"].(string))
			b[len(b)-3] ^= 1
			ts["signedTimestamp"] = b64(b)
		})
		edit("real tsa byte flipped without promise", func(m map[string]any) {
			delete(entryAt(m, 0), "inclusionPromise")
			ts := get(m, "verificationMaterial", "timestampVerificationData")["rfc3161Timestamps"].([]any)[0].(map[string]any)
			b, _ := base64.StdEncoding.DecodeString(ts["signedTimestamp"].(string))
			b[len(b)-3] ^= 1
			ts["signedTimestamp"] = b64(b)
		})
		edit("real tsa time only (no promise), sct present", func(m map[string]any) {
			delete(entryAt(m, 0), "inclusionPromise")
		})
		edit("real tsa duplicated", func(m map[string]any) {
			td := get(m, "verificationMaterial", "timestampVerificationData")
			ts := td["rfc3161Timestamps"].([]any)
			td["rfc3161Timestamps"] = []any{ts[0], ts[0]}
		})
		edit("real body byte changed", func(m map[string]any) {
			e := entryAt(m, 0)
			b, _ := base64.StdEncoding.DecodeString(e["canonicalizedBody"].(string))
			e["canonicalizedBody"] = b64(bytes.Replace(b, []byte(`"dsse"`), []byte(`"dsse" `), 1))
		})
		edit("real as v0.2", func(m map[string]any) { m["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.2" })
		edit("real as v0.1", func(m map[string]any) { m["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.1" })
		edit("real log unknown", func(m map[string]any) {
			get(entryAt(m, 0), "logId")["keyId"] = b64(sha([]byte("unknown")))
		})
	}

	data, err := json.MarshalIndent(struct {
		Roots map[string]string `json:"roots"`
		Cases []*ocCase         `json:"cases"`
	}{roots, cases}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	ok, failed := 0, 0
	for _, c := range cases {
		if c.Error == "" {
			ok++
		} else {
			failed++
		}
	}
	t.Logf("%d cases: %d verified, %d refused", len(cases), ok, failed)
}

var currentCase string
