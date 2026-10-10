package main

// RSA public keys of every size of public exponent, answered by what buildx v0.37.1 reads
// and verifies them with, for crates/gitsign/tests/rsa.rs:
//
//   - `verify`: Go's crypto/rsa (VerifyPKCS1v15, VerifyPSS with the salt's length found
//     and of the hash's) over keys made here, each signature made with the key's own
//     private exponent, so that a key Go refuses (exponent above 2^31-1, even, below 2,
//     modulus even or under 1024 bits) has signatures that would verify were it taken.
//     `keyError`: VerifyPKCS1v15's error where the key, not the signature, is refused.
//   - `ssh`: x/crypto/ssh v0.55.0's ParsePublicKey of `ssh-rsa` keys of those exponents,
//     and for those it takes, hiddeco/sshsig's Verify of a signature they made.
//   - `pgp`: go-crypto v1.4.1's packet.Reader.Next of RSA public key packets, their
//     exponents of up to three octets and more, as written (a leading zero counts).
//
// Keys come from a ChaCha8 stream of a fixed seed, so each run makes the same.
// `generate-rsa` copies this file into buildx's cmd/buildx and runs it there.

import (
	"bytes"
	"crypto"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"math/big"
	"math/rand/v2"
	"os"
	"testing"

	"github.com/ProtonMail/go-crypto/openpgp/packet"
	"github.com/hiddeco/sshsig"
	"golang.org/x/crypto/ssh"
)

type rsaVerifyCase struct {
	Name     string `json:"name"`
	N        string `json:"n"`
	E        string `json:"e"`
	Digest   string `json:"digest"`
	PKCS1    string `json:"pkcs1"`
	PSS      string `json:"pss"`
	KeyError string `json:"keyError,omitempty"`
	PKCS1Ok  string `json:"pkcs1Answer"`
	PSSAuto  string `json:"pssAutoAnswer"`
	PSSHash  string `json:"pssHashAnswer"`
}

type rsaSSHCase struct {
	Name      string `json:"name"`
	Wire      string `json:"wire"`
	Parse     string `json:"parse"`
	Message   string `json:"message,omitempty"`
	Signature string `json:"signature,omitempty"`
	Verify    string `json:"verify,omitempty"`
}

type rsaPGPCase struct {
	Name   string `json:"name"`
	Packet string `json:"packet"`
	Next   string `json:"next"`
}

