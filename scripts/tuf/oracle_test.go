package main

// go-tuf v2's answers (as buildx v0.37.1 vendors it) for crates/tuf/tests/oracle.rs: TUF
// repositories built here with go-tuf's own metadata API, each fetched by go-tuf's
// updater from memory at a fixed time, and Sigstore's live repository as it was fetched
// once. `generate` copies this file into buildx's cmd/buildx and runs it there.

import (
	"bytes"
	"crypto"
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/secure-systems-lab/go-securesystemslib/cjson"
	"github.com/sigstore/sigstore/pkg/signature"
	"github.com/theupdateframework/go-tuf/v2/metadata"
	"github.com/theupdateframework/go-tuf/v2/metadata/config"
	"github.com/theupdateframework/go-tuf/v2/metadata/fetcher"
	"github.com/theupdateframework/go-tuf/v2/metadata/updater"
)

type tufCase struct {
	Name   string            `json:"name"`
	Base   string            `json:"base"`
	Root   string            `json:"root"`
	Files  map[string]string `json:"files"`
	Local  map[string]string `json:"local,omitempty"`
	Now    string            `json:"now"`
	Target string            `json:"target"`
	// go-tuf's answer.
	Error        string `json:"error"`
	TargetSHA256 string `json:"targetSha256,omitempty"`
	RootVersion  int64  `json:"rootVersion"`
}

// memFetcher serves a case's files, as DefaultFetcher does a repository's.
type memFetcher struct{ files map[string][]byte }

func (f *memFetcher) DownloadFile(url string, maxLength int64, _ time.Duration) ([]byte, error) {
	data, ok := f.files[url]
	if !ok {
		return nil, &metadata.ErrDownloadHTTP{StatusCode: 404, URL: url}
	}
	if int64(len(data)) > maxLength {
		return nil, &metadata.ErrDownloadLengthMismatch{Msg: fmt.Sprintf("download failed for %s, length %d is larger than expected %d", url, len(data), maxLength)}
	}
	return data, nil
}

// recorder fetches from the real repository and keeps what it got.
type recorder struct {
	inner fetcher.Fetcher
	files map[string][]byte
}

func (r *recorder) DownloadFile(url string, maxLength int64, d time.Duration) ([]byte, error) {
	data, err := r.inner.DownloadFile(url, maxLength, d)
	if err == nil {
		r.files[url] = data
	}
	return data, err
}

type key struct {
	signer signature.Signer
	meta   *metadata.Key
}

func newKey(t *testing.T, kind string) key {
	switch kind {
	case "ed25519":
		pub, priv, err := ed25519.GenerateKey(rand.Reader)
		must(t, err)
		s, err := signature.LoadED25519Signer(priv)
		must(t, err)
		k, err := metadata.KeyFromPublicKey(pub)
		must(t, err)
		return key{s, k}
	case "rsa":
		priv, err := rsa.GenerateKey(rand.Reader, 2048)
		must(t, err)
		s, err := signature.LoadRSAPSSSigner(priv, crypto.SHA256, &rsa.PSSOptions{Hash: crypto.SHA256})
		must(t, err)
		k, err := metadata.KeyFromPublicKey(&priv.PublicKey)
		must(t, err)
		return key{s, k}
	case "p384":
		priv, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
		must(t, err)
		s, err := signature.LoadECDSASigner(priv, crypto.SHA384)
		must(t, err)
		k, err := metadata.KeyFromPublicKey(&priv.PublicKey)
		must(t, err)
		k.Scheme = metadata.KeySchemeECDSA_SHA2_P384
		return key{s, k}
	default:
		priv, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		must(t, err)
		s, err := signature.LoadECDSASigner(priv, crypto.SHA256)
		must(t, err)
		k, err := metadata.KeyFromPublicKey(&priv.PublicKey)
		must(t, err)
		return key{s, k}
	}
}

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func id(t *testing.T, k key) string {
	i, err := k.meta.ID()
	must(t, err)
	return i
}

var epoch = time.Date(2030, 1, 1, 0, 0, 0, 0, time.UTC)

