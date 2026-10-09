package zzshardstlog

// sigstore-go's answers (as buildx v0.37.1 vendors it: sigstore-go v1.2.2, rekor v1.5.3,
// rekor-tiles v2.3.0) for crates/sigstore/tests/tlog_oracle.rs: transparency log
// entries built here with test Rekor keys (Rekor v1 bodies of each registered kind,
// signed entry timestamps, RFC 6962 trees and signed checkpoints; Rekor v2 entries and
// C2SP notes) and mutated, each run through tlog.ParseTransparencyLogEntry and the
// functions sigstore-go's verifier calls on it. `generate-tlog` copies this file into
// buildx's cmd/zz_shards_tlog and runs it there.

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"math/big"
	"os"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/cyberphone/json-canonicalization/go/src/webpki.org/jsoncanonicalizer"
	protocommon "github.com/sigstore/protobuf-specs/gen/pb-go/common/v1"
	v1 "github.com/sigstore/protobuf-specs/gen/pb-go/rekor/v1"
	pb "github.com/sigstore/rekor-tiles/v2/pkg/generated/protobuf"
	tilesnote "github.com/sigstore/rekor-tiles/v2/pkg/note"
	"github.com/sigstore/rekor-tiles/v2/pkg/types/hashedrekord"
	rekorVerify "github.com/sigstore/rekor-tiles/v2/pkg/verify"
	"github.com/sigstore/sigstore-go/pkg/root"
	"github.com/sigstore/sigstore-go/pkg/tlog"
	"github.com/sigstore/sigstore/pkg/signature"
	"github.com/transparency-dev/merkle/rfc6962"
	sumdbnote "golang.org/x/mod/sumdb/note"
	"google.golang.org/protobuf/encoding/protojson"
)

type proofJSON struct {
	LogIndex   int64    `json:"logIndex"`
	RootHash   string   `json:"rootHash"`
	TreeSize   int64    `json:"treeSize"`
	Hashes     []string `json:"hashes"`
	Checkpoint *string  `json:"checkpoint"`
}

type tleJSON struct {
	LogIndex       int64      `json:"logIndex"`
	LogID          *string    `json:"logId"`
	KindVersion    []string   `json:"kindVersion"`
	IntegratedTime int64      `json:"integratedTime"`
	Promise        *string    `json:"promise"`
	Proof          *proofJSON `json:"proof"`
	Body           *string    `json:"body"`
}

type logJSON struct {
	ID      string `json:"id"`
	Key     string `json:"key"`
	Start   *int64 `json:"start"`
	End     *int64 `json:"end"`
	BaseURL string `json:"baseUrl"`
}

type v2JSON struct {
	Origin    string `json:"origin"`
	Digest    string `json:"digest"`
	Signature string `json:"signature"`
	// "publicKey" or "x509Certificate", and its raw bytes.
	VerifierKind string `json:"verifierKind"`
	VerifierRaw  string `json:"verifierRaw"`
	Details      string `json:"details"`
}

type want struct {
	Parse              string  `json:"parse"`
	Validate           string  `json:"validate"`
	IsV2               bool    `json:"isV2"`
	Signature          string  `json:"signature"`
	PublicKey          string  `json:"publicKey"`
	HashedRekordDigest string  `json:"hashedRekordDigest"`
	DssePayloadHash    string  `json:"dssePayloadHash"`
	Promise            bool    `json:"promise"`
	Proof              bool    `json:"proof"`
	V1STH              bool    `json:"v1sth"`
	SET                *string `json:"set"`
	Inclusion          *string `json:"inclusion"`
	EntryHash          *string `json:"entryHash"`
	V2                 *string `json:"v2"`
}

