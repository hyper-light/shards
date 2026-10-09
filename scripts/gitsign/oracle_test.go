package main

// go-crypto's answers (as buildx v0.37.1 vendors it) to the armored signatures of
// crates/gitsign/testdata/corpus.json, for crates/gitsign/tests/oracle.rs: what BuildKit's
// pgpsign.ParseArmoredDetachedSignature makes of each, and what buildx's git signature
// summary (gitsign.ParseSignature) says of it. `generate` copies this file into buildx's
// cmd/buildx and runs it there.

import (
	"bytes"
	"crypto"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/ProtonMail/go-crypto/openpgp"
	"github.com/ProtonMail/go-crypto/openpgp/armor"
	"github.com/ProtonMail/go-crypto/openpgp/packet"
	"github.com/moby/buildkit/util/gitutil/gitobject"
	"github.com/moby/buildkit/util/gitutil/gitsign"
	"github.com/moby/buildkit/util/pgpsign"
	digest "github.com/opencontainers/go-digest"
	"golang.org/x/crypto/ssh"
)

type pgpAnswer struct {
	Name      string `json:"name"`
	Error     string `json:"error,omitempty"`
	Version   int    `json:"version,omitempty"`
	SigType   int    `json:"sigType,omitempty"`
	Algo      int    `json:"algo,omitempty"`
	Hash      int    `json:"hash,omitempty"`
	Created   int64  `json:"created,omitempty"`
	KeyID     string `json:"keyID,omitempty"`
	Finger    string `json:"fingerprint,omitempty"`
	Suffix    string `json:"hashSuffix,omitempty"`
	Tag       string `json:"hashTag,omitempty"`
	Summary   string `json:"summary"`
}

