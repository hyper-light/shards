package zzshardssigstore

// BuildKit's policy helpers' answers (moby/policy-helpers as buildx v0.37.1 vendors it)
// for crates/sigstore/tests/image.rs: images whose provenance attestations are signed
// against oracle_test.go's virtual Sigstore (a Sigstore bundle, cosign's simple signing,
// or a Docker Hardened Image's key), each signature chain resolved by the vendored
// image.ResolveSignatureChain and verified by a verbatim copy of Verifier.VerifyImage's
// body (verifier.go) and of hashedrecordbundle.go, with the trusted root and the DHI key
// given rather than fetched by TUF and carried in the binary. The provider is buildx's
// acProvider (policy/signatures.go) for a BuildKit image, and a plain map honoring
// artifact types for a DHI one; every referrers call and its answer is recorded, so the
// harness answers the same. Also containerd/platforms' Only, Normalize and FormatAll
// over a grid of platforms for crates/sigstore/tests/platforms.rs.
//
// `generate-image` copies this file and oracle_test.go into buildx's
// cmd/zz_shards_image and runs it there, with TZ=UTC.

import (
	"bytes"
	"context"
	"crypto/elliptic"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"maps"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"testing"
	"testing/cryptotest"
	"time"

	"github.com/containerd/containerd/v2/core/content"
	"github.com/containerd/containerd/v2/core/remotes"
	cerrdefs "github.com/containerd/errdefs"
	"github.com/containerd/platforms"
	slsa02 "github.com/in-toto/in-toto-golang/in_toto/slsa_provenance/v0.2"
	slsa1 "github.com/in-toto/in-toto-golang/in_toto/slsa_provenance/v1"
	policyimage "github.com/moby/policy-helpers/image"
	"github.com/moby/policy-helpers/types"
	digest "github.com/opencontainers/go-digest"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
	"github.com/pkg/errors"
	protobundle "github.com/sigstore/protobuf-specs/gen/pb-go/bundle/v1"
	v1common "github.com/sigstore/protobuf-specs/gen/pb-go/common/v1"
	v1 "github.com/sigstore/protobuf-specs/gen/pb-go/rekor/v1"
	prototrustroot "github.com/sigstore/protobuf-specs/gen/pb-go/trustroot/v1"
	"github.com/sigstore/sigstore-go/pkg/bundle"
	"github.com/sigstore/sigstore-go/pkg/fulcio/certificate"
	"github.com/sigstore/sigstore-go/pkg/root"
	"github.com/sigstore/sigstore-go/pkg/tlog"
	"github.com/sigstore/sigstore-go/pkg/verify"
	"github.com/sigstore/sigstore/pkg/signature"
)

// ---------------------------------------------------------------- VerifyImage, verbatim

// dhiKey stands in for roots/dhi's carried key and dhiEpoch.
type dhiKey struct {
	pub       any
	validFrom int64
}

// verifyImage is Verifier.VerifyImage with the trust provider's root (fulcioRoot) and
// the DHI root (dhi.TrustedRoot of it, with dk's key) given.
func verifyImage(ctx context.Context, provider policyimage.ReferrersProvider, desc ocispecs.Descriptor, platform *ocispecs.Platform, fulcioRoot root.TrustedMaterial, dk dhiKey) (*types.SignatureInfo, error) {
	sc, err := policyimage.ResolveSignatureChain(ctx, provider, desc, platform)
	if err != nil {
		return nil, errors.Wrapf(err, "resolving signature chain for image %s", desc.Digest)
	}

	if sc.AttestationManifest == nil || sc.SignatureManifest == nil {
		return nil, errors.WithStack(&noSigChainError{
			Target:         desc.Digest,
			HasAttestation: sc.AttestationManifest != nil,
		})
	}

	attestationBytes, err := sc.ManifestBytes(ctx, sc.AttestationManifest)
	if err != nil {
		return nil, errors.Wrapf(err, "reading attestation manifest %s", sc.AttestationManifest.Digest)
	}

	var attestation ocispecs.Manifest
	if err := json.Unmarshal(attestationBytes, &attestation); err != nil {
		return nil, errors.Wrapf(err, "unmarshaling attestation manifest %s", sc.AttestationManifest.Digest)
	}

	if attestation.Subject == nil {
		return nil, errors.Errorf("attestation manifest %s has no subject", sc.AttestationManifest.Digest)
	}
	if attestation.Subject.Digest != sc.ImageManifest.Digest {
		return nil, errors.Errorf("attestation manifest %s subject digest %s does not match image manifest digest %s", sc.AttestationManifest.Digest, attestation.Subject.Digest, sc.ImageManifest.Digest)
	}
	if attestation.Subject.MediaType != ocispecs.MediaTypeImageManifest && attestation.Subject.MediaType != ocispecs.MediaTypeImageIndex {
		return nil, errors.Errorf("attestation manifest %s subject media type %s is not an image manifest or index", sc.AttestationManifest.Digest, attestation.Subject.MediaType)
	}
	if attestation.Subject.Size != sc.ImageManifest.Size {
		return nil, errors.Errorf("attestation manifest %s subject size %d does not match image manifest size %d", sc.AttestationManifest.Digest, attestation.Subject.Size, sc.ImageManifest.Size)
	}
	hasSLSA := false
	for _, l := range attestation.Layers {
		if isSLSAPredicateType(l.Annotations["in-toto.io/predicate-type"]) {
			hasSLSA = true
			break
		}
	}
	if !hasSLSA {
		return nil, errors.Errorf("attestation manifest %s has no SLSA provenance layer", sc.AttestationManifest.Digest)
	}

	anyCert, err := anyCerificateIdentity()
	if err != nil {
		return nil, errors.WithStack(err)
	}
	var artifactPolicy verify.ArtifactPolicyOption

	var trustedRoot root.TrustedMaterial

	sigBytes, err := sc.ManifestBytes(ctx, sc.SignatureManifest)
	if err != nil {
		return nil, errors.Wrapf(err, "reading signature manifest %s", sc.SignatureManifest.Digest)
	}

	var mfst ocispecs.Manifest
	if err := json.Unmarshal(sigBytes, &mfst); err != nil {
		return nil, errors.Wrapf(err, "unmarshaling signature manifest %s", sc.SignatureManifest.Digest)
	}

	// basic validations
	if mfst.Subject == nil {
		return nil, errors.Errorf("signature manifest %s has no subject", sc.SignatureManifest.Digest)
	}
	if mfst.Subject.Digest != sc.AttestationManifest.Digest {
		return nil, errors.Errorf("signature manifest %s subject digest %s does not match attestation manifest digest %s", sc.SignatureManifest.Digest, mfst.Subject.Digest, sc.AttestationManifest.Digest)
	}
	if mfst.Subject.MediaType != ocispecs.MediaTypeImageManifest && mfst.Subject.MediaType != ocispecs.MediaTypeImageIndex {
		return nil, errors.Errorf("signature manifest %s subject media type %s is not an image manifest or index", sc.SignatureManifest.Digest, mfst.Subject.MediaType)
	}
	if mfst.Subject.Size != sc.AttestationManifest.Size {
		return nil, errors.Errorf("signature manifest %s subject size %d does not match attestation manifest size %d", sc.SignatureManifest.Digest, mfst.Subject.Size, sc.AttestationManifest.Size)
	}
	if len(mfst.Layers) == 0 {
		return nil, errors.Errorf("signature manifest %s has %d layers, expected 1", sc.SignatureManifest.Digest, len(mfst.Layers))
	}
	layer := mfst.Layers[0]

	var dockerReference string

	var se verify.SignedEntity
	sigType := types.SignatureBundleV03
	switch layer.MediaType {
	case policyimage.ArtifactTypeSigstoreBundle:
		if mfst.ArtifactType != policyimage.ArtifactTypeSigstoreBundle {
			return nil, errors.Errorf("signature manifest %s is not a bundle (artifact type %q)", sc.SignatureManifest.Digest, mfst.ArtifactType)
		}
		bundleBytes, err := policyimage.ReadBlob(ctx, sc.Provider, layer)
		if err != nil {
			return nil, errors.Wrapf(err, "reading bundle layer %s from signature manifest %s", layer.Digest, sc.SignatureManifest.Digest)
		}
		b, err := loadBundle(bundleBytes)
		if err != nil {
			return nil, errors.Wrapf(err, "loading signature bundle from manifest %s", sc.SignatureManifest.Digest)
		}
		se = b

		alg, rawDgst, err := rawDigest(sc.AttestationManifest.Digest)
		if err != nil {
			return nil, errors.WithStack(err)
		}
		artifactPolicy = verify.WithArtifactDigest(alg, rawDgst)
	case policyimage.MediaTypeCosignSimpleSigning:
		sigType = types.SignatureSimpleSigningV1
		payloadBytes, err := policyimage.ReadBlob(ctx, sc.Provider, layer)
		if err != nil {
			return nil, errors.Wrapf(err, "reading bundle layer %s from signature manifest %s", layer.Digest, sc.SignatureManifest.Digest)
		}
		var payload struct {
			Critical struct {
				Identity struct {
					DockerReference string `json:"docker-reference"`
				} `json:"identity"`
				Image struct {
					DockerManifestDigest string `json:"docker-manifest-digest"`
				} `json:"image"`
				Type string `json:"type"`
			} `json:"critical"`
			Optional map[string]any `json:"optional"`
		}
		if err := json.Unmarshal(payloadBytes, &payload); err != nil {
			return nil, errors.Wrapf(err, "unmarshaling simple signing payload from manifest %s", sc.SignatureManifest.Digest)
		}
		if payload.Critical.Image.DockerManifestDigest != sc.AttestationManifest.Digest.String() {
			return nil, errors.Errorf("simple signing payload in manifest %s has docker-manifest-digest %s which does not match attestation manifest digest %s", sc.SignatureManifest.Digest, payload.Critical.Image.DockerManifestDigest, sc.AttestationManifest.Digest)
		}
		if payload.Critical.Type != "cosign container image signature" {
			return nil, errors.Errorf("simple signing payload in manifest %s has invalid type %q", sc.SignatureManifest.Digest, payload.Critical.Type)
		}
		dockerReference = payload.Critical.Identity.DockerReference

		hrse, err := newHashedRecordSignedEntity(&mfst, sc.DHI)
		if err != nil {
			return nil, errors.Wrapf(err, "loading hashed record signed entity from manifest %s", sc.SignatureManifest.Digest)
		}
		se = hrse
		alg, rawDgst, err := rawDigest(layer.Digest)
		if err != nil {
			return nil, errors.WithStack(err)
		}
		artifactPolicy = verify.WithArtifactDigest(alg, rawDgst)
	default:
		return nil, errors.Errorf("signature manifest %s layer has invalid media type %s", sc.SignatureManifest.Digest, layer.MediaType)
	}

	verifierOpts := []verify.VerifierOption{}

	if sc.DHI {
		// dhi.TrustedRoot(fulcioRoot), its key the case's.
		v, err := signature.LoadVerifierWithOpts(dk.pub)
		if err != nil {
			return nil, errors.Wrap(errors.Wrap(err, "loading DHI public key verifier"), "getting DHI trust root")
		}
		trustedRoot = &dhiMaterial{v: v, validFrom: dk.validFrom, fulcio: fulcioRoot}
		// DHI signature may or may not have transparency data
		// validation needs to be done in a later additional policy step
		if _, hasBundleAnnotation := layer.Annotations["dev.sigstore.cosign/bundle"]; !hasBundleAnnotation {
			verifierOpts = append(verifierOpts, verify.WithNoObserverTimestamps())
		} else {
			verifierOpts = append(verifierOpts,
				verify.WithObserverTimestamps(1),
				verify.WithTransparencyLog(1),
			)
		}
		// signed with pubkey without cert identity
		anyCert = verify.WithoutIdentitiesUnsafe()
	} else {
		trustedRoot = fulcioRoot
		verifierOpts = append(verifierOpts,
			verify.WithObserverTimestamps(1),
			verify.WithTransparencyLog(1),
			verify.WithSignedCertificateTimestamps(1),
		)
	}
	gv, err := verify.NewVerifier(trustedRoot, verifierOpts...)
	if err != nil {
		return nil, errors.Wrap(err, "creating verifier")
	}

	policy := verify.NewPolicy(artifactPolicy, anyCert)

	result, err := gv.Verify(se, policy)
	if err != nil {
		return nil, errors.Wrap(err, "verifying bundle")
	}

	if result.Signature == nil || (result.Signature.Certificate == nil && !sc.DHI) {
		return nil, errors.Errorf("no valid signatures found")
	}

	si := &types.SignatureInfo{
		Signer:          result.Signature.Certificate,
		Timestamps:      toTimestamps(result.VerifiedTimestamps),
		DockerReference: dockerReference,
		IsDHI:           sc.DHI,
		SignatureType:   sigType,
	}
	si.Kind = si.DetectKind()
	return si, nil
}