// repo: one repository's keys and current metadata, signed by the keys each role names.
type repo struct {
	t                                 *testing.T
	base                              string
	keys                              map[string][]key
	root                              *metadata.Metadata[metadata.RootType]
	timestamp                         *metadata.Metadata[metadata.TimestampType]
	snapshot                          *metadata.Metadata[metadata.SnapshotType]
	targets                           *metadata.Metadata[metadata.TargetsType]
	files                             map[string][]byte
	content                           map[string][]byte
	delegated                         map[string]*metadata.Metadata[metadata.TargetsType]
	delegatedKeys                     map[string][]key
	firstRoot                         []byte
	expiresRoot, expiresTs, expiresSn time.Time
}

func newRepo(t *testing.T, kind string, threshold int) *repo {
	r := &repo{
		t:             t,
		base:          "https://repo.invalid",
		keys:          map[string][]key{},
		files:         map[string][]byte{},
		content:       map[string][]byte{},
		delegated:     map[string]*metadata.Metadata[metadata.TargetsType]{},
		delegatedKeys: map[string][]key{},
	}
	r.root = metadata.Root(epoch.AddDate(1, 0, 0))
	r.root.Signed.ConsistentSnapshot = true
	for _, role := range []string{"root", "timestamp", "snapshot", "targets"} {
		n := 1
		if role == "root" {
			n = threshold
		}
		for i := 0; i < n; i++ {
			k := newKey(t, kind)
			r.keys[role] = append(r.keys[role], k)
			must(t, r.root.Signed.AddKey(k.meta, role))
		}
		r.root.Signed.Roles[role].Threshold = threshold
		if role != "root" {
			r.root.Signed.Roles[role].Threshold = 1
		}
	}
	r.timestamp = metadata.Timestamp(epoch.AddDate(0, 0, 7))
	r.snapshot = metadata.Snapshot(epoch.AddDate(0, 1, 0))
	r.targets = metadata.Targets(epoch.AddDate(0, 3, 0))
	return r
}

// sign signs as Metadata.Sign does, by each key's own ID (Sign would name an ECDSA key
// by the P-256 scheme KeyFromPublicKey gives it).
func sign[T metadata.Roles](t *testing.T, m *metadata.Metadata[T], ks []key) []byte {
	m.ClearSignatures()
	for _, k := range ks {
		payload, err := cjson.EncodeCanonical(m.Signed)
		must(t, err)
		sb, err := k.signer.SignMessage(bytes.NewReader(payload))
		must(t, err)
		m.Signatures = append(m.Signatures, metadata.Signature{KeyID: id(t, k), Signature: sb})
	}
	b, err := m.ToBytes(true)
	must(t, err)
	return b
}

// publish signs every role and serves it: the root at its version, the rest as the
// root's consistent snapshot names them.
func (r *repo) publish() {
	t := r.t
	rootBytes := sign(t, r.root, r.keys["root"])
	if r.firstRoot == nil {
		r.firstRoot = rootBytes
	}
	r.files[fmt.Sprintf("%s/%d.root.json", r.base, r.root.Signed.Version)] = rootBytes
	for name, d := range r.delegated {
		b := sign(t, d, r.delegatedKeys[name])
		r.files[fmt.Sprintf("%s/%d.%s.json", r.base, d.Signed.Version, name)] = b
		r.snapshot.Signed.Meta[name+".json"] = metadata.MetaFile(d.Signed.Version)
	}
	for path, data := range r.content {
		tf, err := metadata.TargetFile().FromBytes(path, data, "sha256")
		must(t, err)
		owner := r.targets
		if i := strings.Index(path, "/"); i > 0 {
			if d, ok := r.delegated[path[:i]]; ok {
				owner = d
			}
		}
		owner.Signed.Targets[path] = tf
		h := hex.EncodeToString(tf.Hashes["sha256"])
		dir, base := filepath.Split(path)
		r.files[fmt.Sprintf("%s/targets/%s%s.%s", r.base, dir, h, base)] = data
	}
	for name, d := range r.delegated {
		b := sign(t, d, r.delegatedKeys[name])
		r.files[fmt.Sprintf("%s/%d.%s.json", r.base, d.Signed.Version, name)] = b
	}
	targetsBytes := sign(t, r.targets, r.keys["targets"])
	r.files[fmt.Sprintf("%s/%d.targets.json", r.base, r.targets.Signed.Version)] = targetsBytes
	r.snapshot.Signed.Meta["targets.json"] = metadata.MetaFile(r.targets.Signed.Version)
	snapBytes := sign(t, r.snapshot, r.keys["snapshot"])
	r.files[fmt.Sprintf("%s/%d.snapshot.json", r.base, r.snapshot.Signed.Version)] = snapBytes
	r.timestamp.Signed.Meta["snapshot.json"] = metadata.MetaFile(r.snapshot.Signed.Version)
	h := sha256.Sum256(snapBytes)
	r.timestamp.Signed.Meta["snapshot.json"].Hashes = metadata.Hashes{"sha256": metadata.HexBytes(h[:])}
	r.timestamp.Signed.Meta["snapshot.json"].Length = int64(len(snapBytes))
	r.files[r.base+"/timestamp.json"] = sign(t, r.timestamp, r.keys["timestamp"])
}