type tlogCase struct {
	Name string   `json:"name"`
	TLE  tleJSON  `json:"tle"`
	Log  *logJSON `json:"log"`
	V2   *v2JSON  `json:"v2,omitempty"`
	Want want     `json:"want"`
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

func strp(s string) *string { return &s }

func i64p(v int64) *int64 { return &v }

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

// The proto entry a case describes, empty implicit bytes unset as protojson leaves them.
func (j tleJSON) proto() *v1.TransparencyLogEntry {
	dec := func(s string) []byte {
		b := must(base64.StdEncoding.DecodeString(s))
		if len(b) == 0 {
			return nil
		}
		return b
	}
	tle := &v1.TransparencyLogEntry{LogIndex: j.LogIndex, IntegratedTime: j.IntegratedTime}
	if j.LogID != nil {
		tle.LogId = &protocommon.LogId{KeyId: dec(*j.LogID)}
	}
	if j.KindVersion != nil {
		tle.KindVersion = &v1.KindVersion{Kind: j.KindVersion[0], Version: j.KindVersion[1]}
	}
	if j.Promise != nil {
		tle.InclusionPromise = &v1.InclusionPromise{SignedEntryTimestamp: dec(*j.Promise)}
	}
	if j.Proof != nil {
		p := &v1.InclusionProof{LogIndex: j.Proof.LogIndex, RootHash: dec(j.Proof.RootHash), TreeSize: j.Proof.TreeSize}
		for _, h := range j.Proof.Hashes {
			p.Hashes = append(p.Hashes, dec(h))
		}
		if j.Proof.Checkpoint != nil {
			p.Checkpoint = &v1.Checkpoint{Envelope: *j.Proof.Checkpoint}
		}
		tle.InclusionProof = p
	}
	if j.Body != nil {
		tle.CanonicalizedBody = dec(*j.Body)
	}
	return tle
}

var treeIDSuffix = regexp.MustCompile(".* - [0-9]+$")

func hasRekorV1STH(entry *tlog.Entry) bool {
	lines := strings.Split(entry.TransparencyLogEntry().GetInclusionProof().GetCheckpoint().GetEnvelope(), "\n")
	return len(lines) >= 4 && treeIDSuffix.MatchString(lines[0])
}

func errText(err error) *string {
	if err == nil {
		return strp("")
	}
	return strp(err.Error())
}

// What sigstore-go makes of a case.
func run(c *tlogCase) {
	tle := c.TLE.proto()
	entry, err := tlog.ParseTransparencyLogEntry(tle)
	if err != nil {
		c.Want = want{Parse: err.Error()}
		return
	}
	w := want{}
	if err := tlog.ValidateEntry(entry); err != nil {
		w.Validate = err.Error()
	}
	w.IsV2 = entry.IsRekorV2()
	w.Signature = b64(entry.Signature())
	switch k := entry.PublicKey().(type) {
	case *x509.Certificate:
		w.PublicKey = "cert:" + b64(k.Raw)
	case nil:
	default:
		w.PublicKey = "key:" + b64(must(x509.MarshalPKIXPublicKey(k)))
	}
	if d, alg, ok := entry.GetHashedRekordDigest(); ok {
		w.HashedRekordDigest = hex.EncodeToString(d) + ":" + alg
	}
	if d, ok := entry.GetDssePayloadHash(); ok {
		w.DssePayloadHash = hex.EncodeToString(d)
	}
	w.Promise = entry.HasInclusionPromise()
	w.Proof = entry.HasInclusionProof()
	w.V1STH = hasRekorV1STH(entry)
	if c.Log != nil {
		key := must(x509.ParsePKIXPublicKey(must(base64.StdEncoding.DecodeString(c.Log.Key))))
		id := must(base64.StdEncoding.DecodeString(c.Log.ID))
		tl := &root.TransparencyLog{BaseURL: c.Log.BaseURL, ID: id, HashFunc: crypto.SHA256, PublicKey: key}
		switch pk := key.(type) {
		case *ecdsa.PublicKey:
			switch pk.Curve {
			case elliptic.P384():
				tl.SignatureHashFunc = crypto.SHA384
			case elliptic.P521():
				tl.SignatureHashFunc = crypto.SHA512
			default:
				tl.SignatureHashFunc = crypto.SHA256
			}
		case ed25519.PublicKey:
			tl.SignatureHashFunc = crypto.SHA512
		default:
			tl.SignatureHashFunc = crypto.SHA256
		}
		if c.Log.Start != nil {
			tl.ValidityPeriodStart = time.Unix(*c.Log.Start, 0)
		}
		if c.Log.End != nil {
			tl.ValidityPeriodEnd = time.Unix(*c.Log.End, 0)
		}
		w.SET = errText(tlog.VerifySET(entry, map[string]*root.TransparencyLog{hex.EncodeToString(id): tl}))
		verifier := must(signature.LoadVerifier(key, tl.SignatureHashFunc))
		if !entry.IsRekorV2() && entry.HasInclusionProof() {
			w.Inclusion = errText(tlog.VerifyInclusion(entry, verifier))
		}
		if c.V2 != nil {
			v := &pb.Verifier{}
			raw := must(base64.StdEncoding.DecodeString(c.V2.VerifierRaw))
			if c.V2.VerifierKind == "publicKey" {
				v.Verifier = &pb.Verifier_PublicKey{PublicKey: &pb.PublicKey{RawBytes: raw}}
			} else {
				v.Verifier = &pb.Verifier_X509Certificate{X509Certificate: &protocommon.X509Certificate{RawBytes: raw}}
			}
			v.KeyDetails = protocommon.PublicKeyDetails(protocommon.PublicKeyDetails_value[c.V2.Details])
			h, err := hashedrekord.ToEntryHash(must(base64.StdEncoding.DecodeString(c.V2.Digest)), &pb.Signature{
				Content:  must(base64.StdEncoding.DecodeString(c.V2.Signature)),
				Verifier: v,
			})
			if err != nil {
				w.EntryHash = strp("error: " + err.Error())
			} else {
				w.EntryHash = strp(b64(h))
				nv, err := tilesnote.NewNoteVerifier(c.V2.Origin, verifier)
				if err != nil {
					w.V2 = strp(fmt.Sprintf("loading note verifier: %v", err))
				} else if err := rekorVerify.VerifyLogEntryWithHash(tle, nv, h); err != nil {
					w.V2 = strp(fmt.Sprintf("verifying log entry: %v", err))
				} else {
					w.V2 = strp("")
				}
			}
		}
	}
	c.Want = w
}

// RFC 6962 trees.
func mth(leaves [][]byte) []byte {
	if len(leaves) == 1 {
		return leaves[0]
	}
	k := 1
	for k*2 < len(leaves) {
		k *= 2
	}
	return rfc6962.DefaultHasher.HashChildren(mth(leaves[:k]), mth(leaves[k:]))
}

func path(m int, leaves [][]byte) [][]byte {
	if len(leaves) == 1 {
		return nil
	}
	k := 1
	for k*2 < len(leaves) {
		k *= 2
	}
	if m < k {
		return append(path(m, leaves[:k]), mth(leaves[k:]))
	}
	return append(path(m-k, leaves[k:]), mth(leaves[:k]))
}

// A tree of `size` leaves with `leaf` at `index`: its root and the proof.
func tree(leaf []byte, index, size int) ([]byte, [][]byte) {
	leaves := make([][]byte, size)
	for i := range leaves {
		if i == index {
			leaves[i] = leaf
		} else {
			leaves[i] = rfc6962.DefaultHasher.HashLeaf([]byte(fmt.Sprintf("leaf %d", i)))
		}
	}
	return mth(leaves), path(index, leaves)
}

type rekorV1 struct {
	priv   *ecdsa.PrivateKey
	id     []byte
	origin string
}

func newRekorV1() *rekorV1 {
	priv := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	der := must(x509.MarshalPKIXPublicKey(&priv.PublicKey))
	id := sha256.Sum256(der)
	return &rekorV1{priv: priv, id: id[:], origin: "rekor.sigstore.dev - 1193050959916656506"}
}

func (r *rekorV1) log() *logJSON {
	return &logJSON{
		ID:      b64(r.id),
		Key:     b64(must(x509.MarshalPKIXPublicKey(&r.priv.PublicKey))),
		Start:   i64p(1_600_000_000),
		BaseURL: "https://rekor.sigstore.dev",
	}
}

// The SET: ECDSA over the canonical JSON of the payload.
func (r *rekorV1) set(body []byte, integrated, index int64, logID []byte) []byte {
	payload := must(json.Marshal(tlog.RekorPayload{
		Body:           base64.StdEncoding.EncodeToString(body),
		IntegratedTime: integrated,
		LogIndex:       index,
		LogID:          hex.EncodeToString(logID),
	}))
	canon := must(jsoncanonicalizer.Transform(payload))
	h := sha256.Sum256(canon)
	return must(ecdsa.SignASN1(rand.Reader, r.priv, h[:]))
}

// A signed checkpoint in rekor v1's format.
func (r *rekorV1) checkpoint(text string, name string) string {
	h := sha256.Sum256([]byte(text))
	sig := must(ecdsa.SignASN1(rand.Reader, r.priv, h[:]))
	der := must(x509.MarshalPKIXPublicKey(&r.priv.PublicKey))
	kh := sha256.Sum256(der)
	return text + "\n— " + name + " " + b64(append(kh[:4:4], sig...)) + "\n"
}

// A complete Rekor v1 entry around `body`.
func (r *rekorV1) entry(kind, version string, body []byte) tleJSON {
	const integrated = 1_774_446_092
	index, size := 5, 13
	leaf := rfc6962.DefaultHasher.HashLeaf(body)
	rootHash, proof := tree(leaf, index, size)
	var hashes []string
	for _, h := range proof {
		hashes = append(hashes, b64(h))
	}
	text := fmt.Sprintf("%s\n%d\n%s\n", r.origin, size, b64(rootHash))
	cp := r.checkpoint(text, "rekor.sigstore.dev")
	set := b64(r.set(body, integrated, 1_180_749_977, r.id))
	return tleJSON{
		LogIndex:       1_180_749_977,
		LogID:          strp(b64(r.id)),
		KindVersion:    []string{kind, version},
		IntegratedTime: integrated,
		Promise:        &set,
		Proof:          &proofJSON{LogIndex: int64(index), RootHash: b64(rootHash), TreeSize: int64(size), Hashes: hashes, Checkpoint: &cp},
		Body:           strp(b64(body)),
	}
}

type signer struct {
	name string
	priv crypto.Signer
	pem  []byte
	// Signs a digest of `h`, as hashedrekord's signer does.
	signDigest func(digest []byte, h crypto.Hash) []byte
	// Signs a message (for DSSE), as LoadVerifier(key, SHA256) verifies.
	signMessage func(msg []byte) []byte
}

func pemKey(pub crypto.PublicKey) []byte {
	return pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: must(x509.MarshalPKIXPublicKey(pub))})
}

func signers() []signer {
	p256 := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	p384 := must(ecdsa.GenerateKey(elliptic.P384(), rand.Reader))
	rsaKey := must(rsa.GenerateKey(rand.Reader, 2048))
	_, edKey := must2(ed25519.GenerateKey(rand.Reader))
	ecdsaSigner := func(k *ecdsa.PrivateKey) signer {
		return signer{
			priv:       k,
			pem:        pemKey(&k.PublicKey),
			signDigest: func(d []byte, _ crypto.Hash) []byte { return must(ecdsa.SignASN1(rand.Reader, k, d)) },
			signMessage: func(m []byte) []byte {
				h := sha256.Sum256(m)
				return must(ecdsa.SignASN1(rand.Reader, k, h[:]))
			},
		}
	}
	s1 := ecdsaSigner(p256)
	s1.name = "p256"
	s2 := ecdsaSigner(p384)
	s2.name = "p384"
	s3 := signer{
		name: "rsa2048",
		priv: rsaKey,
		pem:  pemKey(&rsaKey.PublicKey),
		signDigest: func(d []byte, h crypto.Hash) []byte {
			return must(rsa.SignPKCS1v15(rand.Reader, rsaKey, h, d))
		},
		signMessage: func(m []byte) []byte {
			h := sha256.Sum256(m)
			return must(rsa.SignPKCS1v15(rand.Reader, rsaKey, crypto.SHA256, h[:]))
		},
	}
	s4 := signer{
		name: "ed25519",
		priv: edKey,
		pem:  pemKey(edKey.Public()),
		signDigest: func(d []byte, _ crypto.Hash) []byte {
			if len(d) != 64 {
				// Ed25519ph takes SHA-512 digests only: a pure signature stands in.
				return ed25519.Sign(edKey, d)
			}
			return must(edKey.Sign(rand.Reader, d, &ed25519.Options{Hash: crypto.SHA512}))
		},
		signMessage: func(m []byte) []byte { return ed25519.Sign(edKey, m) },
	}
	// A self-signed certificate for the P-256 key.
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(7),
		Subject:               pkix.Name{CommonName: "shards test"},
		NotBefore:             time.Unix(1_700_000_000, 0),
		NotAfter:              time.Unix(1_900_000_000, 0),
		BasicConstraintsValid: true,
		IsCA:                  true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageDigitalSignature,
	}
	certDER := must(x509.CreateCertificate(rand.Reader, tmpl, tmpl, &p256.PublicKey, p256))
	s5 := ecdsaSigner(p256)
	s5.name = "cert"
	s5.pem = pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: certDER})
	return []signer{s1, s2, s3, s4, s5}
}