// errors.go's NoSigChainError.
type noSigChainError struct {
	Target         digest.Digest
	HasAttestation bool
}

func (e *noSigChainError) Error() string {
	if e.HasAttestation {
		return fmt.Sprintf("no signature found for image %s", e.Target)
	}
	return fmt.Sprintf("no provenance attestation found for image %s", e.Target)
}

func anyCerificateIdentity() (verify.PolicyOption, error) {
	sanMatcher, err := verify.NewSANMatcher("", ".*")
	if err != nil {
		return nil, err
	}
	issuerMatcher, err := verify.NewIssuerMatcher("", ".*")
	if err != nil {
		return nil, err
	}
	extensions := certificate.Extensions{}
	certID, err := verify.NewCertificateIdentity(sanMatcher, issuerMatcher, extensions)
	if err != nil {
		return nil, err
	}
	return verify.WithCertificateIdentity(certID), nil
}

func loadBundle(dt []byte) (*bundle.Bundle, error) {
	var bundle bundle.Bundle
	bundle.Bundle = new(protobundle.Bundle)
	err := bundle.UnmarshalJSON(dt)
	if err != nil {
		return nil, err
	}
	return &bundle, nil
}

func rawDigest(d digest.Digest) (string, []byte, error) {
	alg := d.Algorithm().String()
	b, err := hex.DecodeString(d.Encoded())
	if err != nil {
		return "", nil, errors.Wrapf(err, "decoding digest %s", d)
	}
	return alg, b, nil
}

func isSLSAPredicateType(v string) bool {
	switch v {
	case slsa1.PredicateSLSAProvenance, slsa02.PredicateSLSAProvenance:
		return true
	default:
		return false
	}
}

func toTimestamps(ts []verify.TimestampVerificationResult) []types.TimestampVerificationResult {
	tsout := make([]types.TimestampVerificationResult, len(ts))
	for i, t := range ts {
		tsout[i] = types.TimestampVerificationResult{
			Type:      t.Type,
			URI:       t.URI,
			Timestamp: t.Timestamp,
		}
	}
	return tsout
}

// ---------------------------------------------------------------- providers

// blob: a descriptor and its content, as BuildKit's AttestationChain gives them.
type blob struct {
	desc ocispecs.Descriptor
	data []byte
}

// acProvider is buildx's (policy/signatures.go).
type acProvider struct {
	blobs      map[string]blob
	signatures []digest.Digest
	att        digest.Digest
}

func (p *acProvider) FetchReferrers(ctx context.Context, dgst digest.Digest, opts ...remotes.FetchReferrersOpt) ([]ocispecs.Descriptor, error) {
	if dgst != p.att {
		return nil, nil
	}
	out := make([]ocispecs.Descriptor, 0, len(p.signatures))
	for _, d := range p.signatures {
		b, ok := p.blobs[d.String()]
		if !ok {
			continue
		}
		desc := ocispecs.Descriptor{MediaType: b.desc.MediaType, Digest: b.desc.Digest, Size: b.desc.Size}

		var mfst ocispecs.Manifest
		if err := json.Unmarshal(b.data, &mfst); err != nil {
			return nil, errors.Wrapf(err, "unmarshal signature manifest %s", d)
		}
		desc.ArtifactType = mfst.ArtifactType

		// on image manifest assume legacy format
		if desc.ArtifactType == "" {
			desc.ArtifactType = policyimage.ArtifactTypeCosignSignature
		}
		out = append(out, desc)
	}
	return out, nil
}

func (p *acProvider) ReaderAt(ctx context.Context, desc ocispecs.Descriptor) (content.ReaderAt, error) {
	b, ok := p.blobs[desc.Digest.String()]
	if !ok {
		return nil, errors.WithStack(cerrdefs.ErrNotFound)
	}
	return &readerAt{buf: bytes.NewReader(b.data)}, nil
}

type readerAt struct {
	buf *bytes.Reader
}

func (r *readerAt) ReadAt(p []byte, off int64) (n int, err error) { return r.buf.ReadAt(p, off) }
func (r *readerAt) Size() int64                                   { return int64(r.buf.Len()) }
func (r *readerAt) Close() error                                  { return nil }

// mapProvider serves a registry's referrers as containerd's fetcher does: filtered by
// artifact type.
type mapProvider struct {
	blobs     map[string]blob
	referrers map[string][]ocispecs.Descriptor
	anyType   bool // answers every referrer, its artifact type not filtered
}

func (p *mapProvider) FetchReferrers(ctx context.Context, dgst digest.Digest, opts ...remotes.FetchReferrersOpt) ([]ocispecs.Descriptor, error) {
	var cfg remotes.FetchReferrersConfig
	for _, o := range opts {
		_ = o(ctx, &cfg)
	}
	all := p.referrers[dgst.String()]
	if len(cfg.ArtifactTypes) == 0 || p.anyType {
		return all, nil
	}
	var out []ocispecs.Descriptor
	for _, d := range all {
		if slices.Contains(cfg.ArtifactTypes, d.ArtifactType) {
			out = append(out, d)
		}
	}
	return out, nil
}

func (p *mapProvider) ReaderAt(ctx context.Context, desc ocispecs.Descriptor) (content.ReaderAt, error) {
	b, ok := p.blobs[desc.Digest.String()]
	if !ok {
		return nil, errors.WithStack(cerrdefs.ErrNotFound)
	}
	return &readerAt{buf: bytes.NewReader(b.data)}, nil
}

// recorder records every referrers call and its answer.
type recorder struct {
	policyimage.ReferrersProvider
	calls []imRefCall
}

func (r *recorder) FetchReferrers(ctx context.Context, dgst digest.Digest, opts ...remotes.FetchReferrersOpt) ([]ocispecs.Descriptor, error) {
	var cfg remotes.FetchReferrersConfig
	for _, o := range opts {
		_ = o(ctx, &cfg)
	}
	call := imRefCall{Digest: dgst.String(), ArtifactTypes: cfg.ArtifactTypes, Filters: [][2]string{}}
	if call.ArtifactTypes == nil {
		call.ArtifactTypes = []string{}
	}
	keys := slices.Sorted(maps.Keys(cfg.QueryFilters))
	for _, k := range keys {
		for _, v := range cfg.QueryFilters[k] {
			call.Filters = append(call.Filters, [2]string{k, v})
		}
	}
	res, err := r.ReferrersProvider.FetchReferrers(ctx, dgst, opts...)
	if err != nil {
		call.Error = err.Error()
	} else {
		call.Result = []json.RawMessage{}
		for _, d := range res {
			b, err := json.Marshal(d)
			if err != nil {
				panic(err)
			}
			call.Result = append(call.Result, b)
		}
	}
	r.calls = append(r.calls, call)
	return res, err
}

// ---------------------------------------------------------------- the case file

type imPlatform struct {
	OS         string   `json:"os"`
	Arch       string   `json:"architecture"`
	Variant    string   `json:"variant"`
	OSVersion  string   `json:"osVersion"`
	OSFeatures []string `json:"osFeatures"`
}

type imRefCall struct {
	Digest        string            `json:"digest"`
	ArtifactTypes []string          `json:"artifactTypes"`
	Filters       [][2]string       `json:"filters"`
	Result        []json.RawMessage `json:"result"`
	Error         string            `json:"error"`
}

type imResult struct {
	Kind            string            `json:"kind"`
	SignatureType   string            `json:"signatureType"`
	Signer          map[string]string `json:"signer"`
	Timestamps      []ocTimestamp     `json:"timestamps"`
	DockerReference string            `json:"dockerReference"`
	IsDHI           bool              `json:"isDHI"`
}

type imCase struct {
	Name      string              `json:"name"`
	Root      string              `json:"root"`
	DHIKey    *ocKey              `json:"dhiKey"`
	Platform  imPlatform          `json:"platform"`
	Index     ocispecs.Descriptor `json:"index"`
	Blobs     map[string]string   `json:"blobs"`
	Referrers []imRefCall         `json:"referrers"`
	Error     string              `json:"error"`
	Result    *imResult           `json:"result"`
}

// buildx's toSignatureKind and toSignatureType.
func kindName(k types.Kind) string {
	switch k {
	case types.KindDockerGithubBuilder:
		return "docker-github-builder"
	case types.KindDockerHardenedImage:
		return "docker-hardened-image"
	case types.KindSelfSignedGithubRepo:
		return "self-signed-github-repo"
	case types.KindSelfSigned:
		return "self-signed"
	case types.KindUntrusted:
		return "untrusted"
	}
	return ""
}

func typeName(t types.SignatureType) string {
	switch t {
	case types.SignatureBundleV03:
		return "bundle-v0.3"
	case types.SignatureSimpleSigningV1:
		return "simplesigning-v1"
	}
	return ""
}

