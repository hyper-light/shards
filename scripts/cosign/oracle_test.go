package cosign

// What cosign v3.1.3 makes of private keys and NaCl secretboxes, for shards' cosign key
// reader and signer (crates/sigstore cosignkey.rs, secretbox.rs, sign.rs): keys of each
// type cosign signs with, encrypted with each scrypt parameter set go-securesystemslib
// takes, and crafted ones, each with LoadPrivateKey's PKCS #8 and key hint, or its error;
// and secretboxes x/crypto seals. Run by scripts/cosign/generate inside a cosign v3.1.3
// checkout, as pkg/cosign/zz_shards_oracle_test.go.

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"os"
	"strings"
	"testing"

	"github.com/secure-systems-lab/go-securesystemslib/encrypted"
	"golang.org/x/crypto/nacl/secretbox"
)

type shardsBox struct {
	Key     string `json:"key"`
	Nonce   string `json:"nonce"`
	Message string `json:"message"`
	Box     string `json:"box"`
}

type shardsKey struct {
	Name     string `json:"name"`
	PEM      string `json:"pem"`
	Password string `json:"password"`
	PKCS8    string `json:"pkcs8,omitempty"`
	Hint     string `json:"hint,omitempty"`
	Error    string `json:"error,omitempty"`
}

func shardsMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func shardsRandom(t *testing.T, n int) []byte {
	b := make([]byte, n)
	_, err := rand.Read(b)
	shardsMust(t, err)
	return b
}

// shardsEncrypted is what generate-key-pair writes of `der` (keys.go marshalKeyPair):
// encrypted's JSON under `level`, in a block of `kind`.
func shardsEncrypted(t *testing.T, der, pass []byte, level encrypted.KDFParameterStrength, kind string) []byte {
	enc, err := encrypted.EncryptWithCustomKDFParameters(der, pass, level)
	shardsMust(t, err)
	return pem.EncodeToMemory(&pem.Block{Type: kind, Bytes: enc})
}

// shardsAnswer is LoadPrivateKey's answer for a key: the PKCS #8 it decrypted and the
// hint NewSignerVerifierKeypair names its public key by, or its error.
func shardsAnswer(name string, pemBytes, pass []byte) shardsKey {
	k := shardsKey{Name: name, PEM: string(pemBytes), Password: string(pass)}
	sv, err := LoadPrivateKey(pemBytes, pass, nil)
	if err != nil {
		k.Error = err.Error()
		return k
	}
	p, _ := pem.Decode(pemBytes)
	der, err := encrypted.Decrypt(p.Bytes, pass)
	if err != nil {
		k.Error = err.Error()
		return k
	}
	k.PKCS8 = hex.EncodeToString(der)
	pub, err := sv.PublicKey()
	if err != nil {
		k.Error = err.Error()
		return k
	}
	spki, err := x509.MarshalPKIXPublicKey(pub)
	if err != nil {
		k.Error = err.Error()
		return k
	}
	sum := sha256.Sum256(spki)
	k.Hint = base64.StdEncoding.EncodeToString(sum[:])
	return k
}

