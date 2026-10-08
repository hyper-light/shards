package oracle

import (
	"crypto/dsa"
	"crypto/ecdh"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/asn1"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"math/big"
	"net"
	"os"
	"strings"
	"testing"

	"golang.org/x/crypto/ssh"
	"golang.org/x/crypto/ssh/agent"
)

// An exchange: a request a step sends the agent, and the agent's answer, in hex; a
// signature by a randomized algorithm (ECDSA) is marked so, and held to its format and
// its verification instead of its bytes.
type exchange struct {
	Request    string `json:"request"`
	Answer     string `json:"answer"`
	Randomized bool   `json:"randomized,omitempty"`
}

// A case: the key files of one `--ssh ID=FILE,FILE...`, in order; the error buildx's
// agent gives the first it cannot take, or the exchanges of the agent it makes of them.
type kase struct {
	Name  string   `json:"name"`
	Files []string `json:"files"`
	Error string   `json:"error,omitempty"`
	// Which step gave the error: "parse" (buildx: "failed to parse FILE") or "add"
	// ("failed to add FILE to agent").
	Stage     string     `json:"stage,omitempty"`
	Exchanges []exchange `json:"exchanges,omitempty"`
}

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func pemOf(typ string, der []byte) string {
	return string(pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: der}))
}

func openssh(t *testing.T, k any) string {
	return string(pem.EncodeToMemory(must(ssh.MarshalPrivateKey(k, ""))))
}

func pkcs8(t *testing.T, k any) string {
	return pemOf("PRIVATE KEY", must(x509.MarshalPKCS8PrivateKey(k)))
}

// The OpenSSH key format's outer record (PROTOCOL.key), to make malformed ones from.
type outer struct {
	CipherName   string
	KdfName      string
	KdfOpts      string
	NumKeys      uint32
	PubKey       []byte
	PrivKeyBlock []byte
}

type inner struct {
	Check1  uint32
	Check2  uint32
	Keytype string
	Rest    []byte `ssh:"rest"`
}

const magic = "openssh-key-v1\x00"

// reshape decodes an OpenSSH key file, lets f change its records, and encodes it again.
func reshape(t *testing.T, file string, f func(*outer, *inner)) string {
	block, _ := pem.Decode([]byte(file))
	var o outer
	if err := ssh.Unmarshal(block.Bytes[len(magic):], &o); err != nil {
		t.Fatal(err)
	}
	var i inner
	if err := ssh.Unmarshal(o.PrivKeyBlock, &i); err != nil {
		t.Fatal(err)
	}
	f(&o, &i)
	o.PrivKeyBlock = ssh.Marshal(&i)
	return pemOf("OPENSSH PRIVATE KEY", append([]byte(magic), ssh.Marshal(&o)...))
}

func str(b []byte) []byte {
	out := binary.BigEndian.AppendUint32(nil, uint32(len(b)))
	return append(out, b...)
}

func sign(blob []byte, data string, flags uint32) []byte {
	req := append([]byte{13}, str(blob)...)
	req = append(req, str([]byte(data))...)
	return binary.BigEndian.AppendUint32(req, flags)
}

// serve asks a keyring holding files' keys each request, as buildx's agent is asked.
func serve(t *testing.T, c *kase) {
	ring := agent.NewKeyring()
	var blobs [][]byte
	var ecdsaKeys = map[string]bool{}
	for _, f := range c.Files {
		k, err := ssh.ParseRawPrivateKey([]byte(f))
		if err != nil {
			c.Error, c.Stage = err.Error(), "parse"
			return
		}
		if err := ring.Add(agent.AddedKey{PrivateKey: k}); err != nil {
			c.Error, c.Stage = err.Error(), "add"
			return
		}
		signer := must(ssh.NewSignerFromKey(k))
		blob := signer.PublicKey().Marshal()
		blobs = append(blobs, blob)
		if strings.HasPrefix(signer.PublicKey().Type(), "ecdsa-") {
			ecdsaKeys[string(blob)] = true
		}
	}
	server, client := net.Pipe()
	go agent.ServeAgent(ring, server)
	defer client.Close()
	ask := func(req []byte, randomized bool) {
		msg := binary.BigEndian.AppendUint32(nil, uint32(len(req)))
		if _, err := client.Write(append(msg, req...)); err != nil {
			t.Fatal(err)
		}
		var n [4]byte
		if _, err := readFull(client, n[:]); err != nil {
			t.Fatal(err)
		}
		answer := make([]byte, binary.BigEndian.Uint32(n[:]))
		if _, err := readFull(client, answer); err != nil {
			t.Fatal(err)
		}
		c.Exchanges = append(c.Exchanges, exchange{
			Request:    hex.EncodeToString(req),
			Answer:     hex.EncodeToString(answer),
			Randomized: randomized && len(answer) > 0 && answer[0] == 14,
		})
	}
	ask([]byte{11}, false)
	for _, blob := range blobs {
		for _, flags := range []uint32{0, 2, 4, 6} {
			ask(sign(blob, "shards signs this", flags), ecdsaKeys[string(blob)])
		}
	}
	ask(sign(must(ssh.NewPublicKey(ed25519.PublicKey(make([]byte, 32)))).Marshal(), "x", 0), false)
	ask(append([]byte{23}, str([]byte("pass"))...), false)
	ask([]byte{13, 0, 0}, false)
}