// ---------------------------------------------------------------- images

const (
	mtManifest   = ocispecs.MediaTypeImageManifest
	mtIndex      = ocispecs.MediaTypeImageIndex
	mtEmpty      = "application/vnd.oci.empty.v1+json"
	attArtifact  = "application/vnd.docker.attestation.manifest.v1+json"
	cosignConfig = "application/vnd.dev.cosign.artifact.sig.v1+json"
)

// imSpec: what an image and its signature are made of.
type imSpec struct {
	plats       []ocispecs.Platform // the index's image manifests (default linux/arm64, linux/amd64)
	noPlatEntry bool                // one more image manifest without a platform, first
	anyType     bool                // the DHI registry answers referrers of every artifact type
	target      int                 // which image manifest is attested and signed
	want        *ocispecs.Platform  // the platform asked (default the target's)
	sig         string              // "bundle" (default), "cosign", "none", "both", "both-bundle-last"
	noAtt       bool
	predicates  []string // the attestation's layers' predicate types
	cosignTlog  string   // "" none, "old", "new", "new-b64-logid", "old-payload", "garbage"
	dhi         bool
	bs          bundleSpec // the signer's spec (leaf, entries, timestamps)

	editIndex   func(map[string]any)
	editAtt     func(map[string]any)
	editSig     func(map[string]any)
	editPayload func(map[string]any)
	attBytes    func([]byte) []byte // the attestation manifest as served
	sigBytes    func([]byte) []byte // the signature manifest as served
	layerBytes  func([]byte) []byte // the bundle or payload as served
	indexBytes  func([]byte) []byte
	dropBlob    string // "att", "sig", "layer", "index"
	// The documents as written, before their digests are taken: for what a map cannot
	// hold (duplicate members).
	attRaw, sigRaw, indexRaw, payloadRaw func([]byte) []byte
	dhiValidFrom                         int64 // the DHI key's validFrom, where not an hour before T0
	extraSigs                            []string
	dhiRefsKeep                          func([]ocispecs.Descriptor) []ocispecs.Descriptor
	sigDesc                              func(*ocispecs.Descriptor) // the signature manifest's descriptor as the chain gives it
	indexMT                              string                     // the index's descriptor's media type, where not an index's
}

type imBuilt struct {
	index    ocispecs.Descriptor
	blobs    map[string]blob
	provider policyimage.ReferrersProvider
}

func jsonBytes(t *testing.T, v any) []byte {
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

func descOf(mt string, b []byte) ocispecs.Descriptor {
	return ocispecs.Descriptor{MediaType: mt, Digest: digest.FromBytes(b), Size: int64(len(b))}
}

func descJSON(d ocispecs.Descriptor) map[string]any {
	m := map[string]any{"mediaType": d.MediaType, "digest": d.Digest.String(), "size": d.Size}
	if d.ArtifactType != "" {
		m["artifactType"] = d.ArtifactType
	}
	if len(d.Annotations) > 0 {
		a := map[string]any{}
		for k, v := range d.Annotations {
			a[k] = v
		}
		m["annotations"] = a
	}
	if d.Platform != nil {
		p := map[string]any{"architecture": d.Platform.Architecture, "os": d.Platform.OS}
		if d.Platform.Variant != "" {
			p["variant"] = d.Platform.Variant
		}
		if d.Platform.OSVersion != "" {
			p["os.version"] = d.Platform.OSVersion
		}
		if len(d.Platform.OSFeatures) > 0 {
			p["os.features"] = d.Platform.OSFeatures
		}
		m["platform"] = p
	}
	return m
}

var emptyConfig = descOf(mtEmpty, []byte("{}"))

func (w *world) image(s imSpec) imBuilt {
	t := w.t
	blobs := map[string]blob{}
	put := func(d ocispecs.Descriptor, b []byte) {
		blobs[d.Digest.String()] = blob{desc: d, data: b}
	}
	plats := s.plats
	if plats == nil {
		plats = []ocispecs.Platform{{OS: "linux", Architecture: "arm64"}, {OS: "linux", Architecture: "amd64"}}
	}
	var images []ocispecs.Descriptor
	var entries []any
	if s.noPlatEntry {
		b := jsonBytes(t, map[string]any{"schemaVersion": 2, "mediaType": mtManifest, "config": descJSON(emptyConfig), "layers": []any{}, "annotations": map[string]any{"which": "no platform"}})
		d := descOf(mtManifest, b)
		put(d, b)
		e := descJSON(d)
		if s.dhi {
			e["annotations"] = map[string]any{"com.docker.dhi.build.id": "b"}
		}
		entries = append(entries, e)
	}
	for i, p := range plats {
		b := jsonBytes(t, map[string]any{"schemaVersion": 2, "mediaType": mtManifest, "config": descJSON(emptyConfig), "layers": []any{}, "annotations": map[string]any{"which": fmt.Sprint(i)}})
		d := descOf(mtManifest, b)
		pc := p
		d.Platform = &pc
		put(d, b)
		images = append(images, d)
		e := descJSON(d)
		if s.dhi {
			e["annotations"] = map[string]any{"com.docker.dhi.build.id": "b" + fmt.Sprint(i)}
		}
		entries = append(entries, e)
	}
	img := images[s.target]
	predicates := s.predicates
	if predicates == nil {
		predicates = []string{slsa1.PredicateSLSAProvenance}
	}
	var att ocispecs.Descriptor
	var attEntry map[string]any
	if !s.noAtt {
		var layers []any
		for i, pt := range predicates {
			lb := []byte(fmt.Sprintf(`{"_type":"https://in-toto.io/Statement/v1","predicateType":%q,"n":%d}`, pt, i))
			ld := descOf("application/vnd.in-toto+json", lb)
			put(ld, lb)
			l := descJSON(ld)
			l["annotations"] = map[string]any{"in-toto.io/predicate-type": pt}
			layers = append(layers, l)
		}
		m := map[string]any{
			"schemaVersion": 2, "mediaType": mtManifest, "artifactType": attArtifact,
			"config": descJSON(emptyConfig), "layers": layers,
			"subject": descJSON(ocispecs.Descriptor{MediaType: img.MediaType, Digest: img.Digest, Size: img.Size}),
		}
		if s.dhi {
			m["artifactType"] = "application/vnd.in-toto+json"
		}
		if s.editAtt != nil {
			s.editAtt(m)
		}
		b := jsonBytes(t, m)
		if s.attRaw != nil {
			b = s.attRaw(b)
		}
		att = descOf(mtManifest, b)
		served := b
		if s.attBytes != nil {
			served = s.attBytes(b)
		}
		put(att, served)
		if s.dhi {
			att.ArtifactType = "application/vnd.in-toto+json"
			att.Annotations = map[string]string{"in-toto.io/predicate-type": predicates[0]}
		} else {
			attEntry = descJSON(att)
			attEntry["platform"] = map[string]any{"architecture": "unknown", "os": "unknown"}
			attEntry["annotations"] = map[string]any{
				"vnd.docker.reference.digest": img.Digest.String(),
				"vnd.docker.reference.type":   "attestation-manifest",
			}
			entries = append(entries, attEntry)
		}
	}
	// The signature manifests.
	var sigs []ocispecs.Descriptor
	mkSig := func(kind string) ocispecs.Descriptor {
		var layer ocispecs.Descriptor
		var lb []byte
		m := map[string]any{"schemaVersion": 2, "mediaType": mtManifest}
		attSubject := descJSON(ocispecs.Descriptor{MediaType: att.MediaType, Digest: att.Digest, Size: att.Size})
		switch kind {
		case "bundle":
			bs := s.bs
			attData := blobs[att.Digest.String()].data
			bs.artifact = attData
			if bs.entries == nil {
				bs.entries = []entrySpec{{kind: "dsse"}}
			}
			bb := w.bundle(bs)
			lb = enc(bb.json)
			layer = descOf(policyimage.ArtifactTypeSigstoreBundle, lb)
			m["artifactType"] = policyimage.ArtifactTypeSigstoreBundle
			m["config"] = descJSON(emptyConfig)
		default: // cosign
			payload := map[string]any{
				"critical": map[string]any{
					"identity": map[string]any{"docker-reference": "docker.io/shards/test"},
					"image":    map[string]any{"docker-manifest-digest": att.Digest.String()},
					"type":     "cosign container image signature",
				},
				"optional": nil,
			}
			if s.editPayload != nil {
				s.editPayload(payload)
			}
			lb = jsonBytes(t, payload)
			if s.payloadRaw != nil {
				lb = s.payloadRaw(lb)
			}
			layer = descOf(policyimage.MediaTypeCosignSimpleSigning, lb)
			bs := s.bs
			bs.content = "msg"
			bs.artifact = lb
			if s.dhi {
				bs.material = "pubkey"
				bs.signerKind = "p256-other"
			}
			bs.entries = []entrySpec{{kind: "hashedrekord"}}
			bb := w.bundle(bs)
			ann := map[string]any{"dev.cosignproject.cosign/signature": b64(bb.signed.sig)}
			if bb.signed.cert != nil {
				ann["dev.sigstore.cosign/certificate"] = string(certPEM(bb.signed.cert))
			}
			e := bb.json["verificationMaterial"].(map[string]any)["tlogEntries"].([]any)[0].(map[string]any)
			keyID, _ := base64.StdEncoding.DecodeString(e["logId"].(map[string]any)["keyId"].(string))
			it, _ := strconv.ParseInt(e["integratedTime"].(string), 10, 64)
			li, _ := strconv.ParseInt(e["logIndex"].(string), 10, 64)
			set := e["inclusionPromise"].(map[string]any)["signedEntryTimestamp"].(string)
			switch s.cosignTlog {
			case "old":
				ann["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, map[string]any{
					"SignedEntryTimestamp": set,
					"Payload": map[string]any{
						"body": e["canonicalizedBody"], "integratedTime": it, "logIndex": li, "logID": hex.EncodeToString(keyID),
					},
				}))
			case "old-payload":
				ann["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, map[string]any{
					"SignedEntryTimestamp": set,
					"logID":                hex.EncodeToString(keyID),
					"integratedTime":       fmt.Sprint(it),
					"logIndex":             li,
					"Payload":              map[string]any{"body": e["canonicalizedBody"]},
				}))
			case "new", "new-b64-logid":
				id := hex.EncodeToString(keyID)
				if s.cosignTlog == "new-b64-logid" {
					id = base64.StdEncoding.EncodeToString(keyID)
				} else {
					id = strings.ToUpper(id)
				}
				ann["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, map[string]any{
					"content": map[string]any{"verificationMaterial": map[string]any{"tlogEntries": []any{map[string]any{
						"logIndex": fmt.Sprint(li), "logId": map[string]any{"keyId": id},
						"integratedTime": it, "inclusionPromise": map[string]any{"signedEntryTimestamp": set},
						"canonicalizedBody": e["canonicalizedBody"],
					}}}},
				}))
			case "garbage":
				ann["dev.sigstore.cosign/bundle"] = "{not json"
			}
			l := descJSON(layer)
			l["annotations"] = ann
			m["layers"] = []any{l}
			m["config"] = descJSON(descOf(cosignConfig, []byte("{}")))
			m["subject"] = attSubject
			if s.editSig != nil {
				s.editSig(m)
			}
			served := lb
			if s.layerBytes != nil {
				served = s.layerBytes(lb)
			}
			put(layer, served)
			b := jsonBytes(t, m)
			if s.sigRaw != nil {
				b = s.sigRaw(b)
			}
			d := descOf(mtManifest, b)
			sb := b
			if s.sigBytes != nil {
				sb = s.sigBytes(b)
			}
			put(d, sb)
			d.ArtifactType = cosignConfig
			return d
		}
		if _, ok := m["layers"]; !ok {
			m["layers"] = []any{descJSON(layer)}
		}
		m["subject"] = attSubject
		if s.editSig != nil {
			s.editSig(m)
		}
		served := lb
		if s.layerBytes != nil {
			served = s.layerBytes(lb)
		}
		put(layer, served)
		b := jsonBytes(t, m)
		if s.sigRaw != nil {
			b = s.sigRaw(b)
		}
		d := descOf(mtManifest, b)
		sb := b
		if s.sigBytes != nil {
			sb = s.sigBytes(b)
		}
		put(d, sb)
		d.ArtifactType = policyimage.ArtifactTypeSigstoreBundle
		return d
	}
	if !s.noAtt {
		switch s.sig {
		case "", "bundle":
			sigs = append(sigs, mkSig("bundle"))
		case "cosign":
			sigs = append(sigs, mkSig("cosign"))
		case "both":
			sigs = append(sigs, mkSig("cosign"), mkSig("bundle"))
		case "both-bundle-last":
			sigs = append(sigs, mkSig("bundle"), mkSig("cosign"))
		}
		for _, x := range s.extraSigs {
			sigs = append(sigs, ocispecs.Descriptor{MediaType: mtManifest, Digest: digest.Digest(x), Size: 3})
		}
	}
	idx := map[string]any{"schemaVersion": 2, "mediaType": mtIndex, "manifests": entries}
	if s.dhi {
		idx["annotations"] = map[string]any{"org.opencontainers.image.title": "dhi/shards-test"}
	}
	if s.editIndex != nil {
		s.editIndex(idx)
	}
	ib := jsonBytes(t, idx)
	if s.indexRaw != nil {
		ib = s.indexRaw(ib)
	}
	index := descOf(mtIndex, ib)
	served := ib
	if s.indexBytes != nil {
		served = s.indexBytes(ib)
	}
	put(index, served)
	switch s.dropBlob {
	case "att":
		delete(blobs, att.Digest.String())
	case "sig":
		if len(sigs) > 0 {
			delete(blobs, sigs[0].Digest.String())
		}
	case "index":
		delete(blobs, index.Digest.String())
	}
	var provider policyimage.ReferrersProvider
	if s.dhi {
		refs := map[string][]ocispecs.Descriptor{}
		if !s.noAtt {
			r := []ocispecs.Descriptor{att}
			if s.dhiRefsKeep != nil {
				r = s.dhiRefsKeep(r)
			}
			refs[img.Digest.String()] = r
			for _, sd := range sigs {
				if s.sigDesc != nil {
					s.sigDesc(&sd)
				}
				refs[att.Digest.String()] = append(refs[att.Digest.String()], sd)
			}
		}
		provider = &mapProvider{blobs: blobs, referrers: refs, anyType: s.anyType}
	} else {
		var ds []digest.Digest
		for _, sd := range sigs {
			ds = append(ds, sd.Digest)
		}
		bl := map[string]blob{}
		for k, v := range blobs {
			bl[k] = v
		}
		if s.sigDesc != nil {
			for _, sd := range sigs {
				b := bl[sd.Digest.String()]
				d := b.desc
				s.sigDesc(&d)
				b.desc = d
				bl[sd.Digest.String()] = b
			}
		}
		provider = &acProvider{blobs: bl, signatures: ds, att: att.Digest}
	}
	if s.indexMT != "" {
		index.MediaType = s.indexMT
	}
	return imBuilt{index: ocispecs.Descriptor{MediaType: index.MediaType, Digest: index.Digest, Size: index.Size}, blobs: blobs, provider: provider}
}