type rsaOracle struct {
	Verify []rsaVerifyCase `json:"verify"`
	SSH    []rsaSSHCase    `json:"ssh"`
	PGP    []rsaPGPCase    `json:"pgp"`
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

func answerOf(err error) string {
	if err != nil {
		return err.Error()
	}
	return "ok"
}

// oddOfBits: a number of exactly `bits` bits from r, its top two bits and lowest set.
func oddOfBits(r *rand.ChaCha8, bits int) *big.Int {
	b := make([]byte, (bits+7)/8)
	_, _ = r.Read(b)
	x := new(big.Int).SetBytes(b)
	x.Rsh(x, uint(len(b)*8-bits))
	x.SetBit(x, bits-1, 1)
	x.SetBit(x, bits-2, 1)
	x.SetBit(x, 0, 1)
	return x
}

// rsaPrime: a prime of `bits` bits whose predecessor shares no factor with e.
func rsaPrime(r *rand.ChaCha8, bits int, e *big.Int) *big.Int {
	one := big.NewInt(1)
	for {
		p := oddOfBits(r, bits)
		if !p.ProbablyPrime(20) {
			continue
		}
		if new(big.Int).GCD(nil, nil, e, new(big.Int).Sub(p, one)).Cmp(one) == 0 {
			return p
		}
	}
}

type rsaKey struct {
	n, e, d, p, q *big.Int
}

// makeKey: primes of pBits and qBits bits, and d = e⁻¹ mod lcm(p-1, q-1).
func makeKey(r *rand.ChaCha8, pBits, qBits int, e *big.Int) rsaKey {
	one := big.NewInt(1)
	p := rsaPrime(r, pBits, e)
	q := rsaPrime(r, qBits, e)
	for p.Cmp(q) == 0 {
		q = rsaPrime(r, qBits, e)
	}
	pm1, qm1 := new(big.Int).Sub(p, one), new(big.Int).Sub(q, one)
	g := new(big.Int).GCD(nil, nil, pm1, qm1)
	lambda := new(big.Int).Div(new(big.Int).Mul(pm1, qm1), g)
	d := new(big.Int).ModInverse(e, lambda)
	return rsaKey{n: new(big.Int).Mul(p, q), e: e, d: d, p: p, q: q}
}

var sha256Info = []byte{0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20}

// pkcs1Encoded: EMSA-PKCS1-v1_5 of a SHA-256 digest, k octets (RFC 8017 §9.2).
func pkcs1Encoded(k int, digest []byte) []byte {
	t := append(append([]byte{}, sha256Info...), digest...)
	em := make([]byte, k)
	em[1] = 1
	for i := 2; i < k-len(t)-1; i++ {
		em[i] = 0xff
	}
	copy(em[k-len(t):], t)
	return em
}

func mgf1(seed []byte, n int) []byte {
	var out []byte
	for c := uint32(0); len(out) < n; c++ {
		h := sha256.New()
		h.Write(seed)
		_ = binary.Write(h, binary.BigEndian, c)
		out = h.Sum(out)
	}
	return out[:n]
}

// pssEncoded: EMSA-PSS-ENCODE of a SHA-256 digest with `salt`, emBits = modBits - 1
// (RFC 8017 §9.1.1).
func pssEncoded(modBits int, digest, salt []byte) []byte {
	emBits := modBits - 1
	emLen := (emBits + 7) / 8
	m := append(append(make([]byte, 8), digest...), salt...)
	h := sha256.Sum256(m)
	db := make([]byte, emLen-len(h)-1)
	db[len(db)-len(salt)-1] = 1
	copy(db[len(db)-len(salt):], salt)
	mask := mgf1(h[:], len(db))
	for i := range db {
		db[i] ^= mask[i]
	}
	db[0] &= 0xff >> (8*emLen - emBits)
	return append(append(db, h[:]...), 0xbc)
}

// signed: em raised to d mod n, as k octets.
func signed(k rsaKey, em []byte) []byte {
	s := new(big.Int).Exp(new(big.Int).SetBytes(em), k.d, k.n)
	return s.FillBytes(make([]byte, (k.n.BitLen()+7)/8))
}

func verifyCase(name string, n, e *big.Int, d *big.Int, digest, salt []byte) rsaVerifyCase {
	k := (n.BitLen() + 7) / 8
	key := rsaKey{n: n, e: e, d: d}
	pkcs1 := make([]byte, k)
	pss := make([]byte, k)
	if d != nil {
		pkcs1 = signed(key, pkcs1Encoded(k, digest))
		pss = signed(key, pssEncoded(n.BitLen(), digest, salt))
	}
	pub := &rsa.PublicKey{N: n, E: int(e.Int64())}
	c := rsaVerifyCase{
		Name:    name,
		N:       b64(n.Bytes()),
		E:       b64(e.Bytes()),
		Digest:  b64(digest),
		PKCS1:   b64(pkcs1),
		PSS:     b64(pss),
		PKCS1Ok: answerOf(rsa.VerifyPKCS1v15(pub, crypto.SHA256, digest, pkcs1)),
		PSSAuto: answerOf(rsa.VerifyPSS(pub, crypto.SHA256, digest, pss, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthAuto})),
		PSSHash: answerOf(rsa.VerifyPSS(pub, crypto.SHA256, digest, pss, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthEqualsHash})),
	}
	if err := rsa.VerifyPKCS1v15(pub, crypto.SHA256, make([]byte, 32), make([]byte, k)); err != nil && !errors.Is(err, rsa.ErrVerification) {
		c.KeyError = err.Error()
	}
	return c
}

func sshWire(e, n *big.Int) []byte {
	return ssh.Marshal(struct {
		Name string
		E    *big.Int
		N    *big.Int
	}{ssh.KeyAlgoRSA, e, n})
}

// pgpKeyPacket: a v4 RSA public key packet, its exponent's MPI `bits` long over `e`.
func pgpKeyPacket(n *big.Int, bits uint16, e []byte) []byte {
	var body []byte
	body = append(body, 4, 0x60, 0, 0, 0, 1)
	body = binary.BigEndian.AppendUint16(body, uint16(n.BitLen()))
	body = append(body, n.Bytes()...)
	body = binary.BigEndian.AppendUint16(body, bits)
	body = append(body, e...)
	out := []byte{0xc6, 0xff}
	out = binary.BigEndian.AppendUint32(out, uint32(len(body)))
	return append(out, body...)
}

