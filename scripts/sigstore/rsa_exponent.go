//go:build ignore

// Regenerates crates/sigstore/testdata/rsa_exponent.json: RSA keys with the largest
// public exponent crypto/rsa takes (2^31-1) and one past it (2^31+1), each with its PKIX
// and PKCS #1 encodings and a PKCS #1 v1.5 and a PSS (salt of the hash's length)
// SHA-256 signature of "shards", and what Go makes of each: crypto/x509's parse of each
// encoding, and crypto/rsa's verification of each signature. crypto/rsa makes no key past
// the bound, so the keys are made and the signatures made here, by the textbook
// operations (RFC 8017).
//
//	go run scripts/sigstore/rsa_exponent.go
package main

import (
	"crypto"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"math/big"
	"os"
	"path/filepath"
	"runtime"
)

type key struct {
	E            int    `json:"e"`
	PKIX         string `json:"pkix"`
	PKCS1        string `json:"pkcs1"`
	Signature    string `json:"signature"`
	SignaturePSS string `json:"signaturePss"`
	ParsePKIX    string `json:"parsePkix"`
	ParsePKCS1   string `json:"parsePkcs1"`
	Verify       string `json:"verify"`
	VerifyPSS    string `json:"verifyPss"`
}

// mgf1 with SHA-256 (RFC 8017, B.2.1).
func mgf1(seed []byte, n int) []byte {
	var out []byte
	for c := 0; len(out) < n; c++ {
		h := sha256.New()
		h.Write(seed)
		h.Write([]byte{byte(c >> 24), byte(c >> 16), byte(c >> 8), byte(c)})
		out = h.Sum(out)
	}
	return out[:n]
}

// pss: EMSA-PSS-ENCODE of a SHA-256 digest for a 2048-bit modulus (RFC 8017, 9.1.1).
func pss(digest, salt []byte) []byte {
	const emLen, hLen = 256, 32
	h := sha256.New()
	h.Write(make([]byte, 8))
	h.Write(digest)
	h.Write(salt)
	H := h.Sum(nil)
	db := make([]byte, emLen-hLen-1)
	db[len(db)-len(salt)-1] = 1
	copy(db[len(db)-len(salt):], salt)
	for i, m := range mgf1(H, len(db)) {
		db[i] ^= m
	}
	db[0] &= 0x7f
	return append(append(db, H...), 0xbc)
}

func errText(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

// newKey: a 2048-bit modulus and the private exponent for e.
func newKey(e int) (*big.Int, *big.Int) {
	one := big.NewInt(1)
	for {
		p, err := rand.Prime(rand.Reader, 1024)
		must(err)
		q, err := rand.Prime(rand.Reader, 1024)
		must(err)
		n := new(big.Int).Mul(p, q)
		if p.Cmp(q) == 0 || n.BitLen() != 2048 {
			continue
		}
		phi := new(big.Int).Mul(new(big.Int).Sub(p, one), new(big.Int).Sub(q, one))
		if d := new(big.Int).ModInverse(big.NewInt(int64(e)), phi); d != nil {
			return n, d
		}
	}
}

func main() {
	message := []byte("shards")
	digest := sha256.Sum256(message)
	// EMSA-PKCS1-v1_5 of the digest (RFC 8017, 9.2), for a 256-octet modulus.
	prefix, _ := hex.DecodeString("3031300d060960864801650304020105000420")
	t := append(prefix, digest[:]...)
	em := make([]byte, 256)
	em[1] = 1
	for i := 2; i < len(em)-len(t)-1; i++ {
		em[i] = 0xff
	}
	copy(em[len(em)-len(t):], t)

	var keys []key
	for _, e := range []int{1<<31 - 1, 1<<31 + 1} {
		n, d := newKey(e)
		sig := new(big.Int).Exp(new(big.Int).SetBytes(em), d, n).FillBytes(make([]byte, 256))
		salt := make([]byte, 32)
		_, err := rand.Read(salt)
		must(err)
		sigPSS := new(big.Int).Exp(new(big.Int).SetBytes(pss(digest[:], salt)), d, n).FillBytes(make([]byte, 256))
		pub := &rsa.PublicKey{N: n, E: e}
		pkix, err := x509.MarshalPKIXPublicKey(pub)
		must(err)
		pkcs1 := x509.MarshalPKCS1PublicKey(pub)
		_, perr := x509.ParsePKIXPublicKey(pkix)
		_, p1err := x509.ParsePKCS1PublicKey(pkcs1)
		keys = append(keys, key{
			E:            e,
			PKIX:         hex.EncodeToString(pkix),
			PKCS1:        hex.EncodeToString(pkcs1),
			Signature:    hex.EncodeToString(sig),
			SignaturePSS: hex.EncodeToString(sigPSS),
			ParsePKIX:    errText(perr),
			ParsePKCS1:   errText(p1err),
			Verify:       errText(rsa.VerifyPKCS1v15(pub, crypto.SHA256, digest[:], sig)),
			VerifyPSS:    errText(rsa.VerifyPSS(pub, crypto.SHA256, digest[:], sigPSS, nil)),
		})
	}
	out, err := json.MarshalIndent(map[string]any{"message": string(message), "keys": keys}, "", " ")
	must(err)
	_, file, _, _ := runtime.Caller(0)
	root := filepath.Join(filepath.Dir(file), "..", "..")
	must(os.WriteFile(filepath.Join(root, "crates/sigstore/testdata/rsa_exponent.json"), append(out, '\n'), 0o644))
}