func readFull(c net.Conn, b []byte) (int, error) {
	n := 0
	for n < len(b) {
		m, err := c.Read(b[n:])
		n += m
		if err != nil {
			return n, err
		}
	}
	return n, nil
}

func TestShardsSSHKey(t *testing.T) {
	_, ed := must2(t)(ed25519.GenerateKey(rand.Reader))
	_, ed2 := must2(t)(ed25519.GenerateKey(rand.Reader))
	p256 := must(ecdsa.GenerateKey(elliptic.P256(), rand.Reader))
	p384 := must(ecdsa.GenerateKey(elliptic.P384(), rand.Reader))
	p521 := must(ecdsa.GenerateKey(elliptic.P521(), rand.Reader))
	rsa2048 := must(rsa.GenerateKey(rand.Reader, 2048))
	rsa3072 := must(rsa.GenerateKey(rand.Reader, 3072))
	rsa1024 := must(rsa.GenerateKey(rand.Reader, 1024))
	x25519 := must(ecdh.X25519().GenerateKey(rand.Reader))

	var params dsa.Parameters
	if err := dsa.GenerateParameters(&params, rand.Reader, dsa.L1024N160); err != nil {
		t.Fatal(err)
	}
	dk := dsa.PrivateKey{PublicKey: dsa.PublicKey{Parameters: params}}
	if err := dsa.GenerateKey(&dk, rand.Reader); err != nil {
		t.Fatal(err)
	}
	dsaDER := must(asn1.Marshal(struct {
		Version       int
		P, Q, G, Y, X *big.Int
	}{0, dk.P, dk.Q, dk.G, dk.Y, dk.X}))

	// A key whose exponent is even: its other parts are another key's.
	evenE := &rsa.PrivateKey{PublicKey: rsa.PublicKey{N: rsa2048.N, E: 65536}, D: rsa2048.D, Primes: rsa2048.Primes}
	evenE.Precomputed = rsa2048.Precomputed

	edFile := openssh(t, ed)
	encrypted := string(pem.EncodeToMemory(must(ssh.MarshalPrivateKeyWithPassphrase(ed, "", []byte("pass")))))
	//lint:ignore SA1019 the legacy encrypted PEM form, as old tools wrote it
	encPEM := must(x509.EncryptPEMBlock(rand.Reader, "RSA PRIVATE KEY", x509.MarshalPKCS1PrivateKey(rsa2048), []byte("pass"), x509.PEMCipherAES256))

	cases := []kase{
		{Name: "ed25519-openssh", Files: []string{edFile}},
		{Name: "ed25519-pkcs8", Files: []string{pkcs8(t, ed)}},
		{Name: "ed25519-crlf-in-text", Files: []string{"a note first\r\n" + strings.ReplaceAll(edFile, "\n", "\r\n") + "and after\r\n"}},
		{Name: "ecdsa-p256-openssh", Files: []string{openssh(t, p256)}},
		{Name: "ecdsa-p384-openssh", Files: []string{openssh(t, p384)}},
		{Name: "ecdsa-p521-openssh", Files: []string{openssh(t, p521)}},
		{Name: "ecdsa-p256-sec1", Files: []string{pemOf("EC PRIVATE KEY", must(x509.MarshalECPrivateKey(p256)))}},
		{Name: "ecdsa-p384-pkcs8", Files: []string{pkcs8(t, p384)}},
		{Name: "ecdsa-p521-sec1", Files: []string{pemOf("EC PRIVATE KEY", must(x509.MarshalECPrivateKey(p521)))}},
		{Name: "rsa2048-openssh", Files: []string{openssh(t, rsa2048)}},
		{Name: "rsa2048-pkcs1", Files: []string{pemOf("RSA PRIVATE KEY", x509.MarshalPKCS1PrivateKey(rsa2048))}},
		{Name: "rsa3072-pkcs8", Files: []string{pkcs8(t, rsa3072)}},
		{Name: "rsa3072-openssh", Files: []string{openssh(t, rsa3072)}},
		{Name: "several-and-again", Files: []string{edFile, openssh(t, p256), openssh(t, ed2), pkcs8(t, ed)}},
		{Name: "rsa1024-openssh", Files: []string{openssh(t, rsa1024)}},
		{Name: "dsa-pem", Files: []string{pemOf("DSA PRIVATE KEY", dsaDER)}},
		{Name: "x25519-pkcs8", Files: []string{pkcs8(t, x25519)}},
		{Name: "no-pem", Files: []string{"not a key\n"}},
		// pem.Decode's skips: a block whose END line runs on, one whose END names
		// another type (of the same length), one never ended; then the key.
		{Name: "pem-end-runs-on", Files: []string{strings.Replace(openssh(t, ed2), "-----END OPENSSH PRIVATE KEY-----", "-----END OPENSSH PRIVATE KEY----- more", 1) + edFile}},
		{Name: "pem-end-other-type", Files: []string{strings.Replace(edFile, "-----END OPENSSH PRIVATE KEY", "-----END OPENSSH PRIVATE KEX", 1) + pkcs8(t, p384)}},
		{Name: "pem-headers", Files: []string{strings.Replace(edFile, "-----\n", "-----\nComment: a key\nX-Note: two\n\n", 1)}},
		{Name: "pem-bad-base64-then-key", Files: []string{"-----BEGIN OPENSSH PRIVATE KEY-----\n@@@@\n-----END OPENSSH PRIVATE KEY-----\n" + edFile}},
		{Name: "pem-unended", Files: []string{"-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n"}},
		{Name: "public-key", Files: []string{string(ssh.MarshalAuthorizedKey(must(ssh.NewPublicKey(ed.Public()))))}},
		{Name: "unsupported-type", Files: []string{pemOf("CERTIFICATE", []byte{1, 2, 3})}},
		{Name: "encrypted-openssh", Files: []string{encrypted}},
		{Name: "encrypted-pem", Files: []string{string(pem.EncodeToMemory(encPEM))}},
		{Name: "ok-then-encrypted", Files: []string{edFile, encrypted}},
		{Name: "bad-magic", Files: []string{pemOf("OPENSSH PRIVATE KEY", []byte("openssh-key-v2\x00rest"))}},
		{Name: "multi-key", Files: []string{reshape(t, edFile, func(o *outer, _ *inner) { o.NumKeys = 2 })}},
		{Name: "checks-differ", Files: []string{reshape(t, edFile, func(_ *outer, i *inner) { i.Check2 ^= 1 })}},
		{Name: "kdf-options", Files: []string{reshape(t, edFile, func(o *outer, _ *inner) { o.KdfOpts = "x" })}},
		{Name: "bad-padding", Files: []string{reshape(t, edFile, func(_ *outer, i *inner) { i.Rest = append(i.Rest, 9) })}},
		{Name: "unhandled-type", Files: []string{reshape(t, edFile, func(_ *outer, i *inner) { i.Keytype = "ssh-dss" })}},
		{Name: "rsa-exponent-even", Files: []string{openssh(t, evenE)}},
	}
	for i := range cases {
		serve(t, &cases[i])
	}
	out := must(json.MarshalIndent(cases, "", "  "))
	if err := os.WriteFile(os.Getenv("SHARDS_SSHKEY_OUT"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func must2(t *testing.T) func(ed25519.PublicKey, ed25519.PrivateKey, error) (ed25519.PublicKey, ed25519.PrivateKey) {
	return func(p ed25519.PublicKey, k ed25519.PrivateKey, err error) (ed25519.PublicKey, ed25519.PrivateKey) {
		if err != nil {
			t.Fatal(err)
		}
		return p, k
	}
}