func must2[A, B any](a A, b B, err error) (A, B) {
	if err != nil {
		panic(err)
	}
	return a, b
}

func compact(v any) []byte {
	b := must(json.Marshal(v))
	return b
}

// A hashedrekord v0.0.1 body.
func hashedRekordBody(s signer, alg string, h crypto.Hash, payload []byte) []byte {
	hh := h.New()
	hh.Write(payload)
	d := hh.Sum(nil)
	return compact(map[string]any{
		"apiVersion": "0.0.1",
		"kind":       "hashedrekord",
		"spec": map[string]any{
			"data": map[string]any{"hash": map[string]any{"algorithm": alg, "value": hex.EncodeToString(d)}},
			"signature": map[string]any{
				"content":   b64(s.signDigest(d, h)),
				"publicKey": map[string]any{"content": b64(s.pem)},
			},
		},
	})
}

func pae(t string, p []byte) []byte {
	return []byte(fmt.Sprintf("DSSEv1 %d %s %d %s", len(t), t, len(p), p))
}

// A DSSE envelope's JSON, its signature by `s`.
func envelope(s signer, payload []byte) (string, string) {
	sig := b64(s.signMessage(pae("application/vnd.in-toto+json", payload)))
	env := compact(map[string]any{
		"payloadType": "application/vnd.in-toto+json",
		"payload":     b64(payload),
		"signatures":  []any{map[string]any{"keyid": "", "sig": sig}},
	})
	return string(env), sig
}

// A canonical dsse v0.0.1 body (as Rekor logs it).
func dsseBody(s signer, payload []byte) []byte {
	env, sig := envelope(s, payload)
	ph := sha256.Sum256(payload)
	eh := sha256.Sum256([]byte(env))
	return compact(map[string]any{
		"apiVersion": "0.0.1",
		"kind":       "dsse",
		"spec": map[string]any{
			"envelopeHash": map[string]any{"algorithm": "sha256", "value": hex.EncodeToString(eh[:])},
			"payloadHash":  map[string]any{"algorithm": "sha256", "value": hex.EncodeToString(ph[:])},
			"signatures":   []any{map[string]any{"signature": sig, "verifier": b64(s.pem)}},
		},
	})
}

// A dsse v0.0.1 body in its proposed form.
func dsseProposed(s signer, payload []byte, verifiers ...[]byte) []byte {
	env, _ := envelope(s, payload)
	var vs []any
	for _, v := range verifiers {
		vs = append(vs, b64(v))
	}
	return compact(map[string]any{
		"apiVersion": "0.0.1",
		"kind":       "dsse",
		"spec": map[string]any{
			"proposedContent": map[string]any{"envelope": env, "verifiers": vs},
		},
	})
}

// An intoto v0.0.2 body, canonical (no payload) or with its payload.
func intotoBody(s signer, payload []byte, withPayload bool) []byte {
	sig := b64(s.signMessage(pae("application/vnd.in-toto+json", payload)))
	ph := sha256.Sum256(payload)
	env := map[string]any{
		"payloadType": "application/vnd.in-toto+json",
		"signatures":  []any{map[string]any{"publicKey": b64(s.pem), "sig": b64([]byte(sig))}},
	}
	if withPayload {
		env["payload"] = b64([]byte(b64(payload)))
	}
	return compact(map[string]any{
		"apiVersion": "0.0.2",
		"kind":       "intoto",
		"spec": map[string]any{
			"content": map[string]any{
				"envelope":    env,
				"payloadHash": map[string]any{"algorithm": "sha256", "value": hex.EncodeToString(ph[:])},
			},
		},
	})
}

// A JSON edit of a body: its spec map changed by `f`.
func editSpec(body []byte, f func(spec map[string]any)) []byte {
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		panic(err)
	}
	f(m["spec"].(map[string]any))
	return compact(m)
}

func edit(body []byte, f func(m map[string]any)) []byte {
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		panic(err)
	}
	f(m)
	return compact(m)
}

func sub(m map[string]any, path ...string) map[string]any {
	for _, p := range path {
		m = m[p].(map[string]any)
	}
	return m
}

func firstOf(m map[string]any, key string) map[string]any {
	return m[key].([]any)[0].(map[string]any)
}

type rekorV2 struct {
	name   string
	signer signature.SignerVerifier
	pub    crypto.PublicKey
	id     []byte
	origin string
}

func newRekorV2(kind string) *rekorV2 {
	r := &rekorV2{name: kind, origin: "log2025-1.rekor.sigstore.dev"}
	switch kind {
	case "ed25519":
		_, priv := must2(ed25519.GenerateKey(rand.Reader))
		r.signer = must(signature.LoadED25519SignerVerifier(priv))
		r.pub = priv.Public()
	default:
		priv := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
		r.signer = must(signature.LoadECDSASignerVerifier(priv, crypto.SHA256))
		r.pub = &priv.PublicKey
	}
	_, logID := must2(tilesnote.KeyHash(r.origin, r.pub))
	r.id = logID
	return r
}

func (r *rekorV2) log() *logJSON {
	return &logJSON{
		ID:      b64(r.id),
		Key:     b64(must(x509.MarshalPKIXPublicKey(r.pub))),
		Start:   i64p(1_600_000_000),
		BaseURL: "https://" + r.origin,
	}
}

func (r *rekorV2) checkpoint(text string) string {
	ns := must(tilesnote.NewNoteSigner(context.Background(), r.origin, r.signer))
	return string(must(sumdbnote.Sign(&sumdbnote.Note{Text: text}, ns)))
}

// A Rekor v2 hashedrekord entry by `s`, and its v2 description.
func (r *rekorV2) entry(s signer, details string, h crypto.Hash, payload []byte, cert bool) (tleJSON, *v2JSON) {
	hh := h.New()
	hh.Write(payload)
	d := hh.Sum(nil)
	sig := s.signDigest(d, h)
	block, _ := pem.Decode(s.pem)
	kind := "publicKey"
	if cert {
		kind = "x509Certificate"
	}
	ver := &pb.Verifier{KeyDetails: protocommon.PublicKeyDetails(protocommon.PublicKeyDetails_value[details])}
	if cert {
		ver.Verifier = &pb.Verifier_X509Certificate{X509Certificate: &protocommon.X509Certificate{RawBytes: block.Bytes}}
	} else {
		ver.Verifier = &pb.Verifier_PublicKey{PublicKey: &pb.PublicKey{RawBytes: block.Bytes}}
	}
	alg := map[crypto.Hash]protocommon.HashAlgorithm{crypto.SHA256: protocommon.HashAlgorithm_SHA2_256, crypto.SHA384: protocommon.HashAlgorithm_SHA2_384, crypto.SHA512: protocommon.HashAlgorithm_SHA2_512}[h]
	e := &pb.Entry{
		Kind:       "hashedrekord",
		ApiVersion: "0.0.2",
		Spec: &pb.Spec{Spec: &pb.Spec_HashedRekordV002{HashedRekordV002: &pb.HashedRekordLogEntryV002{
			Data:      &protocommon.HashOutput{Digest: d, Algorithm: alg},
			Signature: &pb.Signature{Content: sig, Verifier: ver},
		}}},
	}
	body := must(jsoncanonicalizer.Transform(must(protojson.Marshal(e))))
	leaf := must(hashedrekord.ToEntryHash(d, &pb.Signature{Content: sig, Verifier: ver}))
	index, size := 6, 9
	rootHash, proof := tree(leaf, index, size)
	var hashes []string
	for _, x := range proof {
		hashes = append(hashes, b64(x))
	}
	text := fmt.Sprintf("%s\n%d\n%s\n", r.origin, size, b64(rootHash))
	cp := r.checkpoint(text)
	return tleJSON{
			LogIndex:       int64(index),
			LogID:          strp(b64(r.id)),
			KindVersion:    []string{"hashedrekord", "0.0.2"},
			IntegratedTime: 0,
			Proof:          &proofJSON{LogIndex: int64(index), RootHash: b64(rootHash), TreeSize: int64(size), Hashes: hashes, Checkpoint: &cp},
			Body:           strp(b64(body)),
		}, &v2JSON{
			Origin:       r.origin,
			Digest:       b64(d),
			Signature:    b64(sig),
			VerifierKind: kind,
			VerifierRaw:  b64(block.Bytes),
			Details:      details,
		}
}

