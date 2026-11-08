package zzshardskeyless

// What cosign v3.1.3 sends and makes when it signs keylessly (sigstore-go v1.2.2's
// sign package, rekor v1.5.3, rekor-tiles v2.3.0, sigstore v1.10.8's oauthflow), for
// crates/sigstore/tests/keyless_oracle.rs: sigstore-go's own Fulcio, Rekor v1, Rekor v2
// and timestamp authority clients against test servers that keep each request and answer
// with fixed bytes; the bundle sign.Bundle assembles from those answers; the services
// root.SelectServices selects; the subjects oauthflow reads from tokens. `generate`
// copies this file into cosign's source and runs it there.
//
// Randomness is fixed (testing/cryptotest.SetGlobalRandom), so keys and signatures repeat
// run to run; the timestamp authority's token carries the time it is made, which is all
// that changes.

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/asn1"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"testing/cryptotest"
	"time"

	"github.com/digitorus/timestamp"
	"github.com/go-openapi/runtime"
	protocommon "github.com/sigstore/protobuf-specs/gen/pb-go/common/v1"
	prototrustroot "github.com/sigstore/protobuf-specs/gen/pb-go/trustroot/v1"
	"github.com/sigstore/rekor/pkg/generated/models"
	rekortypes "github.com/sigstore/rekor/pkg/types"
	_ "github.com/sigstore/rekor/pkg/types/dsse/v0.0.1"
	"github.com/sigstore/sigstore-go/pkg/root"
	"github.com/sigstore/sigstore-go/pkg/sign"
	"github.com/sigstore/sigstore/pkg/oauthflow"
	"google.golang.org/protobuf/encoding/protojson"
)

type exchange struct {
	Request string `json:"request"`
	Answer  string `json:"answer,omitempty"`
	ETag    string `json:"etag,omitempty"`
}

type selection struct {
	Services []map[string]any `json:"services"`
	Selector string           `json:"selector"`
	Count    uint32           `json:"count"`
	Versions []uint32         `json:"versions"`
	Now      string           `json:"now"`
	Selected []string         `json:"selected,omitempty"`
	Error    string           `json:"error,omitempty"`
}

type subject struct {
	Claims  string `json:"claims"`
	Subject string `json:"subject,omitempty"`
	Error   string `json:"error,omitempty"`
}