func (r *repo) toCase(name, target string, now time.Time) tufCase {
	c := tufCase{Name: name, Base: r.base, Root: base64.StdEncoding.EncodeToString(r.firstRoot), Files: map[string]string{}, Now: now.Format(time.RFC3339Nano), Target: target}
	for k, v := range r.files {
		c.Files[k] = base64.StdEncoding.EncodeToString(v)
	}
	return c
}

func runTUF(c *tufCase) {
	root, _ := base64.StdEncoding.DecodeString(c.Root)
	files := map[string][]byte{}
	for k, v := range c.Files {
		files[k], _ = base64.StdEncoding.DecodeString(v)
	}
	dir, err := os.MkdirTemp("", "tuf-oracle")
	if err != nil {
		c.Error = err.Error()
		return
	}
	defer os.RemoveAll(dir)
	for k, v := range c.Local {
		b, _ := base64.StdEncoding.DecodeString(v)
		_ = os.WriteFile(filepath.Join(dir, k), b, 0o644)
	}
	now, _ := time.Parse(time.RFC3339Nano, c.Now)
	cfg, err := config.New(c.Base, root)
	if err != nil {
		c.Error = err.Error()
		return
	}
	cfg.LocalMetadataDir = dir
	cfg.LocalTargetsDir = filepath.Join(dir, "targets")
	cfg.Fetcher = &memFetcher{files}
	up, err := updater.New(cfg)
	if err != nil {
		c.Error = err.Error()
		return
	}
	up.UnsafeSetRefTime(now)
	defer func() {
		ts := up.GetTrustedMetadataSet()
		c.RootVersion = ts.Root.Signed.Version
	}()
	if err := up.Refresh(); err != nil {
		c.Error = err.Error()
		return
	}
	ti, err := up.GetTargetInfo(c.Target)
	if err != nil {
		c.Error = err.Error()
		return
	}
	_, data, err := up.DownloadTarget(ti, "", "")
	if err != nil {
		c.Error = err.Error()
		return
	}
	h := sha256.Sum256(data)
	c.TargetSHA256 = hex.EncodeToString(h[:])
}