func TestShardsCosignOracle(t *testing.T) {
	out := os.Getenv("SHARDS_COSIGN_ORACLE")
	if out == "" {
		t.Skip("SHARDS_COSIGN_ORACLE names where the answers go")
	}
	var boxes []shardsBox
	for _, n := range []int{0, 1, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1000} {
		var key [32]byte
		var nonce [24]byte
		copy(key[:], shardsRandom(t, 32))
		copy(nonce[:], shardsRandom(t, 24))
		msg := shardsRandom(t, n)
		boxes = append(boxes, shardsBox{
			Key:     hex.EncodeToString(key[:]),
			Nonce:   hex.EncodeToString(nonce[:]),
			Message: hex.EncodeToString(msg),
			Box:     hex.EncodeToString(secretbox.Seal(nil, msg, &nonce, &key)),
		})
	}

	var keys []shardsKey
	pkcs8 := func(k crypto.PrivateKey) []byte {
		der, err := x509.MarshalPKCS8PrivateKey(k)
		shardsMust(t, err)
		return der
	}
	p256, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	shardsMust(t, err)
	p384, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	shardsMust(t, err)
	p521, err := ecdsa.GenerateKey(elliptic.P521(), rand.Reader)
	shardsMust(t, err)
	p224, err := ecdsa.GenerateKey(elliptic.P224(), rand.Reader)
	shardsMust(t, err)
	_, ed, err := ed25519.GenerateKey(rand.Reader)
	shardsMust(t, err)
	rsa2048, err := rsa.GenerateKey(rand.Reader, 2048)
	shardsMust(t, err)
	rsa1024, err := rsa.GenerateKey(rand.Reader, 1024)
	shardsMust(t, err)
	pass := []byte("a password, 1")
	for _, c := range []struct {
		name string
		der  []byte
	}{
		{"ecdsa p256", pkcs8(p256)},
		{"ecdsa p384", pkcs8(p384)},
		{"ecdsa p521", pkcs8(p521)},
		{"ed25519", pkcs8(ed)},
		{"rsa 2048", pkcs8(rsa2048)},
	} {
		keys = append(keys, shardsAnswer(c.name+" standard", shardsEncrypted(t, c.der, pass, encrypted.Standard, SigstorePrivateKeyPemType), pass))
	}
	der := pkcs8(p256)
	keys = append(keys,
		shardsAnswer("legacy", shardsEncrypted(t, der, pass, encrypted.Legacy, SigstorePrivateKeyPemType), pass),
		shardsAnswer("owasp", shardsEncrypted(t, der, pass, encrypted.OWASP, SigstorePrivateKeyPemType), pass),
		shardsAnswer("cosign block", shardsEncrypted(t, der, pass, encrypted.Standard, CosignPrivateKeyPemType), pass),
		shardsAnswer("empty password", shardsEncrypted(t, der, nil, encrypted.Standard, SigstorePrivateKeyPemType), nil),
		shardsAnswer("wrong password", shardsEncrypted(t, der, pass, encrypted.Standard, SigstorePrivateKeyPemType), []byte("another")),
		shardsAnswer("plain block", pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: []byte("only its type is read")}), pass),
		shardsAnswer("no block", []byte("not a key\n"), pass),
		shardsAnswer("p224", shardsEncrypted(t, pkcs8(p224), pass, encrypted.Standard, SigstorePrivateKeyPemType), pass),
		shardsAnswer("rsa 1024", shardsEncrypted(t, pkcs8(rsa1024), pass, encrypted.Standard, SigstorePrivateKeyPemType), pass),
		shardsAnswer("not pkcs8", shardsEncrypted(t, []byte("not a key at all"), pass, encrypted.Standard, SigstorePrivateKeyPemType), pass),
	)
	// The JSON tampered with: each field read as Go reads it.
	enc, err := encrypted.Encrypt(der, pass)
	shardsMust(t, err)
	for _, c := range []struct{ name, from, to string }{
		{"weak scrypt", `"N":65536`, `"N":1024`},
		{"scrypt r", `"r":8`, `"r":9`},
		{"kdf name", `"name":"scrypt"`, `"name":"pbkdf2"`},
		{"cipher name", `"name":"nacl/secretbox"`, `"name":"aes"`},
		{"N a string", `"N":65536`, `"N":"65536"`},
		{"N a fraction", `"N":65536`, `"N":65536.5`},
		{"N in capitals", `"N":65536`, `"n":65536`},
		{"kdf a list", `"kdf":{`, `"kdf":[{`},
		{"not json", `{"kdf"`, `{kdf`},
	} {
		text := strings.Replace(string(enc), c.from, c.to, 1)
		if c.name == "kdf a list" {
			text = strings.Replace(text, `},"cipher"`, `}],"cipher"`, 1)
		}
		keys = append(keys, shardsAnswer(c.name, pem.EncodeToMemory(&pem.Block{Type: SigstorePrivateKeyPemType, Bytes: []byte(text)}), pass))
	}
	var data map[string]any
	shardsMust(t, json.Unmarshal(enc, &data))
	cipher := data["cipher"].(map[string]any)
	nonce, err := base64.StdEncoding.DecodeString(cipher["nonce"].(string))
	shardsMust(t, err)
	cipher["nonce"] = base64.StdEncoding.EncodeToString(nonce[:23])
	short, err := json.Marshal(data)
	shardsMust(t, err)
	keys = append(keys, shardsAnswer("short nonce", pem.EncodeToMemory(&pem.Block{Type: SigstorePrivateKeyPemType, Bytes: short}), pass))
	cipher["nonce"] = "not base64!"
	bad, err := json.Marshal(data)
	shardsMust(t, err)
	keys = append(keys, shardsAnswer("nonce not base64", pem.EncodeToMemory(&pem.Block{Type: SigstorePrivateKeyPemType, Bytes: bad}), pass))

	answers, err := json.MarshalIndent(map[string]any{"secretbox": boxes, "keys": keys}, "", " ")
	shardsMust(t, err)
	shardsMust(t, os.WriteFile(out, append(answers, '\n'), 0o644))
}