type oracle struct {
	Token       string      `json:"token"`
	KeyPEM      string      `json:"keyPem"`
	Payload     string      `json:"payload"`
	Signature   string      `json:"signature"`
	Proof       string      `json:"proof"`
	CertDER     string      `json:"certDer"`
	Fulcio      exchange    `json:"fulcio"`
	TSA         exchange    `json:"tsa"`
	RekorV1     exchange    `json:"rekorV1"`
	RekorV2     exchange    `json:"rekorV2"`
	EntryV1     string      `json:"entryV1"`
	EntryV2     string      `json:"entryV2"`
	BundleV1    string      `json:"bundleV1"`
	BundleV2    string      `json:"bundleV2"`
	Selections  []selection `json:"selections"`
	Subjects    []subject   `json:"subjects"`
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

func b64url(b []byte) string { return base64.RawURLEncoding.EncodeToString(b) }

// A keypair whose proof of possession is kept: the signature over the token's subject.
type keeping struct {
	*sign.EphemeralKeypair
	proof []byte
	calls int
}

func (k *keeping) SignData(ctx context.Context, data []byte) ([]byte, []byte, error) {
	sig, digest, err := k.EphemeralKeypair.SignData(ctx, data)
	k.calls++
	// sign.Bundle signs the content first, then Fulcio's client the subject.
	if k.calls == 2 {
		k.proof = sig
	}
	return sig, digest, err
}

// A certificate for `pub`, issued by `parent` (itself where nil) with `key`.
func certify(t *testing.T, pub any, parent *x509.Certificate, key *ecdsa.PrivateKey, cn string, ca bool, eku []x509.ExtKeyUsage, email string) *x509.Certificate {
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(7),
		Subject:               pkix.Name{CommonName: cn},
		NotBefore:             time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC),
		NotAfter:              time.Date(2036, 1, 1, 0, 0, 0, 0, time.UTC),
		KeyUsage:              x509.KeyUsageDigitalSignature,
		ExtKeyUsage:           eku,
		BasicConstraintsValid: true,
		IsCA:                  ca,
	}
	if ca {
		tmpl.KeyUsage |= x509.KeyUsageCertSign
	}
	if email != "" {
		tmpl.EmailAddresses = []string{email}
	}
	if parent == nil {
		parent = tmpl
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, parent, pub, key)
	if err != nil {
		t.Fatal(err)
	}
	c, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestShardsKeylessOracle(t *testing.T) {
	out := os.Getenv("SHARDS_KEYLESS_ORACLE")
	if out == "" {
		t.Skip("SHARDS_KEYLESS_ORACLE names the output")
	}
	cryptotest.SetGlobalRandom(t, 1)
	o := oracle{}

	// A token of the shape Fulcio's issuers give, its subject an email.
	claims := `{"iss":"http://oracle.issuer","sub":"s","aud":"sigstore","exp":2000000000,"email":"signer@example.com","email_verified":true}`
	o.Token = b64url([]byte(`{"alg":"RS256","kid":"k"}`)) + "." + b64url([]byte(claims)) + "." + b64url([]byte("sig"))

	kp, err := sign.NewEphemeralKeypair(nil)
	if err != nil {
		t.Fatal(err)
	}
	keypair := &keeping{EphemeralKeypair: kp}
	pemKey, err := keypair.GetPublicKeyPem()
	if err != nil {
		t.Fatal(err)
	}
	o.KeyPEM = pemKey

	// Fulcio: a certificate for the key it is sent, from a CA of the test's (sign.Bundle
	// checks no chain without a trusted root; Rekor's client checks the envelope by it).
	caKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	ca := certify(t, caKey.Public(), nil, caKey, "oracle-ca", true, nil, "")
	fulcio := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		// The first bundle's request, whose proof is kept.
		if o.Fulcio.Request == "" {
			o.Fulcio.Request = string(body)
		}
		var req struct {
			PublicKeyRequest struct {
				PublicKey struct {
					Content string `json:"content"`
				} `json:"publicKey"`
			} `json:"publicKeyRequest"`
		}
		if err := json.Unmarshal(body, &req); err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		blk, _ := pem.Decode([]byte(req.PublicKeyRequest.PublicKey.Content))
		if blk == nil {
			http.Error(w, "no PEM", 400)
			return
		}
		pub, err := x509.ParsePKIXPublicKey(blk.Bytes)
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		leaf := certify(t, pub, ca, caKey, "", false, []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning}, "signer@example.com")
		if o.CertDER == "" {
			o.CertDER = b64(leaf.Raw)
		}
		chain := []string{
			string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: leaf.Raw})),
			string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: ca.Raw})),
		}
		answer, _ := json.Marshal(map[string]any{"signedCertificateEmbeddedSct": map[string]any{
			"chain": map[string]any{"certificates": chain}}})
		if o.Fulcio.Answer == "" {
			o.Fulcio.Answer = string(answer)
		}
		w.Write(answer)
	}))
	defer fulcio.Close()

	// A timestamp authority answering with a real token of its own key.
	tsaKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	tsaCert := certify(t, tsaKey.Public(), ca, caKey, "oracle-tsa", false, []x509.ExtKeyUsage{x509.ExtKeyUsageTimeStamping}, "")
	tsa := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		if o.TSA.Request == "" {
			o.TSA.Request = b64(body)
		}
		req, err := timestamp.ParseRequest(body)
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		ts := timestamp.Timestamp{
			HashAlgorithm: req.HashAlgorithm,
			HashedMessage: req.HashedMessage,
			Time:          time.Date(2026, 10, 11, 12, 0, 0, 0, time.UTC),
			Policy:        asn1.ObjectIdentifier{1, 3, 6, 1, 4, 1, 57264, 2},
			SerialNumber:  big.NewInt(9),
			Certificates:  []*x509.Certificate{ca},
		}
		resp, err := ts.CreateResponseWithOpts(tsaCert, tsaKey, crypto.SHA256)
		if err != nil {
			t.Errorf("timestamp: %v", err)
			http.Error(w, err.Error(), 500)
			return
		}
		w.Header().Set("Content-Type", "application/timestamp-reply")
		w.Write(resp)
	}))
	defer tsa.Close()

	// Rekor v1: the proposed entry kept; the answer an entry of its canonical body.
	rekor := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		o.RekorV1.Request = string(body)
		pe, err := models.UnmarshalProposedEntry(strings.NewReader(string(body)), runtime.JSONConsumer())
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		ei, err := rekortypes.UnmarshalEntry(pe)
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		canonical, err := ei.Canonicalize(context.Background())
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		uuid := strings.Repeat("ab", 32)
		answer, _ := json.Marshal(map[string]any{uuid: map[string]any{
			"body":           b64(canonical),
			"integratedTime": 1760184000,
			"logID":          strings.Repeat("c0", 32),
			"logIndex":       1234,
			"verification": map[string]any{
				"inclusionProof": map[string]any{
					"checkpoint": "rekor.oracle - 1\n1000\nq83vAQ==\n\n— rekor.oracle AAAA\n",
					"hashes":     []string{strings.Repeat("01", 32), strings.Repeat("02", 32)},
					"logIndex":   999,
					"rootHash":   strings.Repeat("03", 32),
					"treeSize":   1000,
				},
				"signedEntryTimestamp": b64([]byte("set")),
			},
		}})
		o.RekorV1.Answer = string(answer)
		o.RekorV1.ETag = uuid
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("ETag", uuid)
		w.Header().Set("Location", "/api/v1/log/entries/"+uuid)
		w.WriteHeader(201)
		w.Write(answer)
	}))
	defer rekor.Close()

	// Rekor v2: the request kept; the answer a TransparencyLogEntry of fixed fields.
	tiles := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		o.RekorV2.Request = string(body)
		answer := fmt.Sprintf(`{"logIndex":"77","logId":{"keyId":"%s"},"kindVersion":{"kind":"hashedrekord","version":"0.0.2"},"inclusionProof":{"logIndex":"77","rootHash":"%s","treeSize":"78","hashes":["%s"],"checkpoint":{"envelope":"rekor.tiles - 2\n78\nq83vAQ==\n"}},"canonicalizedBody":"%s"}`,
			b64([]byte("log")), b64([]byte("root")), b64([]byte("hash")), b64([]byte(`{"kind":"hashedrekord"}`)))
		o.RekorV2.Answer = answer
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(201)
		w.Write([]byte(answer))
	}))
	defer tiles.Close()

	payload := []byte(`{"_type":"https://in-toto.io/Statement/v1","subject":[{"digest":{"sha256":"` + strings.Repeat("ab", 32) + `"},"annotations":{}}],"predicateType":"https://sigstore.dev/cosign/sign/v1","predicate":{}}`)
	o.Payload = b64(payload)
	content := &sign.DSSEData{Data: payload, PayloadType: "application/vnd.in-toto+json"}
	opts := sign.BundleOptions{
		CertificateProvider:        sign.NewFulcio(&sign.FulcioOptions{BaseURL: fulcio.URL}),
		CertificateProviderOptions: &sign.CertificateProviderOptions{IDToken: o.Token},
		TimestampAuthorities:       []*sign.TimestampAuthority{sign.NewTimestampAuthority(&sign.TimestampAuthorityOptions{URL: tsa.URL})},
		TransparencyLogs:           []sign.Transparency{sign.NewRekor(&sign.RekorOptions{BaseURL: rekor.URL, Version: 1})},
	}
	b, err := sign.Bundle(content, keypair, opts)
	if err != nil {
		t.Fatal(err)
	}
	o.Proof = b64(keypair.proof)
	o.Signature = b64(b.GetDsseEnvelope().Signatures[0].Sig)
	j, _ := protojson.Marshal(b)
	o.BundleV1 = string(j)
	e, _ := protojson.Marshal(b.VerificationMaterial.TlogEntries[0])
	o.EntryV1 = string(e)

	// The same content to Rekor v2, from a keypair whose signatures are new.
	keypair2 := &keeping{EphemeralKeypair: kp}
	opts.TransparencyLogs = []sign.Transparency{sign.NewRekor(&sign.RekorOptions{BaseURL: tiles.URL, Version: 2})}
	b2, err := sign.Bundle(content, keypair2, opts)
	if err != nil {
		t.Fatal(err)
	}
	j2, _ := protojson.Marshal(b2)
	o.BundleV2 = string(j2)
	e2, _ := protojson.Marshal(b2.VerificationMaterial.TlogEntries[0])
	o.EntryV2 = string(e2)

	// Selections.
	at := func(s string) time.Time { tm, _ := time.Parse(time.RFC3339, s); return tm }
	type svc struct {
		url, op string
		v       uint32
		start   string
		end     string
	}
	mk := func(list []svc) ([]root.Service, []map[string]any) {
		var out []root.Service
		var js []map[string]any
		for _, s := range list {
			rs := root.Service{URL: s.url, MajorAPIVersion: s.v, Operator: s.op, ValidityPeriodStart: at(s.start)}
			m := map[string]any{"url": s.url, "majorApiVersion": s.v, "operator": s.op, "start": s.start}
			if s.end != "" {
				rs.ValidityPeriodEnd = at(s.end)
				m["end"] = s.end
			}
			out = append(out, rs)
			js = append(js, m)
		}
		return out, js
	}
	lists := [][]svc{
		{{"https://a1", "a", 1, "2024-01-01T00:00:00Z", ""}, {"https://a2", "a", 1, "2025-01-01T00:00:00Z", ""},
			{"https://b", "b", 1, "2023-01-01T00:00:00Z", ""}, {"https://none1", "", 1, "2022-01-01T00:00:00Z", ""},
			{"https://none2", "", 1, "2021-01-01T00:00:00Z", ""}},
		{{"https://v1", "a", 1, "2024-01-01T00:00:00Z", ""}, {"https://v2", "a", 2, "2025-01-01T00:00:00Z", ""},
			{"https://v2b", "b", 2, "2025-06-01T00:00:00Z", "2026-01-01T00:00:00Z"}},
		{{"https://later", "a", 1, "2030-01-01T00:00:00Z", ""}},
	}
	for _, list := range lists {
		services, js := mk(list)
		for _, versions := range [][]uint32{{1}, {1, 2}, {2}} {
			for _, now := range []string{"2026-10-11T00:00:00Z", "2025-07-01T00:00:00Z"} {
				s := selection{Services: js, Selector: "ALL", Versions: versions, Now: now}
				got, err := root.SelectServices(services, root.ServiceConfiguration{Selector: prototrustroot.ServiceSelector_ALL}, versions, at(now))
				if err != nil {
					// The time printed with the error is the caller's; the words are kept up to it.
					s.Error = strings.SplitN(err.Error(), " and current time", 2)[0]
				}
				for _, g := range got {
					s.Selected = append(s.Selected, g.URL)
				}
				o.Selections = append(o.Selections, s)
			}
		}
	}

	// Subjects.
	for _, c := range []string{
		`{"email":"a@b.c","email_verified":true,"sub":"x"}`,
		`{"email":"a@b.c","email_verified":"True"}`,
		`{"EMAIL":"a@b.c","Email_Verified":"true"}`,
		`{"sub":"repo:o/r:ref:refs/heads/dev"}`,
		`{"email":"a@b.c","email_verified":false,"sub":"x"}`,
		`{"email":"a@b.c"}`,
		`{"email_verified":"yes"}`,
		`{"email_verified":null,"sub":"x"}`,
		`{"aud":"sigstore"}`,
		`null`,
		`{"sub":5}`,
		`[1]`,
		`{"sub":"a","sub":"b"}`,
	} {
		s := subject{Claims: c}
		got, err := oauthflow.SubjectFromUnverifiedToken([]byte(c))
		if err != nil {
			s.Error = err.Error()
		} else {
			s.Subject = got
		}
		o.Subjects = append(o.Subjects, s)
	}
	_ = protocommon.HashAlgorithm_SHA2_256

	data, err := json.MarshalIndent(o, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