func cases(t *testing.T) []tufCase {
	var out []tufCase
	now := epoch.Add(time.Hour)
	add := func(c tufCase) { out = append(out, c) }

	for _, kind := range []string{"p256", "p384", "ed25519", "rsa"} {
		r := newRepo(t, kind, 1)
		r.content["file.txt"] = []byte("hello " + kind)
		r.publish()
		add(r.toCase("valid "+kind, "file.txt", now))
		add(r.toCase("missing target "+kind, "nope.txt", now))
	}

	// Expiry of each role.
	r := newRepo(t, "p256", 1)
	r.content["f"] = []byte("f")
	r.publish()
	add(r.toCase("timestamp expired", "f", epoch.AddDate(0, 0, 8)))
	add(r.toCase("snapshot expired", "f", epoch.AddDate(0, 1, 1)))
	add(r.toCase("targets expired", "f", epoch.AddDate(0, 3, 1)))
	add(r.toCase("root expired", "f", epoch.AddDate(1, 0, 1)))
	add(r.toCase("at the expiry instant", "f", epoch.AddDate(0, 0, 7)))

	// Root rotation: v2 with a new root key, signed by old and new; v3 rotating again.
	r = newRepo(t, "p256", 1)
	r.content["f"] = []byte("rotated")
	r.publish()
	old := r.keys["root"]
	nk := newKey(t, "p256")
	must(t, r.root.Signed.RevokeKey(id(t, old[0]), "root"))
	must(t, r.root.Signed.AddKey(nk.meta, "root"))
	r.root.Signed.Version = 2
	r.keys["root"] = append(old, nk)
	b := sign(t, r.root, r.keys["root"])
	r.files[r.base+"/2.root.json"] = b
	r.keys["root"] = []key{nk}
	nk3 := newKey(t, "ed25519")
	must(t, r.root.Signed.RevokeKey(id(t, nk), "root"))
	must(t, r.root.Signed.AddKey(nk3.meta, "root"))
	r.root.Signed.Version = 3
	// Re-signed as published: v3 by v2's key and its own.
	r.keys["root"] = []key{nk, nk3}
	r.timestamp.Signed.Version = 2
	r.publish()
	add(r.toCase("root rotated twice", "f", now))
	c := r.toCase("root rotation not signed by the old root", "f", now)
	r.root.Signed.Version = 4
	nk4 := newKey(t, "p256")
	must(t, r.root.Signed.RevokeKey(id(t, nk3), "root"))
	must(t, r.root.Signed.AddKey(nk4.meta, "root"))
	c.Files[r.base+"/4.root.json"] = base64.StdEncoding.EncodeToString(sign(t, r.root, []key{nk4}))
	add(c)
	c = r.toCase("root version skipped", "f", now)
	delete(c.Files, r.base+"/2.root.json")
	c.Files[r.base+"/2.root.json"] = c.Files[r.base+"/3.root.json"]
	delete(c.Files, r.base+"/3.root.json")
	add(c)

	// A root that skips a version, signed by the trusted root's own key.
	r = newRepo(t, "p256", 1)
	r.content["f"] = []byte("skip")
	r.publish()
	r.root.Signed.Version = 3
	c = r.toCase("root version 3 served as 2", "f", now)
	c.Files[r.base+"/2.root.json"] = base64.StdEncoding.EncodeToString(sign(t, r.root, r.keys["root"]))
	add(c)

	// Thresholds: two root keys required; one key under two IDs counts once.
	r = newRepo(t, "p256", 2)
	r.content["f"] = []byte("t")
	r.publish()
	add(r.toCase("threshold two met", "f", now))
	r2 := newRepo(t, "p256", 1)
	r2.content["f"] = []byte("dup")
	k := r2.keys["root"][0]
	// The same key, under an ID of its own (its description differs).
	dupKey := metadata.Key{Type: k.meta.Type, Scheme: k.meta.Scheme, Value: k.meta.Value, UnrecognizedFields: map[string]any{"x-dup": "1"}}
	dupID, err := dupKey.ID()
	must(t, err)
	r2.root.Signed.Keys[dupID] = &dupKey
	r2.root.Signed.Roles["root"].KeyIDs = append(r2.root.Signed.Roles["root"].KeyIDs, dupID)
	r2.root.Signed.Roles["root"].Threshold = 2
	rb := sign(t, r2.root, []key{k})
	must(t, err)
	var m map[string]any
	must(t, json.Unmarshal(rb, &m))
	sigs := m["signatures"].([]any)
	s0 := sigs[0].(map[string]any)
	m["signatures"] = append(sigs, map[string]any{"keyid": dupID, "sig": s0["sig"]})
	rb, err = json.Marshal(m)
	must(t, err)
	c = tufCase{Name: "one key under two IDs counts once", Base: r2.base, Root: base64.StdEncoding.EncodeToString(rb), Files: map[string]string{}, Now: now.Format(time.RFC3339Nano), Target: "f"}
	add(c)
	m["signatures"] = append(m["signatures"].([]any), map[string]any{"keyid": dupID, "sig": s0["sig"]})
	rb, err = json.Marshal(m)
	must(t, err)
	add(tufCase{Name: "two signatures by one key ID", Base: r2.base, Root: base64.StdEncoding.EncodeToString(rb), Files: map[string]string{}, Now: now.Format(time.RFC3339Nano), Target: "f"})

	// Tampering: a snapshot not matching the timestamp's hash; a target not matching.
	r = newRepo(t, "p256", 1)
	r.content["f"] = []byte("real")
	r.publish()
	c = r.toCase("snapshot hash mismatch", "f", now)
	for k := range c.Files {
		if strings.HasSuffix(k, ".snapshot.json") {
			b, _ := base64.StdEncoding.DecodeString(c.Files[k])
			b = append(b, ' ')
			c.Files[k] = base64.StdEncoding.EncodeToString(b)
		}
	}
	add(c)
	c = r.toCase("target hash mismatch", "f", now)
	for k := range c.Files {
		if strings.Contains(k, "/targets/") {
			c.Files[k] = base64.StdEncoding.EncodeToString([]byte("evil"))
		}
	}
	add(c)
	c = r.toCase("timestamp too long", "f", now)
	tb, _ := base64.StdEncoding.DecodeString(c.Files[r.base+"/timestamp.json"])
	tb = append(tb, bytes.Repeat([]byte(" "), 20000)...)
	c.Files[r.base+"/timestamp.json"] = base64.StdEncoding.EncodeToString(tb)
	add(c)

	// Rollback: a cached timestamp newer than the repository's.
	r = newRepo(t, "p256", 1)
	r.content["f"] = []byte("x")
	r.timestamp.Signed.Version = 5
	r.publish()
	newer := r.files[r.base+"/timestamp.json"]
	r.timestamp.Signed.Version = 3
	r.publish()
	c = r.toCase("timestamp rollback", "f", now)
	c.Local = map[string]string{"timestamp.json": base64.StdEncoding.EncodeToString(newer)}
	add(c)
	c = r.toCase("cached timestamp equal", "f", now)
	c.Local = map[string]string{"timestamp.json": c.Files[r.base+"/timestamp.json"]}
	add(c)

	// Not consistent snapshots: unversioned names.
	r = newRepo(t, "ed25519", 1)
	r.root.Signed.ConsistentSnapshot = false
	r.content["f"] = []byte("plain")
	r.publish()
	c = r.toCase("not consistent", "f", now)
	plain := map[string]string{}
	for k, v := range c.Files {
		switch {
		case strings.HasSuffix(k, ".snapshot.json"):
			plain[r.base+"/snapshot.json"] = v
		case strings.HasSuffix(k, ".targets.json"):
			plain[r.base+"/targets.json"] = v
		case strings.Contains(k, "/targets/"):
			plain[r.base+"/targets/f"] = v
		default:
			plain[k] = v
		}
	}
	c.Files = plain
	add(c)

	// Delegations: by path glob, by hash prefix, terminating, and succinct hash bins.
	r = newRepo(t, "p256", 1)
	dk := newKey(t, "ed25519")
	dkID := id(t, dk)
	r.targets.Signed.Delegations = &metadata.Delegations{
		Keys: map[string]*metadata.Key{dkID: dk.meta},
		Roles: []metadata.DelegatedRole{
			{Name: "skip", KeyIDs: []string{dkID}, Threshold: 1, Paths: []string{"other/*"}},
			{Name: "lib", KeyIDs: []string{dkID}, Threshold: 1, Paths: []string{"lib/*.so"}, Terminating: true},
			{Name: "never", KeyIDs: []string{dkID}, Threshold: 1, Paths: []string{"lib/*"}},
		},
	}
	for _, n := range []string{"skip", "lib", "never"} {
		r.delegated[n] = metadata.Targets(epoch.AddDate(0, 3, 0))
		r.delegatedKeys[n] = []key{dk}
	}
	r.content["lib/a.so"] = []byte("so")
	r.publish()
	add(r.toCase("delegated by path", "lib/a.so", now))
	add(r.toCase("delegated, terminating, not found", "lib/b.txt", now))

	r = newRepo(t, "p256", 1)
	r.targets.Signed.Delegations = &metadata.Delegations{
		Keys: map[string]*metadata.Key{dkID: dk.meta},
		SuccinctRoles: &metadata.SuccinctRoles{
			KeyIDs: []string{dkID}, Threshold: 1, BitLength: 4, NamePrefix: "bin",
		},
	}
	path := "pkg/x"
	roles := r.targets.Signed.Delegations.SuccinctRoles.GetRolesForTarget(path)
	r.delegated[roles[0].Name] = metadata.Targets(epoch.AddDate(0, 3, 0))
	r.delegatedKeys[roles[0].Name] = []key{dk}
	r.publish()
	d := r.delegated[roles[0].Name]
	tf, err := metadata.TargetFile().FromBytes(path, []byte("binned"), "sha256")
	must(t, err)
	d.Signed.Targets[path] = tf
	h := hex.EncodeToString(tf.Hashes["sha256"])
	r.files[fmt.Sprintf("%s/targets/pkg/%s.x", r.base, h)] = []byte("binned")
	r.publish()
	add(r.toCase("succinct hash bins", path, now))

	// Fields go-tuf keeps: unrecognized ones, a name in another case, and numbers.
	for _, extra := range []struct {
		name string
		edit func(map[string]any)
	}{
		{"unrecognized fields", func(s map[string]any) { s["x-note"] = map[string]any{"b": []any{1, "two", true, nil}, "a": "é<&>"} }},
		{"a field name in another case", func(s map[string]any) { s["Version"] = 1 }},
		{"an integer written as 1e3", func(s map[string]any) { s["x-n"] = json.Number("1e3") }},
		{"a fractional number", func(s map[string]any) { s["x-n"] = json.Number("1.5") }},
		{"expires with a fraction and an offset", func(s map[string]any) { s["expires"] = "2031-01-01T00:00:00.500+02:00" }},
		{"no consistent_snapshot", func(s map[string]any) { delete(s, "consistent_snapshot") }},
	} {
		r = newRepo(t, "p256", 1)
		r.content["f"] = []byte("extra")
		r.publish()
		var m map[string]any
		dec := json.NewDecoder(bytes.NewReader(r.firstRoot))
		dec.UseNumber()
		must(t, dec.Decode(&m))
		extra.edit(m["signed"].(map[string]any))
		raw, err := json.Marshal(m)
		must(t, err)
		root, err := metadata.Root().FromBytes(raw)
		if err != nil {
			add(tufCase{Name: extra.name, Base: r.base, Root: base64.StdEncoding.EncodeToString(raw), Files: map[string]string{}, Now: now.Format(time.RFC3339Nano), Target: "f"})
			continue
		}
		var signed []byte
		root.ClearSignatures()
		payload, serr := cjson.EncodeCanonical(root.Signed)
		if serr == nil {
			sb, err := r.keys["root"][0].signer.SignMessage(bytes.NewReader(payload))
			must(t, err)
			root.Signatures = append(root.Signatures, metadata.Signature{KeyID: id(t, r.keys["root"][0]), Signature: sb})
			// Its signature as go-tuf makes it, over go-tuf's re-encoding; the text served is
			// the edited one.
			var mm map[string]any
			d2 := json.NewDecoder(bytes.NewReader(raw))
			d2.UseNumber()
			must(t, d2.Decode(&mm))
			sb, _ = json.Marshal(root.Signatures)
			var sigs []any
			must(t, json.Unmarshal(sb, &sigs))
			mm["signatures"] = sigs
			signed, err = json.Marshal(mm)
			must(t, err)
		} else {
			signed = raw
		}
		c := r.toCase(extra.name, "f", now)
		c.Root = base64.StdEncoding.EncodeToString(signed)
		add(c)
	}
	return out
}