// member adds members to a JSON object's end.
func member(extra string) func([]byte) []byte {
	return func(b []byte) []byte {
		out := append([]byte{}, b[:len(b)-1]...)
		return append(append(out, ","+extra...), '}')
	}
}

// rekorEdit edits the signature layer's Rekor bundle annotation as JSON.
func rekorEdit(f func(map[string]any)) func(map[string]any) {
	return func(m map[string]any) {
		a := m["layers"].([]any)[0].(map[string]any)["annotations"].(map[string]any)
		var rb map[string]any
		if err := json.Unmarshal([]byte(a["dev.sigstore.cosign/bundle"].(string)), &rb); err != nil {
			panic(err)
		}
		f(rb)
		b, err := json.Marshal(rb)
		if err != nil {
			panic(err)
		}
		a["dev.sigstore.cosign/bundle"] = string(b)
	}
}

func rekorNewEntry(rb map[string]any) map[string]any {
	return rb["content"].(map[string]any)["verificationMaterial"].(map[string]any)["tlogEntries"].([]any)[0].(map[string]any)
}

func platformJSON(p ocispecs.Platform) imPlatform {
	f := p.OSFeatures
	if f == nil {
		f = []string{}
	}
	return imPlatform{OS: p.OS, Arch: p.Architecture, Variant: p.Variant, OSVersion: p.OSVersion, OSFeatures: f}
}

