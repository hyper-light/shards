package main

// x/crypto/ssh's answers (v0.55.0, as buildx v0.37.1 vendors it) about OpenSSH certificates,
// for crates/gitsign/tests/certs.rs: ParsePublicKey of certificates and keys in wire form,
// ParseAuthorizedKey of key files, and buildx's verify_git_signature (BuildKit's
// gitsign.VerifySignature: sshsig.Verify against the file's key) of SSH signatures made
// with certificates. `generate-certs` copies this file into buildx's cmd/buildx and runs it
// with the files ssh-keygen made.

import (
	"bytes"
	"crypto/dsa"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"io"
	"math/big"
	"os"
	"sort"
	"strconv"
	"strings"
	"testing"

	"github.com/hiddeco/sshsig"
	"github.com/moby/buildkit/util/gitutil/gitobject"
	"github.com/moby/buildkit/util/gitutil/gitsign"
	"golang.org/x/crypto/ssh"
)

type certParse struct {
	Name        string `json:"name"`
	Kind        string `json:"kind"` // "wire" or "authorized"
	Input       string `json:"input"`
	Type        string `json:"type,omitempty"`
	Fingerprint string `json:"fingerprint,omitempty"`
	Marshal     string `json:"marshal,omitempty"`
	Error       string `json:"error,omitempty"`
}

type certVerify struct {
	Name      string `json:"name"`
	Signature string `json:"signature"`
	Keys      string `json:"keys"`
	Error     string `json:"error"`
}

type certAnswers struct {
	Data   string       `json:"data"`
	Parse  []certParse  `json:"parse"`
	Verify []certVerify `json:"verify"`
}

func wireString(b []byte) []byte {
	out := binary.BigEndian.AppendUint32(nil, uint32(len(b)))
	return append(out, b...)
}

func u64(v uint64) []byte { return binary.BigEndian.AppendUint64(nil, v) }
func u32(v uint32) []byte { return binary.BigEndian.AppendUint32(nil, v) }

func cat(parts ...[]byte) []byte { return bytes.Join(parts, nil) }

// keyFields: a key's wire form without its type name.
func keyFields(k ssh.PublicKey) []byte {
	b := k.Marshal()
	n := binary.BigEndian.Uint32(b)
	return b[4+n:]
}

// generic: a certificate's key-independent fields, each settable to any bytes.
type generic struct {
	serial     uint64
	certType   uint32
	keyID      []byte
	principals []byte
	after      uint64
	before     uint64
	critical   []byte
	extensions []byte
	reserved   []byte
	sigKey     []byte
	sig        []byte
}

func (g generic) bytes() []byte {
	return cat(u64(g.serial), u32(g.certType), wireString(g.keyID), wireString(g.principals),
		u64(g.after), u64(g.before), wireString(g.critical), wireString(g.extensions),
		wireString(g.reserved), wireString(g.sigKey), wireString(g.sig))
}

func tuple(k, v string) []byte {
	if v == "" {
		return cat(wireString([]byte(k)), wireString(nil))
	}
	return cat(wireString([]byte(k)), wireString(wireString([]byte(v))))
}

func certBlob(algo string, key ssh.PublicKey, g generic) []byte {
	return cat(wireString([]byte(algo)), wireString([]byte("nonce-0123456789abcdef0123456789")), keyFields(key), g.bytes())
}