// live: Sigstore's repository as fetched now, from the root the policy helpers carry.
func live(t *testing.T, root []byte) tufCase {
	rec := &recorder{inner: fetcher.NewDefaultFetcher(), files: map[string][]byte{}}
	dir := t.TempDir()
	base := "https://tuf-repo-cdn.sigstore.dev"
	cfg, err := config.New(base, root)
	must(t, err)
	cfg.LocalMetadataDir = dir
	cfg.LocalTargetsDir = filepath.Join(dir, "targets")
	cfg.Fetcher = rec
	up, err := updater.New(cfg)
	must(t, err)
	must(t, up.Refresh())
	ti, err := up.GetTargetInfo("trusted_root.json")
	must(t, err)
	_, _, err = up.DownloadTarget(ti, "", "")
	must(t, err)
	c := tufCase{Name: "sigstore live", Base: base, Root: base64.StdEncoding.EncodeToString(root), Files: map[string]string{}, Now: time.Now().UTC().Format(time.RFC3339Nano), Target: "trusted_root.json"}
	for k, v := range rec.files {
		c.Files[k] = base64.StdEncoding.EncodeToString(v)
	}
	return c
}

func TestShardsTUFOracle(t *testing.T) {
	out := os.Getenv("SHARDS_TUF_OUT")
	if out == "" {
		t.Skip("SHARDS_TUF_OUT names the file to write")
	}
	all := cases(t)
	root, err := os.ReadFile(os.Getenv("SHARDS_TUF_ROOT"))
	must(t, err)
	all = append(all, live(t, root))
	for i := range all {
		runTUF(&all[i])
	}
	b, err := json.MarshalIndent(all, "", " ")
	must(t, err)
	must(t, os.WriteFile(out, append(b, '\n'), 0o644))
}