func TestShardsImageOracle(t *testing.T) {
	out := os.Getenv("SHARDS_IMAGE_OUT")
	if out == "" {
		t.Skip("SHARDS_IMAGE_OUT not set")
	}
	cryptotest.SetGlobalRandom(t, 2)
	w := newWorld(t)
	dk := ecKey(t, "leaf other", elliptic.P256()) // the "p256-other" signer: the DHI key
	dhiPEM := string(keyPEM(t, dk.Public()))
	roots := map[string]string{}
	var cases []*imCase

	// A Fulcio-like CA under Sigstore's own intermediate name, for the signer kinds.
	srk := ecKey(t, "sigstore root", elliptic.P384())
	sik := ecKey(t, "sigstore intermediate", elliptic.P384())
	sigstoreCA := ca{rootKey: srk, interKey: sik}
	sigstoreCA.root = mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{Organization: []string{"sigstore.dev"}, CommonName: "sigstore"},
		NotBefore:             t0.AddDate(-1, 0, 0),
		NotAfter:              t0.AddDate(4, 0, 0),
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		BasicConstraintsValid: true,
		IsCA:                  true,
		MaxPathLen:            1,
	}, nil, srk.Public(), srk)
	sigstoreCA.inter = mkcert(t, &x509.Certificate{
		Subject:               pkix.Name{Organization: []string{"sigstore.dev"}, CommonName: "sigstore-intermediate"},
		NotBefore:             t0.AddDate(-1, 0, 0),
		NotAfter:              t0.AddDate(4, 0, 0),
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageCodeSigning},
		BasicConstraintsValid: true,
		IsCA:                  true,
		MaxPathLenZero:        true,
	}, sigstoreCA.root, sik.Public(), srk)
	withSigstoreCA := w.trustedRoot(func(r *prototrustroot.TrustedRoot) {
		r.CertificateAuthorities = append(r.CertificateAuthorities, &prototrustroot.CertificateAuthority{
			Uri: "https://fulcio.sigstore.test",
			CertChain: &v1common.X509CertificateChain{Certificates: []*v1common.X509Certificate{
				{RawBytes: sigstoreCA.inter.Raw}, {RawBytes: sigstoreCA.root.Raw},
			}},
			ValidFor: tr(t0.AddDate(-1, 0, 0), nil),
		})
	})

	add := func(name, rootJSON string, s imSpec) {
		currentCase = name
		b := w.image(s)
		want := s.want
		if want == nil {
			plats := s.plats
			if plats == nil {
				plats = []ocispecs.Platform{{OS: "linux", Architecture: "arm64"}, {OS: "linux", Architecture: "amd64"}}
			}
			p := plats[s.target]
			want = &p
		}
		id := hex.EncodeToString(sha([]byte(rootJSON)))[:16]
		roots[id] = rootJSON
		c := &imCase{Name: name, Root: id, Platform: platformJSON(*want), Index: b.index, Blobs: map[string]string{}}
		for k, v := range b.blobs {
			c.Blobs[k] = b64(v.data)
		}
		vf := t0.Add(-time.Hour).Unix()
		if s.dhiValidFrom != 0 {
			vf = s.dhiValidFrom
		}
		if s.dhi {
			c.DHIKey = &ocKey{PEM: dhiPEM, ValidFrom: vf}
		}
		tm, err := root.NewTrustedRootFromJSON([]byte(rootJSON))
		if err != nil {
			t.Fatal(err)
		}
		rec := &recorder{ReferrersProvider: b.provider}
		si, err := verifyImage(context.Background(), rec, b.index, want, tm, dhiKey{pub: dk.Public(), validFrom: vf})
		c.Referrers = rec.calls
		if c.Referrers == nil {
			c.Referrers = []imRefCall{}
		}
		if err != nil {
			c.Error = err.Error()
		} else {
			r := &imResult{Kind: kindName(si.Kind), SignatureType: typeName(si.SignatureType), DockerReference: si.DockerReference, IsDHI: si.IsDHI, Timestamps: []ocTimestamp{}}
			if si.Signer != nil {
				r.Signer = summary(si.Signer)
			}
			for _, ts := range si.Timestamps {
				r.Timestamps = append(r.Timestamps, ocTimestamp{Type: ts.Type, URI: ts.URI, Secs: ts.Timestamp.Unix(), Nanos: ts.Timestamp.Nanosecond()})
			}
			c.Result = r
		}
		cases = append(cases, c)
	}
	std := w.rootJSON
	full := []entrySpec{{kind: "dsse"}}
	tsa := []tsaSpec{{}}
	arm64 := ocispecs.Platform{OS: "linux", Architecture: "arm64"}
	amd64 := ocispecs.Platform{OS: "linux", Architecture: "amd64"}
	set := func(path ...string) func(m map[string]any, v any) {
		return func(m map[string]any, v any) {
			for _, p := range path[:len(path)-1] {
				m = m[p].(map[string]any)
			}
			m[path[len(path)-1]] = v
		}
	}
	layer0 := func(m map[string]any) map[string]any { return m["layers"].([]any)[0].(map[string]any) }

	// ---- valid
	add("bundle signed, tlog", std, imSpec{bs: bundleSpec{entries: full}})
	add("bundle signed, tlog and tsa", std, imSpec{bs: bundleSpec{entries: full, tsas: tsa}})
	add("bundle signed for amd64", std, imSpec{target: 1})
	add("cosign signed, old rekor bundle", std, imSpec{sig: "cosign", cosignTlog: "old"})
	add("cosign signed, rekor bundle at top", std, imSpec{sig: "cosign", cosignTlog: "old-payload"})
	add("cosign signed, new rekor bundle", std, imSpec{sig: "cosign", cosignTlog: "new"})
	add("cosign signed, new rekor bundle base64 log id", std, imSpec{sig: "cosign", cosignTlog: "new-b64-logid"})
	add("cosign signed, garbage rekor bundle", std, imSpec{sig: "cosign", cosignTlog: "garbage"})
	add("cosign signed, no rekor bundle", std, imSpec{sig: "cosign"})
	add("bundle preferred over cosign", std, imSpec{sig: "both"})
	add("bundle first then cosign", std, imSpec{sig: "both-bundle-last"})
	add("slsa v0.2 attestation", std, imSpec{predicates: []string{slsa02.PredicateSLSAProvenance}})
	add("slsa among other layers", std, imSpec{predicates: []string{"https://spdx.dev/Document", slsa1.PredicateSLSAProvenance}})
	// Signer kinds.
	leafURI0 := leafURI
	exts0 := fulcioV2Exts
	setExt := func(n int, v string) {
		fulcioV2Exts = slices.Clone(fulcioV2Exts)
		for i := range fulcioV2Exts {
			if fulcioV2Exts[i].n == n {
				fulcioV2Exts[i].v = v
			}
		}
	}
	add("signer: self-signed github repo", withSigstoreCA, imSpec{bs: bundleSpec{entries: full, leaf: leafSpec{ca: &sigstoreCA}}})
	leafURI = "https://github.com/docker/github-builder/.github/workflows/build.yml@refs/heads/main"
	setExt(9, leafURI)
	add("signer: docker github builder", withSigstoreCA, imSpec{bs: bundleSpec{entries: full, leaf: leafSpec{ca: &sigstoreCA}}})
	add("signer: docker github builder, cosign (not a bundle)", withSigstoreCA, imSpec{sig: "cosign", cosignTlog: "old", bs: bundleSpec{leaf: leafSpec{ca: &sigstoreCA}}})
	setExt(11, "self-hosted")
	add("signer: self-signed, self-hosted runner", withSigstoreCA, imSpec{bs: bundleSpec{entries: full, leaf: leafSpec{ca: &sigstoreCA}}})
	leafURI = leafURI0
	fulcioV2Exts = exts0
	add("signer: untrusted CA name", std, imSpec{bs: bundleSpec{entries: full}})

	// ---- the chain
	add("no attestation", std, imSpec{noAtt: true})
	add("no signature", std, imSpec{sig: "none"})
	add("not an index", std, imSpec{indexMT: mtManifest})
	add("index blob missing", std, imSpec{dropBlob: "index"})
	add("index digest mismatch (read as nothing)", std, imSpec{indexBytes: func(b []byte) []byte { return append(b, ' ') }})
	add("index not json", std, imSpec{indexBytes: func(b []byte) []byte { return []byte("{") }})
	add("index manifests not a list", std, imSpec{editIndex: func(m map[string]any) { m["manifests"] = "x" }})
	add("index size a string", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["size"] = "12"
	}})
	add("index size a fraction", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["size"] = 1.5
	}})
	add("index data not base64", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["data"] = "!!"
	}})
	add("index fields in other case", std, imSpec{editIndex: func(m map[string]any) {
		e := m["manifests"].([]any)[0].(map[string]any)
		e["MEDIATYPE"] = e["mediaType"]
		delete(e, "mediaType")
	}})
	add("platform not found", std, imSpec{want: &ocispecs.Platform{OS: "linux", Architecture: "s390x"}})
	add("platform arm64 v8 asked", std, imSpec{want: &ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "v8"}})
	add("platform arm64 unknown variant asked", std, imSpec{want: &ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "v12"}})
	add("platform aarch64 asked", std, imSpec{want: &ocispecs.Platform{OS: "Linux", Architecture: "aarch64"}})
	add("platform arm v7 picked among arm variants", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "arm", Variant: "v6"}, {OS: "linux", Architecture: "arm", Variant: "v7"}, {OS: "linux", Architecture: "arm", Variant: "v5"}},
		target: 1, want: &ocispecs.Platform{OS: "linux", Architecture: "arm", Variant: "v7"},
	})
	add("platform arm v6 runs on v7, attestation is v6's", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "arm", Variant: "v6"}},
		target: 0, want: &ocispecs.Platform{OS: "linux", Architecture: "arm", Variant: "v7"},
	})
	add("platform amd64 v1 for v3", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "amd64"}, {OS: "linux", Architecture: "amd64", Variant: "v2"}},
		target: 1, want: &ocispecs.Platform{OS: "linux", Architecture: "amd64", Variant: "v3"},
	})
	add("platform 386 for amd64", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "386"}},
		target: 0, want: &amd64,
	})
	add("platform os.features preferred", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "arm64"}, {OS: "linux", Architecture: "arm64", OSFeatures: []string{"a"}}},
		target: 1, want: &ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a", "b"}},
	})
	add("platform os.features not a subset", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "linux", Architecture: "arm64", OSFeatures: []string{"z"}}},
		target: 0, want: &ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a"}},
	})
	add("platform windows os.version", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "windows", Architecture: "amd64", OSVersion: "10.0.20348.1"}, {OS: "windows", Architecture: "amd64", OSVersion: "10.0.17763.1"}},
		target: 0, want: &ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.26100.5"},
	})
	add("platform windows os.version mismatch", std, imSpec{
		plats:  []ocispecs.Platform{{OS: "windows", Architecture: "amd64", OSVersion: "10.0.17763.1"}},
		target: 0, want: &ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.20348.5"},
	})
	add("platform entry without platform picked last", std, imSpec{noPlatEntry: true, target: 0})
	add("platform only an entry without platform", std, imSpec{noPlatEntry: true, plats: []ocispecs.Platform{{OS: "linux", Architecture: "s390x"}}, target: 0, want: &arm64})
	add("image entry docker schema 2", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["mediaType"] = "application/vnd.docker.distribution.manifest.v2+json"
	}})
	add("image entry not a manifest type", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["mediaType"] = "application/vnd.oci.image.index.v1+json"
	}})
	add("attestation annotation for another image", std, imSpec{editIndex: func(m map[string]any) {
		for _, e := range m["manifests"].([]any) {
			em := e.(map[string]any)
			if a, ok := em["annotations"].(map[string]any); ok && a["vnd.docker.reference.type"] != nil {
				a["vnd.docker.reference.digest"] = "sha256:" + strings.Repeat("0", 64)
			}
		}
	}})
	add("attestation blob missing", std, imSpec{dropBlob: "att"})
	add("attestation digest mismatch", std, imSpec{attBytes: func(b []byte) []byte { return append(b, ' ') }})
	add("attestation not json", std, imSpec{attBytes: func(b []byte) []byte { return []byte("[") }})
	add("attestation no subject", std, imSpec{editAtt: func(m map[string]any) { delete(m, "subject") }})
	add("attestation subject another digest", std, imSpec{editAtt: func(m map[string]any) {
		set("subject", "digest")(m, "sha256:"+strings.Repeat("1", 64))
	}})
	add("attestation subject an index", std, imSpec{editAtt: func(m map[string]any) { set("subject", "mediaType")(m, mtIndex) }})
	add("attestation subject other media type", std, imSpec{editAtt: func(m map[string]any) { set("subject", "mediaType")(m, "text/plain") }})
	add("attestation subject size", std, imSpec{editAtt: func(m map[string]any) { set("subject", "size")(m, 1) }})
	add("attestation no SLSA layer", std, imSpec{predicates: []string{"https://spdx.dev/Document"}})
	add("attestation no layers", std, imSpec{editAtt: func(m map[string]any) { m["layers"] = nil }})
	add("signature manifest missing", std, imSpec{dropBlob: "sig"})
	add("signature manifest digest mismatch", std, imSpec{sigBytes: func(b []byte) []byte { return append(b, ' ') }})
	add("signature manifest not json", std, imSpec{sigBytes: func(b []byte) []byte { return []byte("x") }})
	add("signature manifest no subject", std, imSpec{editSig: func(m map[string]any) { delete(m, "subject") }})
	add("signature subject another digest", std, imSpec{editSig: func(m map[string]any) {
		set("subject", "digest")(m, "sha256:"+strings.Repeat("2", 64))
	}})
	add("signature subject media type", std, imSpec{editSig: func(m map[string]any) { set("subject", "mediaType")(m, "x") }})
	add("signature subject size", std, imSpec{editSig: func(m map[string]any) { set("subject", "size")(m, 9) }})
	add("signature no layers", std, imSpec{editSig: func(m map[string]any) { m["layers"] = []any{} }})
	add("signature layer other media type", std, imSpec{editSig: func(m map[string]any) { layer0(m)["mediaType"] = "text/plain" }})
	add("bundle manifest without bundle artifact type", std, imSpec{editSig: func(m map[string]any) { delete(m, "artifactType") }})
	add("bundle layer missing", std, imSpec{editSig: func(m map[string]any) {
		layer0(m)["digest"] = "sha256:" + strings.Repeat("3", 64)
	}})
	add("bundle layer digest mismatch", std, imSpec{layerBytes: func(b []byte) []byte { return append(b, ' ') }})
	add("bundle layer not a bundle", std, imSpec{layerBytes: func(b []byte) []byte { return []byte(`{"mediaType":"x"}`) }})
	add("bundle over another artifact", std, imSpec{bs: bundleSpec{entries: full, subjects: []map[string]any{{"name": "x", "digest": map[string]any{"sha256": strings.Repeat("4", 64)}}}}})
	add("bundle without log entry", std, imSpec{bs: bundleSpec{entries: []entrySpec{}, tsas: tsa}})
	add("referrers: unparseable signature manifest", std, imSpec{extraSigs: []string{"sha256:" + strings.Repeat("5", 64)}})
	add("cosign payload digest mismatch", std, imSpec{sig: "cosign", cosignTlog: "old", editPayload: func(m map[string]any) {
		set("critical", "image", "docker-manifest-digest")(m, "sha256:"+strings.Repeat("6", 64))
	}})
	add("cosign payload wrong type", std, imSpec{sig: "cosign", cosignTlog: "old", editPayload: func(m map[string]any) {
		set("critical", "type")(m, "something")
	}})
	add("cosign payload type a number", std, imSpec{sig: "cosign", cosignTlog: "old", editPayload: func(m map[string]any) {
		set("critical", "type")(m, 3)
	}})
	add("cosign payload not json", std, imSpec{sig: "cosign", cosignTlog: "old", layerBytes: func(b []byte) []byte { return []byte("nope") }})
	add("cosign no signature annotation", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		delete(layer0(m)["annotations"].(map[string]any), "dev.cosignproject.cosign/signature")
	}})
	add("cosign signature not base64", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.cosignproject.cosign/signature"] = "**"
	}})
	add("cosign no certificate", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		delete(layer0(m)["annotations"].(map[string]any), "dev.sigstore.cosign/certificate")
	}})
	add("cosign certificate not PEM", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.sigstore.cosign/certificate"] = "certificate"
	}})
	add("cosign certificate bad DER", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.sigstore.cosign/certificate"] = string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: []byte{0x30, 0x01, 0x00}}))
	}})
	add("cosign signature over other content", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.cosignproject.cosign/signature"] = b64([]byte("not the signature"))
	}})
	add("cosign rekor bundle body not base64", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.sigstore.cosign/bundle"] = `{"SignedEntryTimestamp":"!!","Payload":{}}`
	}})
	add("cosign rekor bundle log id not hex", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		var rb map[string]any
		_ = json.Unmarshal([]byte(a["dev.sigstore.cosign/bundle"].(string)), &rb)
		rb["Payload"].(map[string]any)["logID"] = "zz"
		a["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, rb))
	}})
	add("cosign rekor bundle body not rekor's", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		var rb map[string]any
		_ = json.Unmarshal([]byte(a["dev.sigstore.cosign/bundle"].(string)), &rb)
		rb["Payload"].(map[string]any)["body"] = b64([]byte(`{"kind":"nothing"}`))
		a["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, rb))
	}})
	add("cosign rekor bundle SET wrong", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		var rb map[string]any
		_ = json.Unmarshal([]byte(a["dev.sigstore.cosign/bundle"].(string)), &rb)
		rb["Payload"].(map[string]any)["integratedTime"] = float64(t0.Unix() + 1)
		a["dev.sigstore.cosign/bundle"] = string(jsonBytes(t, rb))
	}})
	add("cosign signature descriptor without artifact type (legacy)", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		delete(m, "artifactType")
	}})

	// ---- Docker Hardened Images
	add("dhi signed, no rekor bundle", std, imSpec{dhi: true, sig: "cosign"})
	add("dhi signed, rekor bundle", std, imSpec{dhi: true, sig: "cosign", cosignTlog: "old"})
	add("dhi signed, new rekor bundle", std, imSpec{dhi: true, sig: "cosign", cosignTlog: "new"})
	add("dhi no docker reference", std, imSpec{dhi: true, sig: "cosign", editPayload: func(m map[string]any) {
		set("critical", "identity", "docker-reference")(m, "")
	}})
	add("dhi signed by another key", std, imSpec{dhi: true, sig: "cosign", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.cosignproject.cosign/signature"] = b64([]byte("x"))
	}})
	add("dhi bundle signature (keyless)", std, imSpec{dhi: true, sig: "bundle", bs: bundleSpec{entries: full}})
	add("dhi no attestation referrer", std, imSpec{dhi: true, sig: "cosign", dhiRefsKeep: func([]ocispecs.Descriptor) []ocispecs.Descriptor { return nil }})
	add("dhi attestation referrer without predicate", std, imSpec{dhi: true, sig: "cosign", dhiRefsKeep: func(r []ocispecs.Descriptor) []ocispecs.Descriptor {
		r[0].Annotations = nil
		return r
	}})
	add("dhi attestation referrer other artifact type", std, imSpec{dhi: true, sig: "cosign", dhiRefsKeep: func(r []ocispecs.Descriptor) []ocispecs.Descriptor {
		r[0].ArtifactType = "x"
		return r
	}})
	add("dhi signature referrer other artifact type", std, imSpec{dhi: true, sig: "cosign", sigDesc: func(d *ocispecs.Descriptor) { d.ArtifactType = "y" }})
	add("dhi title not dhi", std, imSpec{dhi: true, sig: "cosign", editIndex: func(m map[string]any) {
		m["annotations"] = map[string]any{"org.opencontainers.image.title": "shards/test"}
	}})
	add("dhi an entry without build id", std, imSpec{dhi: true, sig: "cosign", noPlatEntry: true, editIndex: func(m map[string]any) {
		delete(m["manifests"].([]any)[0].(map[string]any), "annotations")
	}})
	add("dhi rekor bundle SET wrong", std, imSpec{dhi: true, sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["Payload"].(map[string]any)["integratedTime"] = float64(t0.Unix() + 1)
	})})
	add("dhi key not yet valid", std, imSpec{dhi: true, sig: "cosign", cosignTlog: "old", dhiValidFrom: t0.Add(time.Hour).Unix()})

	// ---- documents as encoding/json reads them: members repeated, nulls, wrong types
	add("index duplicate annotations merge (dhi)", std, imSpec{dhi: true, sig: "cosign", indexRaw: member(`"annotations":{"other":"x"}`)})
	add("index duplicate annotations null (dhi)", std, imSpec{dhi: true, sig: "cosign", indexRaw: member(`"annotations":null`)})
	add("index duplicate manifests truncate", std, imSpec{indexRaw: member(`"manifests":[{"size":5}]`)})
	add("index manifests grow back into capacity", std, imSpec{indexRaw: member(`"manifests":[{"size":5}],"manifests":[{},{},{}]`)})
	add("index manifests grow past capacity", std, imSpec{indexRaw: member(`"manifests":[{"size":5}],"manifests":[{},{},{},{},{}]`)})
	add("index manifests null then again", std, imSpec{indexRaw: member(`"manifests":null,"manifests":[{},{},{}]`)})
	add("index media type null keeps", std, imSpec{indexRaw: member(`"mediaType":null`)})
	add("index manifest element null keeps", std, imSpec{indexRaw: member(`"manifests":[null,null,null]`)})
	add("index manifest platform null", std, imSpec{indexRaw: member(`"manifests":[{"platform":null},null,null]`)})
	add("index manifest platform merges", std, imSpec{indexRaw: member(`"manifests":[{"platform":{"variant":"v8"}},null,null]`)})
	add("index manifest platform a string", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["platform"] = "linux/arm64"
	}})
	add("index manifest os.features a string", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["platform"].(map[string]any)["os.features"] = "a"
	}})
	add("index manifest os.features element null", std, imSpec{indexRaw: member(`"manifests":[{"platform":{"os.features":["a","b"]}},null,null],"manifests":[{"platform":{"os.features":[null]}},null,null]`)})
	add("index manifest urls element a number", std, imSpec{editIndex: func(m map[string]any) {
		m["manifests"].([]any)[0].(map[string]any)["urls"] = []any{"u", 3}
	}})
	add("index annotation a number", std, imSpec{editIndex: func(m map[string]any) { m["annotations"] = map[string]any{"a": 3, "b": "c"} }})
	add("index annotations an array", std, imSpec{editIndex: func(m map[string]any) { m["annotations"] = []any{} }})
	add("index schemaVersion a string", std, imSpec{editIndex: func(m map[string]any) { m["schemaVersion"] = "2" }})
	add("index schemaVersion huge", std, imSpec{indexRaw: member(`"schemaVersion":1e30`)})
	add("index size negative exponent", std, imSpec{indexRaw: member(`"manifests":[{"size":12e-1}]`)})
	add("index subject merges", std, imSpec{indexRaw: member(`"subject":{"size":1},"subject":{"digest":"sha256:00"}`)})
	add("index type error then base64 error", std, imSpec{indexRaw: member(`"schemaVersion":"x","manifests":[{"data":"!"}]`)})
	add("index two type errors, first kept", std, imSpec{indexRaw: member(`"mediaType":1,"artifactType":[]`)})
	add("index digest a number", std, imSpec{indexRaw: member(`"manifests":[{"digest":5}]`)})
	add("index data emptied, then bytes over nulls", std, imSpec{indexRaw: member(`"manifests":[{"data":"AAEC"}],"manifests":[{"data":[]}],"manifests":[{"data":[null,null,7]}]`)})
	add("index data byte too big", std, imSpec{indexRaw: member(`"manifests":[{"data":[256]}]`)})
	add("index data byte negative", std, imSpec{indexRaw: member(`"manifests":[{"data":[-1]}]`)})
	add("index urls emptied, then nulls", std, imSpec{indexRaw: member(`"manifests":[{"urls":["a","b","c"]}],"manifests":[{"urls":[]}],"manifests":[{"urls":[null,null]}]`)})
	add("index os.features emptied, then grown fresh", std, imSpec{
		plats:    []ocispecs.Platform{{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a"}}},
		target:   0,
		want:     &ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a", "b"}},
		indexRaw: member(`"manifests":[{"platform":{"os.features":[]}}],"manifests":[{"platform":{"os.features":[null]}}]`),
	})
	add("index os.features grown back into capacity (picks the image)", std, imSpec{
		plats:    []ocispecs.Platform{{OS: "linux", Architecture: "arm64", OSFeatures: []string{"b", "a"}}},
		target:   0,
		want:     &ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a", "b"}},
		indexRaw: member(`"manifests":[{"platform":{"os.features":["a"]}}],"manifests":[{"platform":{"os.features":[null,null]}}]`),
	})
	add("index manifests emptied, then grown fresh", std, imSpec{indexRaw: member(`"manifests":[],"manifests":[{},{}]`)})
	add("attestation layers grown back into capacity", std, imSpec{
		predicates: []string{"https://spdx.dev/Document", slsa1.PredicateSLSAProvenance},
		attRaw:     member(`"layers":[{}],"layers":[null,null]`),
	})
	add("cosign payload optional number out of range", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"optional":{"a":[1e400]}`)})
	add("rekor new: log index out of range (old shape read)", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		a["dev.sigstore.cosign/bundle"] = strings.Replace(a["dev.sigstore.cosign/bundle"].(string), `"logIndex":"`, `"logIndex":1e400,"x":"`, 1)
	}})
	add("rekor new: key id with a final sigma", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rekorNewEntry(rb)["logId"] = map[string]any{"keyId": "ΑΣ"}
	})})
	add("rekor new: key id with a dotted capital I", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rekorNewEntry(rb)["logId"] = map[string]any{"keyId": "İ0"}
	})})
	add("rekor new: entries repeated, first merged", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		s := a["dev.sigstore.cosign/bundle"].(string)
		a["dev.sigstore.cosign/bundle"] = strings.Replace(s, `"tlogEntries":[`, `"tlogEntries":[{"logIndex":"1"}],"tlogEntries":[`, 1)
		a["dev.sigstore.cosign/bundle"] = strings.Replace(a["dev.sigstore.cosign/bundle"].(string), `"tlogEntries":[{"canonicalizedBody"`, `"tlogEntries":[{"canonicalizedBody"`, 1)
	}})
	add("rekor old: integrated time out of range", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		a := layer0(m)["annotations"].(map[string]any)
		a["dev.sigstore.cosign/bundle"] = strings.Replace(a["dev.sigstore.cosign/bundle"].(string), `{`, `{"integratedTime":-1e999,`, 1)
	}})
	add("platform entry without platform listed last", std, imSpec{noPlatEntry: true, target: 0, editIndex: func(m map[string]any) {
		ms := m["manifests"].([]any)
		m["manifests"] = append(append([]any{}, ms[1:]...), ms[0])
	}})
	add("attestation entry of another reference type", std, imSpec{editIndex: func(m map[string]any) {
		for _, e := range m["manifests"].([]any) {
			if a, ok := e.(map[string]any)["annotations"].(map[string]any); ok && a["vnd.docker.reference.type"] != nil {
				a["vnd.docker.reference.type"] = "sbom"
			}
		}
	}})
	add("bundle signed by a public key (not dhi)", std, imSpec{bs: bundleSpec{entries: full, material: "pubkey"}})
	add("dhi a build id empty", std, imSpec{dhi: true, sig: "cosign", editIndex: func(m map[string]any) {
		m["manifests"].([]any)[1].(map[string]any)["annotations"] = map[string]any{"com.docker.dhi.build.id": ""}
	}})
	add("dhi title with dhi/ inside", std, imSpec{dhi: true, sig: "cosign", editIndex: func(m map[string]any) {
		m["annotations"] = map[string]any{"org.opencontainers.image.title": "x/dhi/test"}
	}})
	add("dhi slsa v0.2 attestation", std, imSpec{dhi: true, sig: "cosign", predicates: []string{slsa02.PredicateSLSAProvenance}})
	add("dhi registry ignoring artifact types, other type", std, imSpec{dhi: true, sig: "cosign", anyType: true, dhiRefsKeep: func(r []ocispecs.Descriptor) []ocispecs.Descriptor {
		r[0].ArtifactType = "x"
		return r
	}})
	add("dhi registry ignoring artifact types", std, imSpec{dhi: true, sig: "cosign", anyType: true})
	add("index top level an array", std, imSpec{indexRaw: func([]byte) []byte { return []byte(`[]`) }})
	add("index top level null", std, imSpec{indexRaw: func([]byte) []byte { return []byte(`null`) }})
	add("attestation duplicate subject merges", std, imSpec{attRaw: member(`"subject":{"mediaType":"x"}`)})
	add("attestation subject null", std, imSpec{attRaw: member(`"subject":null`)})
	add("attestation subject a string", std, imSpec{attRaw: member(`"subject":"x"`)})
	add("attestation layers merge annotations", std, imSpec{attRaw: member(`"layers":[{"annotations":{"x":"y"}}]`)})
	add("attestation layer annotation null value", std, imSpec{attRaw: member(`"layers":[{"annotations":{"in-toto.io/predicate-type":null}}]`)})
	add("attestation config a number", std, imSpec{attRaw: member(`"config":5`)})
	add("signature layers element null keeps", std, imSpec{sigRaw: member(`"layers":[null]`)})
	add("signature artifactType null keeps", std, imSpec{sigRaw: member(`"artifactType":null`)})
	add("signature subject size a string", std, imSpec{sigRaw: member(`"subject":{"size":"685"}`)})
	add("cosign payload critical null", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":null`)})
	add("cosign payload critical merges", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":{"identity":{"docker-reference":"other"}}`)})
	add("cosign payload type null keeps", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":{"type":null}`)})
	add("cosign payload critical a string", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":"x"`)})
	add("cosign payload identity an array", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":{"identity":[]}`)})
	add("cosign payload image a number", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"critical":{"image":1}`)})
	add("cosign payload optional a number", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"optional":1`)})
	add("cosign payload optional an object", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: member(`"optional":{"a":[1,{"b":null}]}`)})
	add("cosign payload an array", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: func([]byte) []byte { return []byte(`[1]`) }})
	add("cosign payload in other case", std, imSpec{sig: "cosign", cosignTlog: "old", payloadRaw: func(b []byte) []byte {
		return bytes.Replace(bytes.Replace(b, []byte(`"critical"`), []byte(`"CRITICAL"`), 1), []byte(`"docker-manifest-digest"`), []byte(`"Docker-Manifest-Digest"`), 1)
	}})
	// cosign's Rekor bundle annotation, in its two shapes.
	add("rekor new: integrated time a string", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		e := rekorNewEntry(rb)
		e["integratedTime"] = fmt.Sprint(e["integratedTime"])
	})})
	add("rekor new: log index a number", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		e := rekorNewEntry(rb)
		e["logIndex"], _ = strconv.ParseFloat(e["logIndex"].(string), 64)
	})})
	add("rekor new: integrated time unreadable", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rekorNewEntry(rb)["integratedTime"] = "soon"
	})})
	add("rekor new: integrated time an object", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rekorNewEntry(rb)["integratedTime"] = map[string]any{}
	})})
	add("rekor new: key id a number (old shape read)", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rekorNewEntry(rb)["logId"] = map[string]any{"keyId": 1}
	})})
	add("rekor new: second entry not base64 (old shape read)", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		tl := rb["content"].(map[string]any)["verificationMaterial"].(map[string]any)
		tl["tlogEntries"] = append(tl["tlogEntries"].([]any), map[string]any{"canonicalizedBody": "!"})
	})})
	add("rekor new: no SET (old shape read)", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		delete(rekorNewEntry(rb), "inclusionPromise")
	})})
	add("rekor new: no entries", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rb["content"].(map[string]any)["verificationMaterial"].(map[string]any)["tlogEntries"] = []any{}
	})})
	add("rekor new: content a string", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rb["content"] = "x"
	})})
	add("rekor new and old shapes both, new read", std, imSpec{sig: "cosign", cosignTlog: "new", editSig: rekorEdit(func(rb map[string]any) {
		rb["SignedEntryTimestamp"] = b64([]byte("x"))
	})})
	add("rekor old: SET a number", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["SignedEntryTimestamp"] = 1
	})})
	add("rekor old: Payload a string", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["Payload"] = "x"
	})})
	add("rekor old: body a number", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["Payload"].(map[string]any)["body"] = 1
	})})
	add("rekor old: log ID a number at top", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["logID"] = 1
	})})
	add("rekor old: integrated time zero at top", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["integratedTime"] = 0
	})})
	add("rekor old: integrated time a fraction", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["Payload"].(map[string]any)["integratedTime"] = float64(t0.Unix()) + 0.75
	})})
	add("rekor old: log index a negative string", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["logIndex"] = "-4"
	})})
	add("rekor old: in other case", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: rekorEdit(func(rb map[string]any) {
		rb["signedentrytimestamp"] = rb["SignedEntryTimestamp"]
		delete(rb, "SignedEntryTimestamp")
		rb["PAYLOAD"] = rb["Payload"]
		delete(rb, "Payload")
	})})
	add("rekor old: top level an array", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.sigstore.cosign/bundle"] = "[]"
	}})
	add("rekor old: empty object", std, imSpec{sig: "cosign", cosignTlog: "old", editSig: func(m map[string]any) {
		layer0(m)["annotations"].(map[string]any)["dev.sigstore.cosign/bundle"] = "{}"
	}})

	if t.Failed() {
		return
	}
	file := map[string]any{"roots": roots, "cases": cases}
	b, err := json.MarshalIndent(file, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	verified := 0
	for _, c := range cases {
		if c.Error == "" {
			verified++
		}
	}
	t.Logf("%d cases: %d verified, %d refused", len(cases), verified, len(cases)-verified)
}