func TestShardsSSHCertOracle(t *testing.T) {
	in, out := os.Getenv("SHARDS_CERT_FILES"), os.Getenv("SHARDS_CERT_OUT")
	if out == "" {
		t.Skip("SHARDS_CERT_OUT names the file to write")
	}
	raw, err := os.ReadFile(in)
	if err != nil {
		t.Fatal(err)
	}
	files := map[string]string{}
	if err := json.Unmarshal(raw, &files); err != nil {
		t.Fatal(err)
	}
	data := []byte(files["data"])
	var a certAnswers
	a.Data = base64.StdEncoding.EncodeToString(data)

	wire := func(name string, blob []byte) {
		c := certParse{Name: name, Kind: "wire", Input: base64.StdEncoding.EncodeToString(blob)}
		k, err := ssh.ParsePublicKey(blob)
		if err != nil {
			c.Error = err.Error()
		} else {
			c.Type, c.Fingerprint, c.Marshal = k.Type(), ssh.FingerprintSHA256(k), base64.StdEncoding.EncodeToString(k.Marshal())
		}
		a.Parse = append(a.Parse, c)
	}
	authorized := func(name, text string) {
		c := certParse{Name: name, Kind: "authorized", Input: text}
		k, _, _, _, err := ssh.ParseAuthorizedKey([]byte(text))
		if err != nil {
			c.Error = err.Error()
		} else {
			c.Type, c.Fingerprint, c.Marshal = k.Type(), ssh.FingerprintSHA256(k), base64.StdEncoding.EncodeToString(k.Marshal())
		}
		a.Parse = append(a.Parse, c)
	}
	verify := func(name, sig, keys string) {
		c := certVerify{Name: name, Signature: sig, Keys: keys}
		obj := &gitobject.GitObject{Signature: sig, SignedData: string(data)}
		if err := gitsign.VerifySignature(obj, []byte(keys), nil); err != nil {
			c.Error = err.Error()
		}
		a.Verify = append(a.Verify, c)
	}
	blobOf := func(line string) []byte {
		f := strings.Fields(line)
		b, err := base64.StdEncoding.DecodeString(f[1])
		if err != nil {
			t.Fatal(err)
		}
		return b
	}

	// ssh-keygen's: each certificate as a file says it, with options and a comment, and
	// declared as another type; the signatures made with it and with its key.
	var names []string
	for f := range files {
		if strings.HasSuffix(f, "-cert.pub") && !strings.Contains(f, "_again") {
			names = append(names, strings.TrimSuffix(f, "-cert.pub"))
		}
	}
	sort.Strings(names)
	for _, n := range names {
		cert := files[n+"-cert.pub"]
		again := files[n+"_again-cert.pub"]
		plain := files[n+".pub"]
		authorized(n+" certificate", cert)
		authorized(n+" certificate with options", `cert-authority,from="10.*" `+cert)
		authorized(n+" certificate declared as its key's type", strings.Replace(cert, "-cert-v01@openssh.com", "", 1))
		authorized(n+" certificate and key", "# keys\n"+plain+cert)
		wire(n+" certificate blob", blobOf(cert))
		wire(n+" certificate blob with a byte after", append(blobOf(cert), 0))
		verify(n+" signed with its certificate, the certificate as key", files[n+"_cert.sig"], cert)
		verify(n+" signed with its certificate, after its key's line", files[n+"_cert.sig"], plain+cert)
		verify(n+" signed with its certificate, its key as key", files[n+"_cert.sig"], plain)
		verify(n+" signed with its key, the certificate as key", files[n+"_plain.sig"], cert)
		verify(n+" signed with its certificate, another certificate of its key", files[n+"_cert.sig"], again)
		verify(n+" signed with its key, its key as key", files[n+"_plain.sig"], plain)
	}
	for _, ca := range []string{"ca_ed25519", "ca_ecdsa", "ca_rsa"} {
		verify("ed25519 signed with its certificate, "+ca+" as key", files["ed25519_cert.sig"], files[ca+".pub"])
	}

	// Every truncation of ssh-keygen's Ed25519 certificate, every fifth of its RSA one.
	ed := blobOf(files["ed25519-cert.pub"])
	for i := 0; i < len(ed); i++ {
		wire("ed25519 certificate cut to "+strconv.Itoa(i), ed[:i])
	}
	rsaCert := blobOf(files["rsa2048-cert.pub"])
	for i := 0; i < len(rsaCert); i += 5 {
		wire("rsa certificate cut to "+strconv.Itoa(i), rsaCert[:i])
	}

	// Crafted certificates: each field of the generic part, and keys ssh-keygen does not make.
	edPub, edPriv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	edKey, err := ssh.NewPublicKey(edPub)
	if err != nil {
		t.Fatal(err)
	}
	caPub, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	caKey, err := ssh.NewPublicKey(caPub)
	if err != nil {
		t.Fatal(err)
	}
	sigBody := cat(wireString([]byte("ssh-ed25519")), wireString(make([]byte, 64)))
	base := generic{
		serial: 7, certType: ssh.UserCert, keyID: []byte("crafted"),
		principals: cat(wireString([]byte("alice")), wireString([]byte("bob"))),
		after:      1, before: ssh.CertTimeInfinity,
		critical:   cat(tuple("force-command", "/bin/true"), tuple("verify-required", "")),
		extensions: cat(tuple("permit-pty", ""), tuple("z@shards.invalid", "v")),
		sigKey:     caKey.Marshal(), sig: sigBody,
	}
	ed25519Cert := "ssh-ed25519-cert-v01@openssh.com"
	with := func(f func(*generic)) []byte {
		g := base
		f(&g)
		return certBlob(ed25519Cert, edKey, g)
	}
	wire("crafted", with(func(*generic) {}))
	wire("crafted, a reserved field", with(func(g *generic) { g.reserved = []byte("future") }))
	wire("crafted, no principals or options", with(func(g *generic) { g.principals, g.critical, g.extensions = nil, nil, nil }))
	wire("crafted, a principal cut short", with(func(g *generic) { g.principals = append(g.principals, 0, 0, 0, 9, 'x') }))
	wire("crafted, options out of order", with(func(g *generic) {
		g.critical = cat(tuple("verify-required", ""), tuple("force-command", "/bin/true"))
	}))
	wire("crafted, an option twice", with(func(g *generic) { g.critical = cat(tuple("a", ""), tuple("a", "")) }))
	wire("crafted, extensions out of order", with(func(g *generic) { g.extensions = cat(tuple("z", ""), tuple("a", "")) }))
	wire("crafted, an option's value with bytes after it", with(func(g *generic) {
		g.critical = cat(wireString([]byte("a")), wireString(cat(wireString([]byte("v")), []byte{1})))
	}))
	wire("crafted, an option's value an empty string", with(func(g *generic) {
		g.critical = cat(wireString([]byte("a")), wireString(wireString(nil)))
	}))
	wire("crafted, an option's value cut short", with(func(g *generic) {
		g.critical = cat(wireString([]byte("a")), wireString([]byte{0, 0, 0, 5, 'v'}))
	}))
	wire("crafted, an option's name cut short", with(func(g *generic) { g.critical = []byte{0, 0, 0, 9, 'a'} }))
	wire("crafted, an option without a value", with(func(g *generic) { g.critical = wireString([]byte("a")) }))
	wire("crafted, signed by a certificate", with(func(g *generic) { g.sigKey = with(func(*generic) {}) }))
	wire("crafted, signed by an RSA SHA-2 certificate type", with(func(g *generic) {
		g.sigKey = cat(wireString([]byte("rsa-sha2-256-cert-v01@openssh.com")), []byte("x"))
	}))
	wire("crafted, a signature key with no type", with(func(g *generic) { g.sigKey = nil }))
	wire("crafted, a signature key of no known type", with(func(g *generic) { g.sigKey = wireString([]byte("foo")) }))
	wire("crafted, a signature key with junk after it", with(func(g *generic) { g.sigKey = append(caKey.Marshal(), 1) }))
	wire("crafted, a signature key cut short", with(func(g *generic) { g.sigKey = caKey.Marshal()[:20] }))
	wire("crafted, no signature", with(func(g *generic) { g.sig = nil }))
	wire("crafted, a signature with a format alone", with(func(g *generic) { g.sig = wireString([]byte("ssh-ed25519")) }))
	wire("crafted, a signature with bytes after it", with(func(g *generic) { g.sig = append(sigBody, 1, 2) }))
	for _, f := range []string{"sk-ssh-ed25519@openssh.com", "sk-ecdsa-sha2-nistp256@openssh.com", "sk-ssh-ed25519-cert-v01@openssh.com", "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com"} {
		wire("crafted, a "+f+" signature with its flags and counter", with(func(g *generic) {
			g.sig = cat(wireString([]byte(f)), wireString(make([]byte, 64)), []byte{1, 0, 0, 0, 7})
		}))
	}
	wire("crafted, a host certificate", with(func(g *generic) { g.certType = ssh.HostCert }))
	wire("crafted, of another type number", with(func(g *generic) { g.certType = 77 }))
	key := wireString([]byte("nonce"))
	gen := base.bytes()
	wire("crafted, nothing after its key", cat(wireString([]byte(ed25519Cert)), key, keyFields(edKey)))
	wire("crafted, a serial alone", cat(wireString([]byte(ed25519Cert)), key, keyFields(edKey), u64(1)))
	wire("crafted, its key id cut short", cat(wireString([]byte(ed25519Cert)), key, keyFields(edKey), u64(1), u32(1), []byte{0, 0, 1, 0, 'x'}))
	wire("crafted, a byte after it", cat(wireString([]byte(ed25519Cert)), key, keyFields(edKey), gen, []byte{0}))
	wire("crafted, no nonce", wireString([]byte(ed25519Cert)))
	wire("crafted, its key empty", cat(wireString([]byte(ed25519Cert)), key))
	wire("crafted, its key of another size", cat(wireString([]byte(ed25519Cert)), key, wireString(make([]byte, 31)), gen))
	wire("crafted, an RSA SHA-2 certificate type", cat(wireString([]byte("rsa-sha2-512-cert-v01@openssh.com")), key, keyFields(edKey), gen))

	// ECDSA certificates of the wrong curve, and keys of each type, empty or cut short.
	p384, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	p384Key, err := ssh.NewPublicKey(&p384.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	wire("ecdsa nistp256 certificate of a nistp384 key", certBlob("ecdsa-sha2-nistp256-cert-v01@openssh.com", p384Key, base))
	wire("ecdsa nistp384 certificate", certBlob("ecdsa-sha2-nistp384-cert-v01@openssh.com", p384Key, base))
	for _, algo := range []string{"ssh-rsa", "ssh-dss", "ecdsa-sha2-nistp256", "sk-ecdsa-sha2-nistp256@openssh.com", "ssh-ed25519", "sk-ssh-ed25519@openssh.com"} {
		wire(algo+" empty", wireString([]byte(algo)))
		wire(algo+" certificate of an empty key", cat(wireString([]byte(algo+"-cert-v01@openssh.com")), key))
	}
	wire("ecdsa curve cut short", cat(wireString([]byte("ecdsa-sha2-nistp256")), []byte{0, 0, 0, 8, 'n'}))
	wire("ecdsa point missing", cat(wireString([]byte("ecdsa-sha2-nistp256")), wireString([]byte("nistp256"))))

	// Security keys: no ssh-keygen without the device, so their keys made here.
	p256, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	point := elliptic.Marshal(elliptic.P256(), p256.X, p256.Y)
	skEcdsaKey := cat(wireString([]byte("nistp256")), wireString(point))
	skEdKey := wireString(edPub)
	for _, sk := range []struct {
		algo, cert string
		key, app   []byte
	}{
		{"sk-ecdsa-sha2-nistp256@openssh.com", "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com", skEcdsaKey, wireString([]byte("ssh:"))},
		{"sk-ssh-ed25519@openssh.com", "sk-ssh-ed25519-cert-v01@openssh.com", skEdKey, wireString([]byte("ssh:shards"))},
	} {
		fields := cat(sk.key, sk.app)
		wire(sk.algo, cat(wireString([]byte(sk.algo)), fields))
		wire(sk.algo+" without its application", cat(wireString([]byte(sk.algo)), sk.key))
		wire(sk.algo+" with its application cut short", cat(wireString([]byte(sk.algo)), sk.key, sk.app[:len(sk.app)-1]))
		wire(sk.cert, cat(wireString([]byte(sk.cert)), key, fields, gen))
		authorized(sk.cert+" in a file", sk.cert+" "+base64.StdEncoding.EncodeToString(cat(wireString([]byte(sk.cert)), key, fields, gen))+" a comment\n")
	}

	// DSA: generated here at FIPS 186-2's one size, its parameters' checks, and a
	// certificate of it.
	var params dsa.Parameters
	if err := dsa.GenerateParameters(&params, rand.Reader, dsa.L1024N160); err != nil {
		t.Fatal(err)
	}
	dsaPriv := &dsa.PrivateKey{PublicKey: dsa.PublicKey{Parameters: params}}
	if err := dsa.GenerateKey(dsaPriv, rand.Reader); err != nil {
		t.Fatal(err)
	}
	mp := func(i *big.Int) []byte { return ssh.Marshal(struct{ N *big.Int }{i}) }
	dsaKey := func(p, q, g, y *big.Int) []byte {
		return cat(wireString([]byte("ssh-dss")), mp(p), mp(q), mp(g), mp(y))
	}
	dp := &dsaPriv.PublicKey
	wire("dsa", dsaKey(dp.P, dp.Q, dp.G, dp.Y))
	wire("dsa with a 161-bit Q", dsaKey(dp.P, new(big.Int).Lsh(big.NewInt(1), 160), dp.G, dp.Y))
	wire("dsa with G as P", dsaKey(dp.P, dp.Q, dp.P, dp.Y))
	wire("dsa with G zero", dsaKey(dp.P, dp.Q, big.NewInt(0), dp.Y))
	wire("dsa with G negative", dsaKey(dp.P, dp.Q, big.NewInt(-5), dp.Y))
	wire("dsa with Y as P", dsaKey(dp.P, dp.Q, dp.G, dp.P))
	dsaPublic, err := ssh.NewPublicKey(dp)
	if err != nil {
		t.Fatal(err)
	}
	wire("dsa certificate", certBlob("ssh-dss-cert-v01@openssh.com", dsaPublic, base))

	// Signatures made here with certificates: by a key a CA never signed for (the CA's
	// signature is never checked), by DSA, and with another key's certificate.
	signWith := func(cert *ssh.Certificate, signer ssh.Signer) string {
		cs, err := ssh.NewCertSigner(cert, signer)
		if err != nil {
			t.Fatal(err)
		}
		s, err := sshsig.Sign(bytes.NewReader(data), cs, sshsig.HashSHA512, "git")
		if err != nil {
			t.Fatal(err)
		}
		return string(sshsig.Armor(s))
	}
	certOf := func(k ssh.PublicKey) *ssh.Certificate {
		return &ssh.Certificate{
			Nonce: []byte("n"), Key: k, Serial: 1, CertType: ssh.UserCert, KeyId: "go",
			ValidPrincipals: []string{"alice"}, ValidBefore: ssh.CertTimeInfinity,
			Permissions:  ssh.Permissions{Extensions: map[string]string{"permit-pty": ""}},
			SignatureKey: caKey, Signature: &ssh.Signature{Format: "ssh-ed25519", Blob: make([]byte, 64)},
		}
	}
	edSigner, err := ssh.NewSignerFromKey(edPriv)
	if err != nil {
		t.Fatal(err)
	}
	edCert := certOf(edKey)
	line := func(c *ssh.Certificate) string { return string(ssh.MarshalAuthorizedKey(c)) }
	verify("go ed25519 certificate with a CA signature of zeros", signWith(edCert, edSigner), line(edCert))
	dsaSigner, err := ssh.NewSignerFromKey(dsaPriv)
	if err != nil {
		t.Fatal(err)
	}
	dsaCert := certOf(dsaPublic)
	verify("go dsa certificate", signWith(dsaCert, dsaSigner), line(dsaCert))
	// The certificate of one key, signed by another.
	_, otherPriv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	otherSigner, err := ssh.NewSignerFromKey(otherPriv)
	if err != nil {
		t.Fatal(err)
	}
	s, err := sshsig.Sign(bytes.NewReader(data), lying{edCert, otherSigner}, sshsig.HashSHA512, "git")
	if err != nil {
		t.Fatal(err)
	}
	verify("go ed25519 certificate, signed by another key", string(sshsig.Armor(s)), line(edCert))

	b, err := json.MarshalIndent(a, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// lying: a signer that names one key and signs with another.
type lying struct {
	pub    ssh.PublicKey
	signer ssh.Signer
}

func (l lying) PublicKey() ssh.PublicKey { return l.pub }
func (l lying) Sign(r io.Reader, data []byte) (*ssh.Signature, error) {
	return l.signer.Sign(r, data)
}