func TestShardsRSA(t *testing.T) {
	out := os.Getenv("SHARDS_RSA_OUT")
	if out == "" {
		t.Skip("SHARDS_RSA_OUT names the file to write")
	}
	r := rand.NewChaCha8([32]byte{'s', 'h', 'a', 'r', 'd', 's', '-', 'r', 's', 'a'})
	digest := sha256.Sum256([]byte("signed by an exponent"))
	salt := make([]byte, 32)
	_, _ = r.Read(salt)
	var o rsaOracle

	base := makeKey(r, 512, 512, big.NewInt(65537))
	exponents := []struct {
		name string
		e    *big.Int
	}{
		{"one", big.NewInt(1)},
		{"two", big.NewInt(2)},
		{"three", big.NewInt(3)},
		{"seventeen", big.NewInt(17)},
		{"65537", big.NewInt(65537)},
		{"65536", big.NewInt(65536)},
		{"2^24-1", big.NewInt(1<<24 - 1)},
		{"2^24+1", big.NewInt(1<<24 + 1)},
		{"2^31-1", big.NewInt(1<<31 - 1)},
		{"2^31", big.NewInt(1 << 31)},
		{"2^31+1", big.NewInt(1<<31 + 1)},
		{"2^32+1", big.NewInt(1<<32 + 1)},
		{"2^62+1", big.NewInt(1<<62 + 1)},
	}
	keys := map[string]rsaKey{}
	for _, x := range exponents {
		if x.e.Bit(0) == 0 {
			// No d for an even exponent: the key is refused before any signature.
			o.Verify = append(o.Verify, verifyCase("exponent "+x.name, base.n, x.e, nil, digest[:], salt))
			continue
		}
		k := makeKey(r, 512, 512, x.e)
		keys[x.name] = k
		o.Verify = append(o.Verify, verifyCase("exponent "+x.name, k.n, k.e, k.d, digest[:], salt))
	}
	small := makeKey(r, 512, 511, big.NewInt(65537))
	o.Verify = append(o.Verify, verifyCase("a modulus of 1023 bits", small.n, small.e, small.d, digest[:], salt))
	even := new(big.Int).Add(base.n, big.NewInt(1))
	o.Verify = append(o.Verify, verifyCase("an even modulus", even, base.e, nil, digest[:], salt))
	wrong := verifyCase("a signature of another digest", base.n, base.e, base.d, digest[:], salt)
	other := sha256.Sum256([]byte("another"))
	wrong.Digest = b64(other[:])
	pub := &rsa.PublicKey{N: base.n, E: 65537}
	pkcs1, _ := base64.StdEncoding.DecodeString(wrong.PKCS1)
	pss, _ := base64.StdEncoding.DecodeString(wrong.PSS)
	wrong.PKCS1Ok = answerOf(rsa.VerifyPKCS1v15(pub, crypto.SHA256, other[:], pkcs1))
	wrong.PSSAuto = answerOf(rsa.VerifyPSS(pub, crypto.SHA256, other[:], pss, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthAuto}))
	wrong.PSSHash = answerOf(rsa.VerifyPSS(pub, crypto.SHA256, other[:], pss, &rsa.PSSOptions{SaltLength: rsa.PSSSaltLengthEqualsHash}))
	o.Verify = append(o.Verify, wrong)

	message := []byte("a Git commit, signed")
	sshExponents := append(exponents, struct {
		name string
		e    *big.Int
	}{"-65537", big.NewInt(-65537)})
	for _, x := range sshExponents {
		n := base.n
		k, made := keys[x.name]
		if made {
			n = k.n
		}
		wire := sshWire(x.e, n)
		c := rsaSSHCase{Name: "exponent " + x.name, Wire: b64(wire)}
		key, err := ssh.ParsePublicKey(wire)
		if err != nil {
			c.Parse = err.Error()
			o.SSH = append(o.SSH, c)
			continue
		}
		c.Parse = "ok " + key.Type()
		if made {
			priv := &rsa.PrivateKey{
				PublicKey: rsa.PublicKey{N: k.n, E: int(k.e.Int64())},
				D:         k.d,
				Primes:    []*big.Int{k.p, k.q},
			}
			priv.Precompute()
			signer, err := ssh.NewSignerFromKey(priv)
			if err != nil {
				t.Fatal(err)
			}
			sig, err := sshsig.SignWithRand(bytes.NewReader(message), r, signer, sshsig.HashSHA512, "git")
			if err != nil {
				t.Fatal(err)
			}
			c.Message = b64(message)
			c.Signature = b64(sshsig.Armor(sig))
			c.Verify = answerOf(sshsig.Verify(bytes.NewReader(message), sig, key, sshsig.HashSHA512, "git"))
		}
		o.SSH = append(o.SSH, c)
	}
	huge := oddOfBits(r, 16385)
	wire := sshWire(big.NewInt(65537), huge)
	_, err := ssh.ParsePublicKey(wire)
	o.SSH = append(o.SSH, rsaSSHCase{Name: "a modulus of 16385 bits", Wire: b64(wire), Parse: answerOf(err)})

	pgp := []struct {
		name string
		bits uint16
		e    []byte
	}{
		{"65537 in three octets", 17, []byte{1, 0, 1}},
		{"2^24-1 in three octets", 24, []byte{0xff, 0xff, 0xff}},
		{"65537 in four octets, a zero first", 32, []byte{0, 1, 0, 1}},
		{"2^24+1 in four octets", 25, []byte{1, 0, 0, 1}},
		{"2^31+1 in four octets", 32, []byte{0x80, 0, 0, 1}},
		{"2^32+1 in five octets", 33, []byte{1, 0, 0, 0, 1}},
		{"none", 0, nil},
	}
	for _, c := range pgp {
		pkt := pgpKeyPacket(base.n, c.bits, c.e)
		p, err := packet.NewReader(bytes.NewReader(pkt)).Next()
		next := answerOf(err)
		if err == io.EOF {
			next = "EOF"
		} else if err == nil {
			if _, ok := p.(*packet.PublicKey); !ok {
				t.Fatalf("%s: %T", c.name, p)
			}
		}
		o.PGP = append(o.PGP, rsaPGPCase{Name: c.name, Packet: b64(pkt), Next: next})
	}

	data, err := json.MarshalIndent(o, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