func TestShardsPGPOracle(t *testing.T) {
	in, out := os.Getenv("SHARDS_PGP_CORPUS"), os.Getenv("SHARDS_PGP_OUT")
	if out == "" {
		t.Skip("SHARDS_PGP_OUT names the file to write")
	}
	var cases []struct {
		Name    string `json:"name"`
		Armored string `json:"armored"`
	}
	data, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, &cases); err != nil {
		t.Fatal(err)
	}
	ids := map[uint]int{3: 2, 4: 11, 5: 8, 6: 9, 7: 10, 11: 12, 13: 14}
	var answers []pgpAnswer
	for _, c := range cases {
		a := pgpAnswer{Name: c.Name}
		sig, _, err := pgpsign.ParseArmoredDetachedSignature([]byte(c.Armored))
		if err != nil {
			a.Error = err.Error()
		} else {
			a.Version = sig.Version
			a.SigType = int(sig.SigType)
			a.Algo = int(sig.PubKeyAlgo)
			a.Hash = ids[uint(sig.Hash)]
			a.Created = sig.CreationTime.Unix()
			if sig.IssuerKeyId != nil {
				a.KeyID = fmt.Sprintf("%016x", *sig.IssuerKeyId)
			}
			a.Finger = hex.EncodeToString(sig.IssuerFingerprint)
			a.Suffix = hex.EncodeToString(sig.HashSuffix)
			a.Tag = hex.EncodeToString(sig.HashTag[:])
		}
		// buildx's summary (policy/git.go parseGitSignature).
		s, err := gitsign.ParseSignature([]byte(c.Armored))
		switch {
		case err != nil:
			a.Summary = "none"
		case s.PGPSignature != nil && s.PGPSignature.IssuerKeyId != nil:
			a.Summary = fmt.Sprintf("pgp v%d %016x", s.PGPSignature.Version, *s.PGPSignature.IssuerKeyId)
		case s.PGPSignature != nil:
			a.Summary = fmt.Sprintf("pgp v%d", s.PGPSignature.Version)
		case s.SSHSignature != nil:
			a.Summary = fmt.Sprintf("ssh v%d %s", s.SSHSignature.Version, ssh.FingerprintSHA256(s.SSHSignature.PublicKey))
		default:
			a.Summary = "none"
		}
		answers = append(answers, a)
	}
	_ = packet.SigTypeBinary
	b, err := json.MarshalIndent(answers, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// The verification cases of crates/gitsign/testdata/verify.json (scripts/gitsign/verify.py
// makes them with gpg and ssh-keygen), with keys gpg does not make added here, and what
// buildx's builtins make of each: gitsign.VerifySignature (verify_git_signature),
// pgpsign.VerifySignatureWithDigest over the digest buildx asks for
// (verify_http_pgp_signature), and the key rings ReadAllArmoredKeyRings reads.

type verifyCase struct {
	Name      string      `json:"name"`
	Kind      string      `json:"kind"`
	Signature string      `json:"signature"`
	Data      string      `json:"data"`
	Keys      string      `json:"keys"`
	Now       int64       `json:"now"`
	Error     string      `json:"error"`
	Digest    string      `json:"digest,omitempty"`
	Ring      *ringAnswer `json:"ring"`
}

type ringAnswer struct {
	Error    string         `json:"error,omitempty"`
	Entities []entityAnswer `json:"entities,omitempty"`
}

type entityAnswer struct {
	Primary     string   `json:"primary"`
	Version     int      `json:"version"`
	Algo        int      `json:"algo"`
	Fingerprint string   `json:"fingerprint"`
	Identities  []string `json:"identities"`
	Subkeys     []string `json:"subkeys"`
	Revocations int      `json:"revocations"`
}

func ringOf(keys string) *ringAnswer {
	el, err := pgpsign.ReadAllArmoredKeyRings([]byte(keys))
	if err != nil {
		return &ringAnswer{Error: err.Error()}
	}
	r := &ringAnswer{}
	for _, e := range el {
		a := entityAnswer{
			Primary:     fmt.Sprintf("%016x", e.PrimaryKey.KeyId),
			Version:     e.PrimaryKey.Version,
			Algo:        int(e.PrimaryKey.PubKeyAlgo),
			Fingerprint: hex.EncodeToString(e.PrimaryKey.Fingerprint),
			Identities:  []string{},
			Subkeys:     []string{},
			Revocations: len(e.Revocations),
		}
		for name := range e.Identities {
			a.Identities = append(a.Identities, name)
		}
		sort.Strings(a.Identities)
		for _, s := range e.Subkeys {
			a.Subkeys = append(a.Subkeys, fmt.Sprintf("%016x", s.PublicKey.KeyId))
		}
		r.Entities = append(r.Entities, a)
	}
	return r
}

func armored(t *testing.T, kind string, write func(io.Writer) error) string {
	var b bytes.Buffer
	w, err := armor.Encode(&b, kind, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := write(w); err != nil {
		t.Fatal(err)
	}
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	return b.String() + "\n"
}

// signAt signs data as e's primary key at `at`, whatever the key's lifetime.
func signAt(t *testing.T, e *openpgp.Entity, data []byte, at time.Time) string {
	sig := &packet.Signature{
		Version:      e.PrimaryKey.Version,
		SigType:      packet.SigTypeBinary,
		PubKeyAlgo:   e.PrimaryKey.PubKeyAlgo,
		Hash:         crypto.SHA256,
		CreationTime: at,
		IssuerKeyId:  &e.PrimaryKey.KeyId,
	}
	cfg := &packet.Config{Time: func() time.Time { return at }}
	h, err := sig.PrepareSign(cfg)
	if err != nil {
		t.Fatal(err)
	}
	h.Write(data)
	if err := sig.Sign(h, e.PrivateKey, cfg); err != nil {
		t.Fatal(err)
	}
	return armored(t, "PGP SIGNATURE", sig.Serialize)
}

// Keys gpg does not make: v6 keys, native Ed25519 and Ed448, SHA3 self-signatures, and a
// key that had expired when it signed.
func goCases(t *testing.T, data []byte) []verifyCase {
	var out []verifyCase
	add := func(name string, cfg *packet.Config, signCfg *packet.Config) {
		e, err := openpgp.NewEntity("shards-"+strings.ReplaceAll(name, " ", "-"), "", "go@shards.invalid", cfg)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		keys := armored(t, openpgp.PublicKeyType, e.Serialize)
		var sig bytes.Buffer
		if err := openpgp.ArmoredDetachSign(&sig, e, bytes.NewReader(data), signCfg); err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		for _, kind := range []string{"git", "digest"} {
			n := "go " + name
			if kind == "digest" {
				n += " digest"
			}
			out = append(out, verifyCase{Name: n, Kind: kind, Signature: sig.String() + "\n", Data: base64.StdEncoding.EncodeToString(data), Keys: keys})
		}
		secret := armored(t, openpgp.PrivateKeyType, func(w io.Writer) error { return e.SerializePrivate(w, nil) })
		out = append(out, verifyCase{Name: "go " + name + " private block", Kind: "git", Signature: sig.String() + "\n", Data: base64.StdEncoding.EncodeToString(data), Keys: secret})
	}
	sha256 := &packet.Config{DefaultHash: crypto.SHA256}
	add("v6 ed25519", &packet.Config{V6Keys: true, Algorithm: packet.PubKeyAlgoEd25519}, sha256)
	add("v6 ed448", &packet.Config{V6Keys: true, Algorithm: packet.PubKeyAlgoEd448}, &packet.Config{DefaultHash: crypto.SHA512})
	add("v4 ed25519 native", &packet.Config{Algorithm: packet.PubKeyAlgoEd25519}, sha256)
	add("v4 ed448", &packet.Config{Algorithm: packet.PubKeyAlgoEd448}, &packet.Config{DefaultHash: crypto.SHA512})
	add("v6 rsa", &packet.Config{V6Keys: true, Algorithm: packet.PubKeyAlgoRSA, RSABits: 3072}, sha256)
	add("v6 ecdsa p256", &packet.Config{V6Keys: true, Algorithm: packet.PubKeyAlgoECDSA, Curve: packet.CurveNistP256}, sha256)
	add("v6 ecdsa p384 sha384", &packet.Config{V6Keys: true, Algorithm: packet.PubKeyAlgoECDSA, Curve: packet.CurveNistP384}, &packet.Config{DefaultHash: crypto.SHA384})
	add("v4 rsa sha3 self-signatures", &packet.Config{Algorithm: packet.PubKeyAlgoRSA, RSABits: 2048, DefaultHash: crypto.SHA3_256}, sha256)
	add("v4 brainpool p256", &packet.Config{Algorithm: packet.PubKeyAlgoECDSA, Curve: packet.CurveBrainpoolP256}, sha256)
	add("v4 secp256k1", &packet.Config{Algorithm: packet.PubKeyAlgoECDSA, Curve: packet.CurveSecP256k1}, sha256)

	// A signing subkey bound without its cross-signature, and with another key's.
	crossed := func(name string, embedded func(*openpgp.Entity) *packet.Signature) {
		e, err := openpgp.NewEntity("shards-cross", "", "cross@shards.invalid", &packet.Config{Algorithm: packet.PubKeyAlgoEdDSA})
		if err != nil {
			t.Fatal(err)
		}
		if err := e.AddSigningSubkey(nil); err != nil {
			t.Fatal(err)
		}
		sub := &e.Subkeys[len(e.Subkeys)-1]
		sub.Sig.EmbeddedSignature = embedded(e)
		if err := sub.Sig.SignKey(sub.PublicKey, e.PrivateKey, nil); err != nil {
			t.Fatal(err)
		}
		keys := armored(t, openpgp.PublicKeyType, e.Serialize)
		var sig bytes.Buffer
		if err := openpgp.ArmoredDetachSign(&sig, e, bytes.NewReader(data), sha256); err != nil {
			t.Fatal(err)
		}
		out = append(out, verifyCase{Name: name, Kind: "git", Signature: sig.String() + "\n", Data: base64.StdEncoding.EncodeToString(data), Keys: keys})
	}
	crossed("go signing subkey without its cross-signature", func(*openpgp.Entity) *packet.Signature { return nil })
	crossed("go signing subkey with another key's cross-signature", func(*openpgp.Entity) *packet.Signature {
		o, err := openpgp.NewEntity("shards-other", "", "other@shards.invalid", &packet.Config{Algorithm: packet.PubKeyAlgoEdDSA})
		if err != nil {
			t.Fatal(err)
		}
		if err := o.AddSigningSubkey(nil); err != nil {
			t.Fatal(err)
		}
		return o.Subkeys[len(o.Subkeys)-1].Sig.EmbeddedSignature
	})

	created := time.Unix(1600000000, 0)
	e, err := openpgp.NewEntity("shards-lifetime", "", "lifetime@shards.invalid", &packet.Config{
		Algorithm: packet.PubKeyAlgoEdDSA, KeyLifetimeSecs: 3600, Time: func() time.Time { return created },
	})
	if err != nil {
		t.Fatal(err)
	}
	keys := armored(t, openpgp.PublicKeyType, e.Serialize)
	for _, c := range []struct {
		name string
		at   time.Time
	}{{"go key expired when it signed", created.Add(2 * time.Hour)}, {"go key valid when it signed", created.Add(time.Minute)}, {"go signature before its key", created.Add(-time.Hour)}, {"go signature in the future", time.Now().Add(time.Hour)}} {
		out = append(out, verifyCase{Name: c.name, Kind: "git", Signature: signAt(t, e, data, c.at), Data: base64.StdEncoding.EncodeToString(data), Keys: keys})
	}
	return out
}

func TestShardsVerifyOracle(t *testing.T) {
	in, out := os.Getenv("SHARDS_VERIFY_CASES"), os.Getenv("SHARDS_VERIFY_OUT")
	if out == "" {
		t.Skip("SHARDS_VERIFY_OUT names the file to write")
	}
	var cases []verifyCase
	raw, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(raw, &cases); err != nil {
		t.Fatal(err)
	}
	data, err := base64.StdEncoding.DecodeString(cases[0].Data)
	if err != nil {
		t.Fatal(err)
	}
	cases = append(cases, goCases(t, data)...)
	for i := range cases {
		c := &cases[i]
		signed, err := base64.StdEncoding.DecodeString(c.Data)
		if err != nil {
			t.Fatal(err)
		}
		c.Now = time.Now().Unix()
		if strings.Contains(c.Keys, "-----BEGIN PGP") || c.Keys == "" {
			c.Ring = ringOf(c.Keys)
		}
		switch c.Kind {
		case "git":
			obj := &gitobject.GitObject{Signature: c.Signature, SignedData: string(signed)}
			if err := gitsign.VerifySignature(obj, []byte(c.Keys), nil); err != nil {
				c.Error = err.Error()
			}
		case "digest":
			sig, _, err := pgpsign.ParseArmoredDetachedSignature([]byte(c.Signature))
			if err != nil {
				c.Error = err.Error()
				break
			}
			ring, err := pgpsign.ReadAllArmoredKeyRings([]byte(c.Keys))
			if err != nil {
				c.Error = err.Error()
				break
			}
			algo, hf := digest.SHA256, crypto.SHA256
			switch sig.Hash {
			case crypto.SHA384:
				algo, hf = digest.SHA384, crypto.SHA384
			case crypto.SHA512:
				algo, hf = digest.SHA512, crypto.SHA512
			}
			h := hf.New()
			h.Write(signed)
			h.Write(sig.HashSuffix)
			d := digest.NewDigestFromBytes(algo, h.Sum(nil))
			c.Digest = string(d)
			if err := pgpsign.VerifySignatureWithDigest(sig, ring, d); err != nil {
				c.Error = err.Error()
			}
		default:
			t.Fatalf("%s: kind %q", c.Name, c.Kind)
		}
	}
	b, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