// ---------------------------------------------------------------- containerd/platforms

type plCase struct {
	Platform  imPlatform `json:"platform"`
	Normal    imPlatform `json:"normalized"`
	FormatAll string     `json:"formatAll"`
	Matches   string     `json:"matches"` // over the grid, 1 where Only(platform) matches it
	Less      string     `json:"less"`    // over the grid's pairs, row-major
}

func TestShardsPlatformsOracle(t *testing.T) {
	out := os.Getenv("SHARDS_PLATFORMS_OUT")
	if out == "" {
		t.Skip("SHARDS_PLATFORMS_OUT not set")
	}
	var grid []ocispecs.Platform
	for _, os := range []string{"linux", "Linux", "windows", "darwin", "macos"} {
		for _, av := range [][2]string{
			{"amd64", ""}, {"amd64", "v1"}, {"amd64", "v2"}, {"amd64", "v3"}, {"x86_64", ""}, {"x86-64", "v4"},
			{"386", ""}, {"i386", "v7"},
			{"arm64", ""}, {"arm64", "v8"}, {"arm64", "8"}, {"arm64", "v8.0"}, {"arm64", "v8.2"}, {"arm64", "v9"}, {"arm64", "9"}, {"arm64", "v9.0"}, {"arm64", "v9.3"}, {"arm64", "v10"}, {"aarch64", ""}, {"ARM64", "V8"},
			{"arm", ""}, {"arm", "v5"}, {"arm", "v6"}, {"arm", "v7"}, {"arm", "7"}, {"arm", "8"}, {"armhf", ""}, {"armel", ""}, {"arm", "v8"},
			{"riscv64", ""}, {"s390x", ""}, {"ppc64le", ""},
		} {
			if os != "linux" && av[0] != "amd64" && av[0] != "arm64" {
				continue
			}
			grid = append(grid, ocispecs.Platform{OS: os, Architecture: av[0], Variant: av[1]})
		}
	}
	grid = append(grid,
		ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"b", "a", "a"}},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"a"}},
		ocispecs.Platform{OS: "linux", Architecture: "amd64", OSFeatures: []string{"x+y", "(z)"}},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.17763.1"},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.20348.5"},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.26100.1"},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.22621.1", OSFeatures: []string{"win32k"}},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0"},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "x.y.z"},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", OSVersion: "5/6+(%)"},
		// An empty OS is the generator's own (runtime.GOOS), recorded as goos.
		ocispecs.Platform{Architecture: "arm64"},
		ocispecs.Platform{Architecture: "amd64", Variant: "v2"},
		ocispecs.Platform{OS: "darwin", Architecture: "arm64"},
		ocispecs.Platform{OS: "windows", Architecture: "amd64", OSVersion: "10.0.20348.1", OSFeatures: []string{"win32k", "a"}},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "v9.5"},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "v8.9"},
		ocispecs.Platform{OS: "linux", Architecture: "arm", Variant: "v9"},
		ocispecs.Platform{OS: "linux", Architecture: "amd64", Variant: "x"},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "9.0"},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", Variant: "v8.5"},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{"", "a"}},
		ocispecs.Platform{OS: "linux", Architecture: "arm64", OSFeatures: []string{""}},
	)
	var cases []plCase
	for _, p := range grid {
		only := platforms.Only(p)
		var m, l strings.Builder
		for _, c := range grid {
			if only.Match(c) {
				m.WriteByte('1')
			} else {
				m.WriteByte('0')
			}
		}
		for _, a := range grid {
			for _, b := range grid {
				if only.Less(a, b) {
					l.WriteByte('1')
				} else {
					l.WriteByte('0')
				}
			}
		}
		cases = append(cases, plCase{Platform: platformJSON(p), Normal: platformJSON(platforms.Normalize(p)), FormatAll: platforms.FormatAll(p), Matches: m.String(), Less: l.String()})
	}
	b, err := json.MarshalIndent(map[string]any{"goos": runtime.GOOS, "cases": cases}, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(b, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Logf("%d platforms", len(grid))
}

// ---------------------------------------------------------------- hashedrecordbundle.go, verbatim

const (
	annotationCert = "dev.sigstore.cosign/certificate"
	// annotationChain     = "dev.sigstore.cosign/chain"
	annotationSignature = "dev.cosignproject.cosign/signature"
	annotationBundle    = "dev.sigstore.cosign/bundle"
)

// hashedRecordSignedEntity implements verify.SignedEntity using cosign oldbundle format.
type hashedRecordSignedEntity struct {
	mfst  *ocispecs.Manifest
	cert  verify.VerificationContent
	sig   *messageSignatureContent
	isDHI bool
}

var _ verify.SignedEntity = &hashedRecordSignedEntity{}
var _ verify.SignatureContent = &hashedRecordSignedEntity{}
var _ verify.VerificationContent = &hashedRecordSignedEntity{}

func newHashedRecordSignedEntity(mfst *ocispecs.Manifest, isDHI bool) (verify.SignedEntity, error) {
	if len(mfst.Layers) == 0 {
		return nil, errors.New("no layers in manifest")
	}
	desc := mfst.Layers[0]
	sigStr, ok := desc.Annotations[annotationSignature]
	if !ok {
		return nil, errors.New("no signature annotation found")
	}
	sig, err := base64.StdEncoding.DecodeString(sigStr)
	if err != nil {
		return nil, errors.Wrapf(err, "decode signature")
	}
	dgstBytest, err := hex.DecodeString(desc.Digest.Hex())
	if err != nil {
		return nil, errors.Wrapf(err, "decode digest")
	}

	hr := &hashedRecordSignedEntity{
		mfst: mfst,
		sig: &messageSignatureContent{
			digest:          dgstBytest,
			signature:       sig,
			digestAlgorithm: desc.Digest.Algorithm().String(),
		},
		isDHI: isDHI,
	}

	if !isDHI {
		certData := desc.Annotations[annotationCert]
		if certData == "" {
			return nil, errors.Errorf("no certificate annotation found")
		}
		block, _ := pem.Decode([]byte(certData))
		if block == nil {
			return nil, errors.New("no PEM certificate found in annotation")
		}
		cert, err := x509.ParseCertificate(block.Bytes)
		if err != nil {
			return nil, errors.WithStack(err)
		}
		hr.cert = bundle.NewCertificate(cert)
	}

	return hr, nil
}

func (d *hashedRecordSignedEntity) HasInclusionPromise() bool {
	return true
}

func (d *hashedRecordSignedEntity) HasInclusionProof() bool {
	return true
}

func (d *hashedRecordSignedEntity) SignatureContent() (verify.SignatureContent, error) {
	return d, nil
}

func (d *hashedRecordSignedEntity) Timestamps() ([][]byte, error) {
	return nil, nil
}

func (d *hashedRecordSignedEntity) TlogEntries() ([]*tlog.Entry, error) {
	bundleBytes, ok := d.extractBundle()
	if !ok {
		return nil, nil
	}
	bundle, err := parseRekorBundle(bundleBytes)
	if err != nil {
		return nil, errors.Wrap(err, "parse rekor bundle")
	}
	logIDRaw, err := hex.DecodeString(bundle.LogID)
	if err != nil {
		return nil, errors.Wrap(err, "decode logID")
	}

	tl, err := tlog.NewTlogEntry(&v1.TransparencyLogEntry{
		LogIndex:          bundle.LogIndex,
		LogId:             &v1common.LogId{KeyId: logIDRaw},
		IntegratedTime:    bundle.IntegratedTime,
		CanonicalizedBody: bundle.Body,
		KindVersion: &v1.KindVersion{
			Kind:    "hashedrekord",
			Version: "0.0.1",
		},
		InclusionPromise: &v1.InclusionPromise{
			SignedEntryTimestamp: bundle.Signature,
		},
	})
	if err != nil {
		return nil, errors.Wrap(err, "create tlog entry")
	}
	return []*tlog.Entry{tl}, nil
}

func (d *hashedRecordSignedEntity) VerificationContent() (verify.VerificationContent, error) {
	return d, nil
}

func (d *hashedRecordSignedEntity) Version() (string, error) {
	return "v0.1", nil
}

func (d *hashedRecordSignedEntity) Signature() []byte {
	return d.sig.signature
}

func (d *hashedRecordSignedEntity) EnvelopeContent() verify.EnvelopeContent {
	return nil
}

func (d *hashedRecordSignedEntity) MessageSignatureContent() verify.MessageSignatureContent {
	return d.sig
}

type messageSignatureContent struct {
	digest          []byte
	digestAlgorithm string
	signature       []byte
}

func (m *messageSignatureContent) Digest() []byte {
	return m.digest
}

func (m *messageSignatureContent) DigestAlgorithm() string {
	return m.digestAlgorithm
}

func (m *messageSignatureContent) Signature() []byte {
	return m.signature
}

// CompareKey traces parameters and returns false.
func (d *hashedRecordSignedEntity) CompareKey(k any, tm root.TrustedMaterial) bool {
	if d.isDHI {
		return (&bundle.PublicKey{}).CompareKey(k, tm)
	}
	if _, ok := k.(*x509.Certificate); !ok {
		return false
	}
	return d.cert.CompareKey(k, tm)
}

func (d *hashedRecordSignedEntity) ValidAtTime(t time.Time, tm root.TrustedMaterial) bool {
	if d.isDHI {
		return (&bundle.PublicKey{}).ValidAtTime(t, tm)
	}
	return d.cert.ValidAtTime(t, tm)
}

func (d *hashedRecordSignedEntity) Certificate() *x509.Certificate {
	if d.isDHI {
		return nil
	}
	return d.cert.Certificate()
}

func (d *hashedRecordSignedEntity) Intermediates() []*x509.Certificate {
	if d.isDHI {
		return nil
	}
	return d.cert.Intermediates()
}

func (d *hashedRecordSignedEntity) PublicKey() verify.PublicKeyProvider {
	if d.isDHI {
		return bundle.PublicKey{}
	}
	return d.cert.PublicKey()
}

func (d *hashedRecordSignedEntity) extractBundle() ([]byte, bool) {
	if len(d.mfst.Layers) == 0 {
		return nil, false
	}
	desc := d.mfst.Layers[0]
	bundleStr := desc.Annotations[annotationBundle]
	if bundleStr == "" {
		return nil, false
	}
	return []byte(bundleStr), true
}

type rekorBundle struct {
	Body           []byte
	Signature      []byte
	LogID          string
	IntegratedTime int64
	LogIndex       int64
}

func parseRekorBundle(bundleBytes []byte) (*rekorBundle, error) {
	var nb struct {
		Content struct {
			VerificationMaterial struct {
				TlogEntries []struct {
					LogIndex any `json:"logIndex"`
					LogID    struct {
						KeyID string `json:"keyId"`
					} `json:"logId"`
					IntegratedTime   any `json:"integratedTime"`
					InclusionPromise struct {
						SignedEntryTimestamp []byte `json:"signedEntryTimestamp"`
					} `json:"inclusionPromise"`
					CanonicalizedBody []byte `json:"canonicalizedBody"`
				} `json:"tlogEntries"`
			} `json:"verificationMaterial"`
		} `json:"content"`
	}
	if json.Unmarshal(bundleBytes, &nb) == nil && len(nb.Content.VerificationMaterial.TlogEntries) > 0 {
		e := nb.Content.VerificationMaterial.TlogEntries[0]
		if len(e.CanonicalizedBody) != 0 && len(e.InclusionPromise.SignedEntryTimestamp) != 0 {
			b := &rekorBundle{
				Body:      e.CanonicalizedBody,
				Signature: e.InclusionPromise.SignedEntryTimestamp,
				LogID:     strings.ToLower(e.LogID.KeyID),
			}

			it, err1 := anyToInt64(e.IntegratedTime)
			if err1 == nil {
				b.IntegratedTime = it
			}
			li, err2 := anyToInt64(e.LogIndex)
			if err2 == nil {
				b.LogIndex = li
			}
			return b, nil
		}
	}

	// Fallback to older cosign bundle shape
	var bundle struct {
		SignedEntryTimestamp []byte `json:"SignedEntryTimestamp"`
		Payload              struct {
			Body           []byte `json:"body"`
			LogID          any    `json:"logID"`
			IntegratedTime any    `json:"integratedTime"`
			LogIndex       any    `json:"logIndex"`
		} `json:"Payload"`
		LogID          any `json:"logID"`
		IntegratedTime any `json:"integratedTime"`
		LogIndex       any `json:"logIndex"`
	}
	if err := json.Unmarshal(bundleBytes, &bundle); err != nil {
		return nil, errors.Wrap(err, "parse bundle json")
	}

	b := &rekorBundle{
		Body:      bundle.Payload.Body,
		Signature: bundle.SignedEntryTimestamp,
	}

	// Prefer top-level fields when present; otherwise fall back to nested under Payload
	// Handle string/number types
	if s, ok := bundle.LogID.(string); ok {
		b.LogID = s
	}
	if b.LogID == "" {
		if s, ok := bundle.Payload.LogID.(string); ok {
			b.LogID = s
		}
	}
	if v, err := anyToInt64(bundle.IntegratedTime); err == nil {
		b.IntegratedTime = v
	}
	if b.IntegratedTime == 0 {
		if v, err := anyToInt64(bundle.Payload.IntegratedTime); err == nil {
			b.IntegratedTime = v
		}
	}
	if v, err := anyToInt64(bundle.LogIndex); err == nil {
		b.LogIndex = v
	}
	if b.LogIndex == 0 {
		if v, err := anyToInt64(bundle.Payload.LogIndex); err == nil {
			b.LogIndex = v
		}
	}
	return b, nil
}

func anyToInt64(v any) (int64, error) {
	switch t := v.(type) {
	case nil:
		return 0, errors.New("nil")
	case float64:
		return int64(t), nil
	case json.Number:
		return t.Int64()
	case string:
		if t == "" {
			return 0, errors.New("empty")
		}
		return strconv.ParseInt(t, 10, 64)
	case int64:
		return t, nil
	case int:
		return int64(t), nil
	default:
		return 0, errors.Errorf("unsupported type %T", v)
	}
}