func TestShardsTlogOracle(t *testing.T) {
	out := os.Getenv("SHARDS_TLOG_OUT")
	if out == "" {
		t.Skip("SHARDS_TLOG_OUT not set")
	}
	var cases []*tlogCase
	add := func(name string, tle tleJSON, log *logJSON) *tlogCase {
		c := &tlogCase{Name: name, TLE: tle, Log: log}
		cases = append(cases, c)
		return c
	}
	r := newRekorV1()
	ss := signers()
	payload := []byte(`{"_type":"https://in-toto.io/Statement/v1","subject":[],"predicateType":"x"}`)

	// Each kind, by each signer.
	for _, s := range ss {
		add("hashedrekord sha256 "+s.name, r.entry("hashedrekord", "0.0.1", hashedRekordBody(s, "sha256", crypto.SHA256, payload)), r.log())
		add("dsse "+s.name, r.entry("dsse", "0.0.1", dsseBody(s, payload)), r.log())
		add("intoto "+s.name, r.entry("intoto", "0.0.2", intotoBody(s, payload, false)), r.log())
		add("dsse proposed "+s.name, r.entry("dsse", "0.0.1", dsseProposed(s, payload, s.pem)), r.log())
		add("intoto with payload "+s.name, r.entry("intoto", "0.0.2", intotoBody(s, payload, true)), r.log())
	}
	p256, p384, rsaS, ed := ss[0], ss[1], ss[2], ss[3]
	add("hashedrekord sha384", r.entry("hashedrekord", "0.0.1", hashedRekordBody(p384, "sha384", crypto.SHA384, payload)), r.log())
	add("hashedrekord sha512 rsa", r.entry("hashedrekord", "0.0.1", hashedRekordBody(rsaS, "sha512", crypto.SHA512, payload)), r.log())
	add("hashedrekord ed25519 sha512", r.entry("hashedrekord", "0.0.1", hashedRekordBody(ed, "sha512", crypto.SHA512, payload)), r.log())

	good := hashedRekordBody(p256, "sha256", crypto.SHA256, payload)
	goodDsse := dsseBody(p256, payload)
	goodIntoto := intotoBody(p256, payload, false)
	body := func(name string, b []byte, kind, version string) {
		add(name, r.entry(kind, version, b), r.log())
	}
	raw := func(name string, b string) {
		body(name, []byte(b), "hashedrekord", "0.0.1")
	}

	// Bodies that are not entries.
	raw("empty body", "")
	raw("white space body", "   ")
	raw("truncated body", `{"kind":"hashedrekord"`)
	raw("syntax error", `{"kind":hashedrekord}`)
	raw("array body", `[1,2]`)
	raw("string body", `"x"`)
	raw("number body", `12`)
	raw("null body", `null`)
	raw("trailing garbage", string(good)+"xyz")
	raw("kind missing", `{"apiVersion":"0.0.1","spec":{}}`)
	raw("kind empty", `{"kind":"","apiVersion":"0.0.1"}`)
	raw("kind number", `{"kind":5}`)
	raw("kind object", `{"kind":{}}`)
	raw("kind unknown", `{"kind":"nope"}`)
	raw("kind uppercase key", `{"KIND":"dsse","apiVersion":"0.0.1","spec":{}}`)
	raw("kind kelvin key", "{\"Kind\":\"dsse\",\"apiVersion\":\"0.0.1\",\"spec\":{}}")
	raw("kind repeated", `{"kind":"dsse","kind":"rekord"}`)
	raw("kind ProposedEntry", `{"kind":"ProposedEntry"}`)
	raw("kind rekord", `{"kind":"rekord","apiVersion":"0.0.1","spec":{}}`)
	raw("kind tuf bad api", `{"kind":"tuf","apiVersion":7}`)
	raw("api version missing", `{"kind":"dsse","spec":{}}`)
	raw("api version null", `{"kind":"dsse","apiVersion":null,"spec":{}}`)
	raw("api version number", `{"kind":"dsse","apiVersion":1,"spec":{}}`)
	raw("api version wrong", `{"kind":"dsse","apiVersion":"0.0.2","spec":{}}`)
	raw("api version empty", `{"kind":"dsse","apiVersion":"","spec":{}}`)
	raw("api version short", `{"kind":"dsse","apiVersion":"0.1","spec":{}}`)
	raw("api version leading zero", `{"kind":"dsse","apiVersion":"0.00.1","spec":{}}`)
	raw("api version prerelease", `{"kind":"dsse","apiVersion":"0.0.1-rc.1","spec":{}}`)
	body("api version build", edit(good, func(m map[string]any) { m["apiVersion"] = "0.0.1+x" }), "hashedrekord", "0.0.1")
	raw("api version bad build", `{"kind":"dsse","apiVersion":"0.0.1+","spec":{}}`)
	raw("intoto 0.0.1", `{"kind":"intoto","apiVersion":"0.0.1","spec":{}}`)
	raw("spec missing", `{"kind":"dsse","apiVersion":"0.0.1"}`)
	raw("spec string", `{"kind":"dsse","apiVersion":"0.0.1","spec":"x"}`)
	raw("spec array", `{"kind":"hashedrekord","apiVersion":"0.0.1","spec":[]}`)
	raw("spec number", `{"kind":"intoto","apiVersion":"0.0.2","spec":1.5}`)
	raw("spec bool", `{"kind":"intoto","apiVersion":"0.0.2","spec":true}`)

	// hashedrekord's validation and signature.
	hr := func(name string, f func(spec map[string]any)) {
		body("hashedrekord: "+name, editSpec(good, f), "hashedrekord", "0.0.1")
	}
	hr("no data", func(s map[string]any) { delete(s, "data") })
	hr("data without hash", func(s map[string]any) { s["data"] = map[string]any{} })
	hr("no algorithm", func(s map[string]any) { delete(sub(s, "data", "hash"), "algorithm") })
	hr("algorithm md5", func(s map[string]any) { sub(s, "data", "hash")["algorithm"] = "md5" })
	hr("algorithm number", func(s map[string]any) { sub(s, "data", "hash")["algorithm"] = 1 })
	hr("no value", func(s map[string]any) { delete(sub(s, "data", "hash"), "value") })
	hr("short value", func(s map[string]any) { sub(s, "data", "hash")["value"] = "abcd" })
	hr("non-hex value", func(s map[string]any) { sub(s, "data", "hash")["value"] = strings.Repeat("g", 64) })
	hr("odd value", func(s map[string]any) {
		v := sub(s, "data", "hash")["value"].(string)
		sub(s, "data", "hash")["value"] = v[:63] + "Z"
	})
	hr("other digest", func(s map[string]any) { sub(s, "data", "hash")["value"] = strings.Repeat("ab", 32) })
	hr("sha384 named for a sha256 digest", func(s map[string]any) { sub(s, "data", "hash")["algorithm"] = "sha384" })
	hr("no signature", func(s map[string]any) { delete(s, "signature") })
	hr("no content", func(s map[string]any) { delete(sub(s, "signature"), "content") })
	hr("bad content base64", func(s map[string]any) { sub(s, "signature")["content"] = "AB!D" })
	hr("short content base64", func(s map[string]any) { sub(s, "signature")["content"] = "ABC" })
	hr("wrong content", func(s map[string]any) { sub(s, "signature")["content"] = b64([]byte("not a signature")) })
	hr("no public key", func(s map[string]any) { delete(sub(s, "signature"), "publicKey") })
	hr("empty public key", func(s map[string]any) { sub(s, "signature")["publicKey"] = map[string]any{} })
	hr("public key not pem", func(s map[string]any) { sub(s, "signature", "publicKey")["content"] = b64([]byte("hello")) })
	hr("public key bad base64", func(s map[string]any) { sub(s, "signature", "publicKey")["content"] = "@@@@" })
	hr("public key other type", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: []byte{1}}))
	})
	hr("public key garbage der", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: []byte{0x30, 0x03, 2, 1, 1}}))
	})
	hr("public key trailing der", func(s map[string]any) {
		block, _ := pem.Decode(p256.pem)
		sub(s, "signature", "publicKey")["content"] = b64(pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: append(block.Bytes, 0)}))
	})
	hr("public key pkcs1", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: x509.MarshalPKCS1PublicKey(&rsaS.priv.(*rsa.PrivateKey).PublicKey)}))
	})
	hr("other key", func(s map[string]any) { sub(s, "signature", "publicKey")["content"] = b64(p384.pem) })
	hr("certificate garbage", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: []byte{0x30, 0}}))
	})
	hr("certificate chain", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(append(append([]byte{}, ss[4].pem...), ss[4].pem...))
	})
	hr("chain with junk", func(s map[string]any) {
		sub(s, "signature", "publicKey")["content"] = b64(append(append([]byte{}, ss[4].pem...), []byte("junk\n")...))
	})
	hr("spec extra field", func(s map[string]any) { s["extra"] = 1 })

	// dsse's.
	ds := func(name string, f func(spec map[string]any)) {
		body("dsse: "+name, editSpec(goodDsse, f), "dsse", "0.0.1")
	}
	ds("no envelope hash", func(s map[string]any) { delete(s, "envelopeHash") })
	ds("no payload hash", func(s map[string]any) { delete(s, "payloadHash") })
	ds("no signatures", func(s map[string]any) { delete(s, "signatures") })
	ds("empty signatures", func(s map[string]any) { s["signatures"] = []any{} })
	ds("signatures not array", func(s map[string]any) { s["signatures"] = "x" })
	ds("signature item not object", func(s map[string]any) { s["signatures"] = []any{"x"} })
	ds("envelope hash md5", func(s map[string]any) { sub(s, "envelopeHash")["algorithm"] = "md5" })
	ds("envelope hash no algorithm", func(s map[string]any) { delete(sub(s, "envelopeHash"), "algorithm") })
	ds("envelope hash no value", func(s map[string]any) { delete(sub(s, "envelopeHash"), "value") })
	ds("envelope hash empty", func(s map[string]any) { s["envelopeHash"] = map[string]any{} })
	ds("payload hash md5", func(s map[string]any) { sub(s, "payloadHash")["algorithm"] = "md5" })
	ds("payload hash bad hex", func(s map[string]any) { sub(s, "payloadHash")["value"] = "zz" })
	ds("two failures", func(s map[string]any) {
		sub(s, "envelopeHash")["algorithm"] = "md5"
		delete(sub(s, "payloadHash"), "value")
	})
	ds("signature missing", func(s map[string]any) { delete(firstOf(s, "signatures"), "signature") })
	ds("signature bad pattern", func(s map[string]any) { firstOf(s, "signatures")["signature"] = "abc" })
	ds("signature url alphabet", func(s map[string]any) { firstOf(s, "signatures")["signature"] = "ab-_" })
	ds("signature undecodable", func(s map[string]any) { firstOf(s, "signatures")["signature"] = "YWJj" })
	ds("verifier missing", func(s map[string]any) { delete(firstOf(s, "signatures"), "verifier") })
	ds("verifier empty", func(s map[string]any) { firstOf(s, "signatures")["verifier"] = "" })
	ds("verifier bad base64", func(s map[string]any) { firstOf(s, "signatures")["verifier"] = "!!!!" })
	ds("verifier not pem", func(s map[string]any) { firstOf(s, "signatures")["verifier"] = b64([]byte("x")) })
	ds("second item bad", func(s map[string]any) {
		s["signatures"] = append(s["signatures"].([]any), map[string]any{"signature": "abc"})
	})
	ds("proposed and hashes", func(s map[string]any) {
		s["proposedContent"] = map[string]any{"envelope": "{}", "verifiers": []any{b64(p256.pem)}}
	})
	prop := dsseProposed(p256, payload, p256.pem)
	dp := func(name string, f func(spec map[string]any)) {
		body("dsse proposed: "+name, editSpec(prop, f), "dsse", "0.0.1")
	}
	dp("no envelope", func(s map[string]any) { delete(sub(s, "proposedContent"), "envelope") })
	dp("no verifiers", func(s map[string]any) { delete(sub(s, "proposedContent"), "verifiers") })
	dp("empty verifiers", func(s map[string]any) { sub(s, "proposedContent")["verifiers"] = []any{} })
	dp("empty-string verifier", func(s map[string]any) { sub(s, "proposedContent")["verifiers"] = []any{""} })
	dp("bad verifier base64", func(s map[string]any) { sub(s, "proposedContent")["verifiers"] = []any{"a"} })
	dp("wrong verifier", func(s map[string]any) { sub(s, "proposedContent")["verifiers"] = []any{b64(p384.pem)} })
	dp("two verifiers", func(s map[string]any) {
		sub(s, "proposedContent")["verifiers"] = []any{b64(p256.pem), b64(p384.pem)}
	})
	dp("verifier not pem", func(s map[string]any) { sub(s, "proposedContent")["verifiers"] = []any{b64([]byte("x"))} })
	dp("envelope not json", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = "{" })
	dp("envelope trailing", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = "{} x" })
	dp("envelope array", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = "[]" })
	dp("envelope null", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = "null" })
	dp("envelope no signatures", func(s map[string]any) {
		sub(s, "proposedContent")["envelope"] = `{"payloadType":"x","payload":"","signatures":[]}`
	})
	dp("envelope type error", func(s map[string]any) {
		sub(s, "proposedContent")["envelope"] = `{"payloadType":1,"signatures":[{"sig":2}]}`
	})
	dp("envelope signatures object", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = `{"signatures":{}}` })
	dp("envelope signature string", func(s map[string]any) { sub(s, "proposedContent")["envelope"] = `{"signatures":["x"]}` })
	dp("envelope bad payload", func(s map[string]any) {
		env, _ := envelope(p256, payload)
		sub(s, "proposedContent")["envelope"] = strings.Replace(env, `"payload":"`, `"payload":"!`, 1)
	})
	dp("envelope bad sig", func(s map[string]any) {
		sub(s, "proposedContent")["envelope"] = `{"payloadType":"x","payload":"YQ==","signatures":[{"sig":"!"}]}`
	})
	dp("envelope extra signature", func(s map[string]any) {
		var e map[string]any
		json.Unmarshal([]byte(sub(s, "proposedContent")["envelope"].(string)), &e)
		e["signatures"] = append(e["signatures"].([]any), map[string]any{"sig": b64([]byte("x"))})
		sub(s, "proposedContent")["envelope"] = string(compact(e))
	})
	dp("envelope case-folded keys", func(s map[string]any) {
		sub(s, "proposedContent")["envelope"] = strings.Replace(strings.Replace(sub(s, "proposedContent")["envelope"].(string), `"payloadType"`, `"PAYLOADTYPE"`, 1), `"sig"`, `"SIG"`, 1)
	})

	// intoto's.
	it := func(name string, f func(spec map[string]any)) {
		body("intoto: "+name, editSpec(goodIntoto, f), "intoto", "0.0.2")
	}
	it("no content", func(s map[string]any) { delete(s, "content") })
	it("no envelope", func(s map[string]any) { delete(sub(s, "content"), "envelope") })
	it("no payload type", func(s map[string]any) { delete(sub(s, "content", "envelope"), "payloadType") })
	it("no signatures", func(s map[string]any) { delete(sub(s, "content", "envelope"), "signatures") })
	it("empty signatures", func(s map[string]any) { sub(s, "content", "envelope")["signatures"] = []any{} })
	it("signature no key", func(s map[string]any) { delete(firstOf(sub(s, "content", "envelope"), "signatures"), "publicKey") })
	it("signature no sig", func(s map[string]any) { delete(firstOf(sub(s, "content", "envelope"), "signatures"), "sig") })
	it("signature empty", func(s map[string]any) { sub(s, "content", "envelope")["signatures"] = []any{map[string]any{}} })
	it("sig bad base64", func(s map[string]any) { firstOf(sub(s, "content", "envelope"), "signatures")["sig"] = "a" })
	it("key bad base64", func(s map[string]any) { firstOf(sub(s, "content", "envelope"), "signatures")["publicKey"] = "a" })
	it("payload hash md5", func(s map[string]any) { sub(s, "content", "payloadHash")["algorithm"] = "md5" })
	it("hash and payload hash bad", func(s map[string]any) {
		sub(s, "content")["hash"] = map[string]any{"algorithm": "sha1"}
		delete(sub(s, "content", "payloadHash"), "value")
	})
	it("envelope and hash bad", func(s map[string]any) {
		delete(sub(s, "content", "envelope"), "payloadType")
		sub(s, "content")["hash"] = map[string]any{}
	})
	it("no payload hash", func(s map[string]any) { delete(sub(s, "content"), "payloadHash") })
	it("payload hash bad hex", func(s map[string]any) { sub(s, "content", "payloadHash")["value"] = "q" })
	it("sig not double encoded", func(s map[string]any) {
		firstOf(sub(s, "content", "envelope"), "signatures")["sig"] = b64([]byte("!!!"))
	})
	withPayload := intotoBody(p256, payload, true)
	ip := func(name string, f func(spec map[string]any)) {
		body("intoto payload: "+name, editSpec(withPayload, f), "intoto", "0.0.2")
	}
	ip("payload not base64 inside", func(s map[string]any) { sub(s, "content", "envelope")["payload"] = b64([]byte("!!")) })
	ip("payload url inside", func(s map[string]any) {
		sub(s, "content", "envelope")["payload"] = b64([]byte(base64.URLEncoding.EncodeToString([]byte("\xfb\xff"))))
	})
	ip("wrong key", func(s map[string]any) {
		firstOf(sub(s, "content", "envelope"), "signatures")["publicKey"] = b64(p384.pem)
	})
	ip("key not pem", func(s map[string]any) {
		firstOf(sub(s, "content", "envelope"), "signatures")["publicKey"] = b64([]byte("x"))
	})
	ip("sig bad inner", func(s map[string]any) { firstOf(sub(s, "content", "envelope"), "signatures")["sig"] = b64([]byte("!")) })
	ip("two signatures one key wrong", func(s map[string]any) {
		e := sub(s, "content", "envelope")
		e["signatures"] = append(e["signatures"].([]any), map[string]any{"publicKey": b64(p384.pem), "sig": b64([]byte(b64([]byte("x"))))})
	})

	// The entry around the body.
	base := r.entry("hashedrekord", "0.0.1", good)
	tl := func(name string, f func(e *tleJSON), log *logJSON) {
		e := base
		if base.Proof != nil {
			p := *base.Proof
			p.Hashes = append([]string{}, base.Proof.Hashes...)
			e.Proof = &p
		}
		f(&e)
		add("entry: "+name, e, log)
	}
	tl("no body", func(e *tleJSON) { e.Body = nil }, r.log())
	tl("negative index", func(e *tleJSON) { e.LogIndex = -1 }, r.log())
	tl("no log id", func(e *tleJSON) { e.LogID = nil }, r.log())
	tl("empty key id", func(e *tleJSON) { e.LogID = strp("") }, r.log())
	tl("no kind version", func(e *tleJSON) { e.KindVersion = nil }, r.log())
	tl("kind version mismatch", func(e *tleJSON) { e.KindVersion = []string{"dsse", "9"} }, r.log())
	tl("proof without checkpoint", func(e *tleJSON) { e.Proof.Checkpoint = nil }, r.log())
	tl("proof with empty checkpoint", func(e *tleJSON) { e.Proof.Checkpoint = strp("") }, r.log())
	tl("no promise", func(e *tleJSON) { e.Promise = nil }, r.log())
	tl("empty promise", func(e *tleJSON) { e.Promise = strp("") }, r.log())
	tl("no proof", func(e *tleJSON) { e.Proof = nil }, r.log())
	// SETs.
	tl("set over another time", func(e *tleJSON) { e.IntegratedTime++ }, r.log())
	tl("set over another index", func(e *tleJSON) { e.LogIndex++ }, r.log())
	tl("set garbage", func(e *tleJSON) { e.Promise = strp(b64([]byte("garbage"))) }, r.log())
	tl("zero integrated time", func(e *tleJSON) { e.IntegratedTime = 0 }, r.log())
	tl("unknown log", func(e *tleJSON) { e.LogID = strp(b64([]byte("other"))) }, r.log())
	tl("log not yet valid", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.Start = i64p(1_800_000_000); return l }())
	tl("log expired", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.End = i64p(1_700_000_000); return l }())
	tl("log valid to the second", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.End = i64p(e0(base)); return l }())
	tl("log valid from the second", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.Start = i64p(e0(base)); return l }())
	tl("log valid from the next second", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.Start = i64p(e0(base) + 1); return l }())
	tl("log without start", func(e *tleJSON) {}, func() *logJSON { l := r.log(); l.Start = nil; return l }())
	// Two signatures, the second not base64: the envelope verifier decodes each.
	dp("envelope undecodable second signature", func(s map[string]any) {
		var e map[string]any
		json.Unmarshal([]byte(sub(s, "proposedContent")["envelope"].(string)), &e)
		e["signatures"] = append(e["signatures"].([]any), map[string]any{"sig": "!"})
		sub(s, "proposedContent")["envelope"] = string(compact(e))
	})
	tl("ed25519 log", func(e *tleJSON) {}, func() *logJSON {
		l := r.log()
		l.Key = b64(must(x509.MarshalPKIXPublicKey(ed.priv.Public())))
		return l
	}())
	tl("rsa log", func(e *tleJSON) {}, func() *logJSON {
		l := r.log()
		l.Key = b64(must(x509.MarshalPKIXPublicKey(rsaS.priv.Public())))
		return l
	}())
	tl("p384 log", func(e *tleJSON) {}, func() *logJSON {
		l := r.log()
		l.Key = b64(must(x509.MarshalPKIXPublicKey(p384.priv.Public())))
		return l
	}())
	// Inclusion proofs.
	tl("proof index beyond", func(e *tleJSON) { e.Proof.LogIndex = e.Proof.TreeSize }, r.log())
	tl("proof negative index", func(e *tleJSON) { e.Proof.LogIndex = -1 }, r.log())
	tl("proof wrong index", func(e *tleJSON) { e.Proof.LogIndex++ }, r.log())
	tl("proof wrong size", func(e *tleJSON) { e.Proof.TreeSize++ }, r.log())
	tl("proof too short", func(e *tleJSON) { e.Proof.Hashes = e.Proof.Hashes[1:] }, r.log())
	tl("proof hash flipped", func(e *tleJSON) {
		h := must(base64.StdEncoding.DecodeString(e.Proof.Hashes[0]))
		h[0] ^= 1
		e.Proof.Hashes[0] = b64(h)
	}, r.log())
	tl("proof root wrong", func(e *tleJSON) { e.Proof.RootHash = b64(make([]byte, 32)) }, r.log())
	tl("proof root empty", func(e *tleJSON) { e.Proof.RootHash = "" }, r.log())
	tl("body changed after proof", func(e *tleJSON) { e.Body = strp(b64(append(good, ' '))) }, r.log())
	// Checkpoints.
	cpText := func(e *tleJSON) string {
		cp := *e.Proof.Checkpoint
		return cp[:strings.LastIndex(cp, "\n\n")+1]
	}
	resign := func(text string) string { return r.checkpoint(text, "rekor.sigstore.dev") }
	tl("checkpoint no blank line", func(e *tleJSON) { e.Proof.Checkpoint = strp(strings.Replace(*e.Proof.Checkpoint, "\n\n", "\n", 1)) }, r.log())
	tl("checkpoint no final newline", func(e *tleJSON) { cp := *e.Proof.Checkpoint; e.Proof.Checkpoint = strp(cp[:len(cp)-1]) }, r.log())
	tl("checkpoint signature line no dash", func(e *tleJSON) { e.Proof.Checkpoint = strp(strings.Replace(*e.Proof.Checkpoint, "— ", "- ", 1)) }, r.log())
	tl("checkpoint signature line no space", func(e *tleJSON) { e.Proof.Checkpoint = strp(strings.Replace(*e.Proof.Checkpoint, "— ", "—", 1)) }, r.log())
	tl("checkpoint signature extra field", func(e *tleJSON) { cp := *e.Proof.Checkpoint; e.Proof.Checkpoint = strp(cp[:len(cp)-1] + " extra\n") }, r.log())
	tl("checkpoint signature missing sig", func(e *tleJSON) { e.Proof.Checkpoint = strp(cpText(e) + "\n— rekor.sigstore.dev\n") }, r.log())
	tl("checkpoint signature bad base64", func(e *tleJSON) { e.Proof.Checkpoint = strp(cpText(e) + "\n— rekor.sigstore.dev ab!c\n") }, r.log())
	tl("checkpoint signature small", func(e *tleJSON) { e.Proof.Checkpoint = strp(cpText(e) + "\n— rekor.sigstore.dev AAAA\n") }, r.log())
	tl("checkpoint empty signature line", func(e *tleJSON) { e.Proof.Checkpoint = strp(cpText(e) + "\n\n") }, r.log())
	tl("checkpoint crlf signature", func(e *tleJSON) { cp := *e.Proof.Checkpoint; e.Proof.Checkpoint = strp(cp[:len(cp)-1] + "\r\n") }, r.log())
	tl("checkpoint tab separated", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(strings.Replace(*e.Proof.Checkpoint, "— rekor.sigstore.dev ", "—\trekor.sigstore.dev\t", 1))
	}, r.log())
	tl("checkpoint two signatures", func(e *tleJSON) {
		cp := *e.Proof.Checkpoint
		lines := cp[strings.LastIndex(cp, "\n\n")+2:]
		e.Proof.Checkpoint = strp(cp + lines)
	}, r.log())
	tl("checkpoint second key", func(e *tleJSON) {
		other := newRekorV1()
		cp := *e.Proof.Checkpoint
		e.Proof.Checkpoint = strp(cp + other.checkpoint(cpText(e), "other")[len(cpText(e))+1:])
	}, r.log())
	tl("checkpoint wrong key hash", func(e *tleJSON) {
		other := newRekorV1()
		e.Proof.Checkpoint = strp(other.checkpoint(cpText(e), "rekor.sigstore.dev"))
	}, r.log())
	tl("checkpoint bad signature", func(e *tleJSON) {
		cp := *e.Proof.Checkpoint
		i := strings.LastIndex(cp, " ")
		sig := must(base64.StdEncoding.DecodeString(strings.TrimSpace(cp[i+1:])))
		sig[len(sig)-2] ^= 1
		e.Proof.Checkpoint = strp(cp[:i+1] + b64(sig) + "\n")
	}, r.log())
	tl("checkpoint other root", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n%d\n%s\n", r.origin, e.Proof.TreeSize, b64(make([]byte, 32)))))
	}, r.log())
	tl("checkpoint three lines", func(e *tleJSON) { e.Proof.Checkpoint = strp(resign("only\n13\n")) }, r.log())
	tl("checkpoint empty origin", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("\n%d\n%s\n", e.Proof.TreeSize, e.Proof.RootHash)))
	}, r.log())
	tl("checkpoint bad size", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n-1\n%s\n", r.origin, e.Proof.RootHash)))
	}, r.log())
	tl("checkpoint huge size", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n99999999999999999999\n%s\n", r.origin, e.Proof.RootHash)))
	}, r.log())
	tl("checkpoint bad hash", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n%d\n%s\n", r.origin, e.Proof.TreeSize, "!!")))
	}, r.log())
	tl("checkpoint other content", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n%d\n%s\nTimestamp: 1\nmore\n", r.origin, e.Proof.TreeSize, e.Proof.RootHash)))
	}, r.log())
	tl("checkpoint origin without tree id", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n%d\n%s\n", "rekor.sigstore.dev", e.Proof.TreeSize, e.Proof.RootHash)))
	}, r.log())
	// Checkpoints signed by logs of other key types, each verified with its own key.
	signedBy := func(text, name string, priv crypto.Signer) string {
		var sig []byte
		h := sha256.Sum256([]byte(text))
		switch k := priv.(type) {
		case ed25519.PrivateKey:
			sig = ed25519.Sign(k, []byte(text))
		case *rsa.PrivateKey:
			sig = must(rsa.SignPKCS1v15(rand.Reader, k, crypto.SHA256, h[:]))
		case *ecdsa.PrivateKey:
			sig = must(ecdsa.SignASN1(rand.Reader, k, h[:]))
		}
		kh := sha256.Sum256(must(x509.MarshalPKIXPublicKey(priv.Public())))
		return text + "\n— " + name + " " + b64(append(kh[:4:4], sig...)) + "\n"
	}
	for _, s := range []signer{ed, rsaS, p384} {
		s := s
		tl("checkpoint by "+s.name+" log", func(e *tleJSON) {
			e.Proof.Checkpoint = strp(signedBy(cpText(e), "rekor.sigstore.dev", s.priv))
		}, func() *logJSON {
			l := r.log()
			l.Key = b64(must(x509.MarshalPKIXPublicKey(s.priv.Public())))
			return l
		}())
	}
	long := "— rekor.sigstore.dev " + strings.Repeat("A", 70000) + "\n"
	tl("checkpoint long line first", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(cpText(e) + "\n" + long + (*e.Proof.Checkpoint)[len(cpText(e))+1:])
	}, r.log())
	tl("checkpoint long line last", func(e *tleJSON) { e.Proof.Checkpoint = strp(*e.Proof.Checkpoint + long) }, r.log())
	tl("checkpoint line at the limit", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(*e.Proof.Checkpoint + "— a " + strings.Repeat("A", 65536-7) + "\n")
	}, r.log())
	tl("checkpoint line past the limit", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(*e.Proof.Checkpoint + "— a " + strings.Repeat("A", 65536-6) + "\n")
	}, r.log())
	tl("negative integrated time", func(e *tleJSON) { e.IntegratedTime = -5 }, r.log())
	hr("value number", func(s map[string]any) { sub(s, "data", "hash")["value"] = 12 })
	ds("signature number", func(s map[string]any) { firstOf(s, "signatures")["signature"] = 5 })
	it("payload type number", func(s map[string]any) { sub(s, "content", "envelope")["payloadType"] = 1 })
	tl("checkpoint origin tree id letters", func(e *tleJSON) {
		e.Proof.Checkpoint = strp(resign(fmt.Sprintf("%s\n%d\n%s\n", "rekor - 12a", e.Proof.TreeSize, e.Proof.RootHash)))
	}, r.log())

	// Rekor v2.
	for _, lk := range []string{"ed25519", "ecdsa"} {
		r2 := newRekorV2(lk)
		for _, c := range []struct {
			s       signer
			details string
			h       crypto.Hash
			cert    bool
		}{
			{p256, "PKIX_ECDSA_P256_SHA_256", crypto.SHA256, false},
			{p384, "PKIX_ECDSA_P384_SHA_384", crypto.SHA384, false},
			{rsaS, "PKIX_RSA_PKCS1V15_2048_SHA256", crypto.SHA256, false},
			{ed, "PKIX_ED25519_PH", crypto.SHA512, false},
			{ss[4], "PKIX_ECDSA_P256_SHA_256", crypto.SHA256, true},
		} {
			e, v := r2.entry(c.s, c.details, c.h, payload, c.cert)
			cs := add("v2 "+lk+" "+c.details+fmt.Sprint(c.cert), e, r2.log())
			cs.V2 = v
		}
		e, v := r2.entry(p256, "PKIX_ECDSA_P256_SHA_256", crypto.SHA256, payload, false)
		v2c := func(name string, f func(e *tleJSON, v *v2JSON)) {
			e2 := e
			p := *e.Proof
			p.Hashes = append([]string{}, e.Proof.Hashes...)
			e2.Proof = &p
			v2 := *v
			f(&e2, &v2)
			cs := add("v2 "+lk+": "+name, e2, r2.log())
			cs.V2 = &v2
		}
		v2c("other origin", func(e *tleJSON, v *v2JSON) { v.Origin = "other.dev" })
		v2c("invalid origin", func(e *tleJSON, v *v2JSON) { v.Origin = "a b" })
		v2c("empty origin", func(e *tleJSON, v *v2JSON) { v.Origin = "" })
		v2c("plus origin", func(e *tleJSON, v *v2JSON) { v.Origin = "a+b" })
		v2c("digest changed", func(e *tleJSON, v *v2JSON) { v.Digest = b64(make([]byte, 32)) })
		v2c("signature changed", func(e *tleJSON, v *v2JSON) { v.Signature = b64([]byte("x")) })
		v2c("details changed", func(e *tleJSON, v *v2JSON) { v.Details = "PKIX_ECDSA_P384_SHA_384" })
		v2c("ed25519 details", func(e *tleJSON, v *v2JSON) { v.Details = "PKIX_ED25519" })
		v2c("certificate verifier", func(e *tleJSON, v *v2JSON) { v.VerifierKind = "x509Certificate" })
		v2c("negative index", func(e *tleJSON, v *v2JSON) { e.LogIndex = -1 })
		v2c("wrong index", func(e *tleJSON, v *v2JSON) { e.LogIndex++ })
		v2c("index beyond", func(e *tleJSON, v *v2JSON) { e.LogIndex = 100 })
		v2c("hash flipped", func(e *tleJSON, v *v2JSON) {
			h := must(base64.StdEncoding.DecodeString(e.Proof.Hashes[0]))
			h[0] ^= 1
			e.Proof.Hashes[0] = b64(h)
		})
		v2c("proof short", func(e *tleJSON, v *v2JSON) { e.Proof.Hashes = e.Proof.Hashes[1:] })
		// A protobuf string is UTF-8 (protojson refuses anything else); U+FFFD itself
		// is a rune note.Open takes.
		v2c("note replacement rune", func(e *tleJSON, v *v2JSON) { e.Proof.Checkpoint = strp("�" + *e.Proof.Checkpoint) })
		v2c("note control char", func(e *tleJSON, v *v2JSON) { e.Proof.Checkpoint = strp("\t" + *e.Proof.Checkpoint) })
		v2c("note no split", func(e *tleJSON, v *v2JSON) {
			e.Proof.Checkpoint = strp(strings.Replace(*e.Proof.Checkpoint, "\n\n", "\n", 1))
		})
		v2c("note unknown signer only", func(e *tleJSON, v *v2JSON) {
			cp := *e.Proof.Checkpoint
			text := cp[:strings.LastIndex(cp, "\n\n")+1]
			other := newRekorV2(lk)
			other.origin = "other.dev"
			e.Proof.Checkpoint = strp(other.checkpoint(text))
		})
		v2c("note bad signature", func(e *tleJSON, v *v2JSON) {
			cp := *e.Proof.Checkpoint
			i := strings.LastIndex(cp, " ")
			sig := must(base64.StdEncoding.DecodeString(strings.TrimSpace(cp[i+1:])))
			sig[len(sig)-1] ^= 1
			e.Proof.Checkpoint = strp(cp[:i+1] + b64(sig) + "\n")
		})
		v2c("note bad line", func(e *tleJSON, v *v2JSON) { e.Proof.Checkpoint = strp(*e.Proof.Checkpoint + "garbage\n") })
		v2c("note duplicate signature", func(e *tleJSON, v *v2JSON) {
			cp := *e.Proof.Checkpoint
			e.Proof.Checkpoint = strp(cp + cp[strings.LastIndex(cp, "\n\n")+2:])
		})
		v2c("checkpoint other origin", func(e *tleJSON, v *v2JSON) {
			cp := *e.Proof.Checkpoint
			text := cp[:strings.LastIndex(cp, "\n\n")+1]
			e.Proof.Checkpoint = strp(r2.checkpoint("x" + text))
		})
		v2c("checkpoint bad size", func(e *tleJSON, v *v2JSON) {
			e.Proof.Checkpoint = strp(r2.checkpoint(r2.origin + "\nx\nAAAA\n"))
		})
		v2c("checkpoint two lines", func(e *tleJSON, v *v2JSON) {
			e.Proof.Checkpoint = strp(r2.checkpoint(r2.origin + "\n9\n"))
		})
	}
	// Rekor v2 bodies sigstore-go does not take as v2.
	v2body := func(name, b string) {
		add("v2 body: "+name, tleJSON{LogIndex: 1, LogID: strp(b64([]byte("x"))), KindVersion: []string{"hashedrekord", "0.0.2"}, Body: strp(b64([]byte(b)))}, nil)
	}
	v2body("dsse v002", `{"kind":"dsse","apiVersion":"0.0.2","spec":{"dsseV002":{}}}`)
	v2body("wrong api", `{"kind":"hashedrekord","apiVersion":"0.0.1","spec":{"hashedRekordV002":{}}}`)
	v2body("empty", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{}}}`)
	v2body("proto names", `{"api_version":"0.0.2","spec":{"hashed_rekord_v002":{"data":{"algorithm":1,"digest":"AQ"},"signature":{"content":"AQ","verifier":{"public_key":{"raw_bytes":"AQ"},"key_details":"PKIX_ED25519"}}}}}`)
	v2body("unknown field", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"x":1}}}`)
	v2body("no verifier", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"AQ"}}}}`)
	v2body("empty verifier", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"AQ","verifier":{}}}}}`)
	v2body("empty raw bytes", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"AQ","verifier":{"x509Certificate":{}}}}}}`)
	v2body("no data", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"signature":{"content":"AQ","verifier":{"publicKey":{"rawBytes":"AQ"}}}}}}`)
	v2body("empty content", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"","verifier":{"publicKey":{"rawBytes":"AQ"}}}}}}`)
	v2body("two verifiers", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"AQ","verifier":{"publicKey":{"rawBytes":"AQ"},"x509Certificate":{"rawBytes":"AQ"}}}}}}`)
	v2body("null oneof", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{},"signature":{"content":"AQ","verifier":{"publicKey":null,"x509Certificate":{"rawBytes":"AQ"}}}}}}`)
	v2body("enum number unknown", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"algorithm":99,"digest":"AQ=="},"signature":{"content":"AQ","verifier":{"publicKey":{"rawBytes":"AQ"},"keyDetails":1e1}}}}}`)
	v2body("enum fraction", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"algorithm":1.5}}}}`)
	v2body("enum name unknown", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"algorithm":"MD5"}}}}`)
	v2body("bytes url padded", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"digest":"-_8="},"signature":{"content":"AQ","verifier":{"publicKey":{"rawBytes":"AQ"}}}}}}`)
	v2body("bytes bad padding", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{"data":{"digest":"AQ="}}}}`)
	v2body("invalid utf8", "{\"apiVersion\":\"0.0.2\",\"kind\":\"\xff\",\"spec\":{\"hashedRekordV002\":{}}}")
	v2body("lone surrogate", `{"apiVersion":"0.0.2","kind":"\ud800","spec":{"hashedRekordV002":{}}}`)
	v2body("trailing comma", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{}},}`)
	v2body("trailing data", `{"apiVersion":"0.0.2","spec":{"hashedRekordV002":{}}} 1`)
	v2body("duplicate names", `{"apiVersion":"0.0.2","apiVersion":"0.0.2","spec":{"hashedRekordV002":{}}}`)
	v2body("string number", `{"apiVersion":0.2,"spec":{"hashedRekordV002":{}}}`)

	for _, c := range cases {
		run(c)
	}
	b, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// The integrated time of an entry, for a log valid until it.
func e0(e tleJSON) int64 { return e.IntegratedTime }
