package zz_shards_provenance

// What buildx v0.37.1 makes of an image's provenance: policy.SourceToInput over
// ResolveSourceMetaResponses whose attestation chains carry SLSA v0.2 and v1
// statements (parseProvenance, the materials as nested inputs), and
// policy.ResolveInputUnknowns resolving the materials' unknowns through a source
// resolver. Each case records json.Marshal of the input, its Unknowns(), the
// log lines, or the error; a panic is recorded as one. TestShardsProvenanceSchema
// records the Go types encoding/json decodes the statements into, as its
// typeFields sees them.
//
// Run by scripts/policy/generate-provenance inside a buildx v0.37.1 checkout.

import (
	"context"
	"encoding"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/containerd/platforms"
	"github.com/docker/buildx/policy"
	"github.com/moby/buildkit/client/llb/sourceresolver"
	gwpb "github.com/moby/buildkit/frontend/gateway/pb"
	provenancetypes "github.com/moby/buildkit/solver/llbsolver/provenance/types"
	"github.com/moby/buildkit/solver/pb"
	digest "github.com/opencontainers/go-digest"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
	"github.com/sirupsen/logrus"
)

const (
	slsa1  = "https://slsa.dev/provenance/v1"
	slsa02 = "https://slsa.dev/provenance/v0.2"
	ptKey  = "in-toto.io/predicate-type"
)

// ---- the schema ----

type inTotoStatement struct {
	PredicateType string          `json:"predicateType"`
	Predicate     json.RawMessage `json:"predicate"`
}

type schemaField struct {
	Name   string `json:"name"`
	Type   string `json:"type"`
	Quoted bool   `json:"quoted,omitempty"`
	// Through an unexported embedded pointer to a struct, which the decoder
	// cannot allocate.
	Unsettable bool `json:"unsettable,omitempty"`
}

type schemaType struct {
	Kind   string        `json:"kind"`
	Bits   int           `json:"bits,omitempty"`
	Elem   string        `json:"elem,omitempty"`
	Key    string        `json:"key,omitempty"`
	Len    int           `json:"len,omitempty"`
	Fields []schemaField `json:"fields,omitempty"`
}

var (
	unmarshalerType     = reflect.TypeFor[json.Unmarshaler]()
	textUnmarshalerType = reflect.TypeFor[encoding.TextUnmarshaler]()
	numberType          = reflect.TypeFor[json.Number]()
)

func typeID(t reflect.Type) string {
	if t.Name() != "" {
		return t.PkgPath() + "." + t.Name()
	}
	switch t.Kind() {
	case reflect.Pointer:
		return "*" + typeID(t.Elem())
	case reflect.Slice:
		return "[]" + typeID(t.Elem())
	case reflect.Array:
		return fmt.Sprintf("[%d]", t.Len()) + typeID(t.Elem())
	case reflect.Map:
		return "map[" + typeID(t.Key()) + "]" + typeID(t.Elem())
	}
	return t.String()
}

type schema map[string]schemaType

func (s schema) add(t reflect.Type) string {
	id := typeID(t)
	if _, ok := s[id]; ok {
		return id
	}
	s[id] = schemaType{Kind: "pending"}
	var st schemaType
	switch {
	case t == numberType:
		st = schemaType{Kind: "number"}
	case t.Kind() != reflect.Pointer && (t.Implements(unmarshalerType) || reflect.PointerTo(t).Implements(unmarshalerType)):
		st = schemaType{Kind: "custom"}
		if t.Kind() == reflect.Struct {
			// The type's own fields, which a decoder of an alias of it sees.
			st.Fields = s.fields(t)
		}
	case t.Kind() != reflect.Pointer && (t.Implements(textUnmarshalerType) || reflect.PointerTo(t).Implements(textUnmarshalerType)):
		st = schemaType{Kind: "text"}
	default:
		switch t.Kind() {
		case reflect.String:
			st = schemaType{Kind: "string"}
		case reflect.Bool:
			st = schemaType{Kind: "bool"}
		case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
			st = schemaType{Kind: "int", Bits: t.Bits()}
		case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
			st = schemaType{Kind: "uint", Bits: t.Bits()}
		case reflect.Float32, reflect.Float64:
			st = schemaType{Kind: "float", Bits: t.Bits()}
		case reflect.Interface:
			if t.NumMethod() == 0 {
				st = schemaType{Kind: "any"}
			} else {
				st = schemaType{Kind: "iface"}
			}
		case reflect.Pointer:
			st = schemaType{Kind: "ptr", Elem: s.add(t.Elem())}
		case reflect.Slice:
			st = schemaType{Kind: "slice", Elem: s.add(t.Elem())}
		case reflect.Array:
			st = schemaType{Kind: "array", Len: t.Len(), Elem: s.add(t.Elem())}
		case reflect.Map:
			k := t.Key()
			kind := "string"
			switch {
			case reflect.PointerTo(k).Implements(textUnmarshalerType):
				kind = "text"
			case k.Kind() == reflect.String:
			case k.Kind() >= reflect.Int && k.Kind() <= reflect.Int64:
				kind = fmt.Sprintf("int%d", k.Bits())
			case k.Kind() >= reflect.Uint && k.Kind() <= reflect.Uintptr:
				kind = fmt.Sprintf("uint%d", k.Bits())
			default:
				kind = "invalid"
			}
			st = schemaType{Kind: "map", Key: kind, Elem: s.add(t.Elem())}
		case reflect.Struct:
			st = schemaType{Kind: "struct", Fields: s.fields(t)}
		default:
			st = schemaType{Kind: "unsupported:" + t.Kind().String()}
		}
	}
	s[id] = st
	return id
}

// typeFields as encoding/json computes it (encode.go), for decoding.
type gofield struct {
	name       string
	tag        bool
	index      []int
	typ        reflect.Type
	quoted     bool
	unsettable bool
}

func (s schema) fields(t reflect.Type) []schemaField {
	type queued struct {
		typ        reflect.Type
		index      []int
		unsettable bool
	}
	current := []queued{}
	next := []queued{{typ: t}}
	var count, nextCount map[reflect.Type]int
	visited := map[reflect.Type]bool{}
	var fields []gofield
	for len(next) > 0 {
		current, next = next, current[:0]
		count, nextCount = nextCount, map[reflect.Type]int{}
		for _, f := range current {
			if visited[f.typ] {
				continue
			}
			visited[f.typ] = true
			for i := 0; i < f.typ.NumField(); i++ {
				sf := f.typ.Field(i)
				if sf.Anonymous {
					t := sf.Type
					if t.Kind() == reflect.Pointer {
						t = t.Elem()
					}
					if !sf.IsExported() && t.Kind() != reflect.Struct {
						continue
					}
				} else if !sf.IsExported() {
					continue
				}
				tag := sf.Tag.Get("json")
				if tag == "-" {
					continue
				}
				name, opts, _ := strings.Cut(tag, ",")
				if !isValidTag(name) {
					name = ""
				}
				index := append(slices.Clone(f.index), i)
				ft := sf.Type
				if ft.Name() == "" && ft.Kind() == reflect.Pointer {
					ft = ft.Elem()
				}
				quoted := false
				if slices.Contains(strings.Split(opts, ","), "string") {
					switch ft.Kind() {
					case reflect.Bool, reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64,
						reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr,
						reflect.Float32, reflect.Float64, reflect.String:
						quoted = true
					}
				}
				if name != "" || !sf.Anonymous || ft.Kind() != reflect.Struct {
					tagged := name != ""
					if name == "" {
						name = sf.Name
					}
					fields = append(fields, gofield{name: name, tag: tagged, index: index, typ: sf.Type, quoted: quoted, unsettable: f.unsettable})
					if count[f.typ] > 1 {
						fields = append(fields, fields[len(fields)-1])
					}
					continue
				}
				nextCount[ft]++
				if nextCount[ft] == 1 {
					uns := f.unsettable || (!sf.IsExported() && sf.Type.Kind() == reflect.Pointer)
					next = append(next, queued{typ: ft, index: index, unsettable: uns})
				}
			}
		}
	}
	slices.SortFunc(fields, func(a, b gofield) int {
		if c := strings.Compare(a.name, b.name); c != 0 {
			return c
		}
		if len(a.index) != len(b.index) {
			return len(a.index) - len(b.index)
		}
		if a.tag != b.tag {
			if a.tag {
				return -1
			}
			return 1
		}
		return slices.Compare(a.index, b.index)
	})
	out := fields[:0]
	for advance, i := 0, 0; i < len(fields); i += advance {
		fi := fields[i]
		for advance = 1; i+advance < len(fields); advance++ {
			if fields[i+advance].name != fi.name {
				break
			}
		}
		if advance == 1 {
			out = append(out, fi)
			continue
		}
		if len(fields[i].index) == len(fields[i+1].index) && fields[i].tag == fields[i+1].tag {
			continue
		}
		out = append(out, fi)
	}
	slices.SortFunc(out, func(a, b gofield) int { return slices.Compare(a.index, b.index) })
	var res []schemaField
	for _, f := range out {
		res = append(res, schemaField{Name: f.name, Type: s.add(f.typ), Quoted: f.quoted, Unsettable: f.unsettable})
	}
	return res
}

func isValidTag(s string) bool {
	if s == "" {
		return false
	}
	for _, c := range s {
		switch {
		case strings.ContainsRune("!#$%&()*+-./:;<=>?@[]^_{|}~ ", c):
		case !('a' <= c && c <= 'z' || 'A' <= c && c <= 'Z' || '0' <= c && c <= '9' || c > 127):
			return false
		}
	}
	return true
}

func TestShardsProvenanceSchema(t *testing.T) {
	s := schema{}
	roots := map[string]string{
		"statement": s.add(reflect.TypeFor[inTotoStatement]()),
		"slsa1":     s.add(reflect.TypeFor[provenancetypes.ProvenancePredicateSLSA1]()),
		"slsa02":    s.add(reflect.TypeFor[provenancetypes.ProvenancePredicateSLSA02]()),
	}
	// The custom decoders' own types (pb/json.go): what each decodes into first.
	aux := map[string]string{
		"github.com/moby/buildkit/solver/pb.Op":         s.add(reflect.TypeOf(jsonOp{})),
		"github.com/moby/buildkit/solver/pb.FileAction": s.add(reflect.TypeOf(jsonFileAction{})),
		"github.com/moby/buildkit/solver/pb.UserOpt":    s.add(reflect.TypeOf(jsonUserOpt{})),
	}
	out, err := json.MarshalIndent(map[string]any{"roots": roots, "custom": aux, "types": s}, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_PROVENANCE_SCHEMA"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// pb/json.go's unexported decoding types, as declared there.
type jsonOp struct {
	Inputs []*pb.Input `json:"inputs,omitempty"`
	Op     struct {
		Exec   *pb.ExecOp        `json:"exec,omitempty"`
		Source *pb.SourceOp      `json:"source,omitempty"`
		File   *pb.FileOp        `json:"file,omitempty"`
		Build  *pb.BuildOp       `json:"build,omitempty"`
		Merge  *pb.MergeOp       `json:"merge,omitempty"`
		Diff   *pb.DiffOp        `json:"diff,omitempty"`
		Pass   *pb.PassthroughOp `json:"passthrough,omitempty"`
	}
	Platform    *pb.Platform          `json:"platform,omitempty"`
	Constraints *pb.WorkerConstraints `json:"constraints,omitempty"`
}

type jsonFileAction struct {
	Input          pb.InputIndex  `json:"input"`
	SecondaryInput pb.InputIndex  `json:"secondaryInput"`
	Output         pb.OutputIndex `json:"output"`
	Action         struct {
		Copy   *pb.FileActionCopy   `json:"copy,omitempty"`
		Mkfile *pb.FileActionMkFile `json:"mkfile,omitempty"`
		Mkdir  *pb.FileActionMkDir  `json:"mkdir,omitempty"`
		Rm     *pb.FileActionRm     `json:"rm,omitempty"`
	}
}

type jsonUserOpt struct {
	User struct {
		ByName *pb.NamedUserOpt `json:"byName,omitempty"`
		ByID   uint32           `json:"byId,omitempty"`
	}
}

// ---- the cases ----

type blobCase struct {
	Key         string            `json:"key"`
	MediaType   string            `json:"mediaType,omitempty"`
	Annotations map[string]string `json:"annotations,omitempty"`
	// The blob's bytes, base64, or the file under testdata/policy that holds them.
	Data     string `json:"data,omitempty"`
	DataFile string `json:"dataFile,omitempty"`
	NoDesc   bool   `json:"noDescriptor,omitempty"`
}

type chainCase struct {
	AttestationManifest string     `json:"attestationManifest,omitempty"`
	Blobs               []blobCase `json:"blobs,omitempty"`
}

type imageCase struct {
	Digest string     `json:"digest"`
	Config string     `json:"config,omitempty"`
	Chain  *chainCase `json:"chain,omitempty"`
}

type sourceCase struct {
	Identifier string            `json:"identifier"`
	Attrs      map[string]string `json:"attrs,omitempty"`
}

type platformCase struct {
	OS      string `json:"os"`
	Arch    string `json:"arch"`
	Variant string `json:"variant,omitempty"`
}

type metaCase struct {
	Source sourceCase `json:"source"`
	Image  *imageCase `json:"image,omitempty"`
}

type resolverReply struct {
	Image *imageCase `json:"image,omitempty"`
	Error string     `json:"error,omitempty"`
}

type resolveCase struct {
	Unknowns []string `json:"unknowns"`
	// Replies by source identifier; an identifier without one is an error.
	Replies map[string]resolverReply `json:"replies,omitempty"`
}

type requestRecord struct {
	Source              string        `json:"source"`
	Platform            *platformCase `json:"platform,omitempty"`
	Image               bool          `json:"image,omitempty"`
	NoConfig            bool          `json:"noConfig,omitempty"`
	AttestationChain    bool          `json:"attestationChain,omitempty"`
	ResolveAttestations []string      `json:"resolveAttestations,omitempty"`
	Git                 bool          `json:"git,omitempty"`
	ReturnObject        bool          `json:"returnObject,omitempty"`
	HTTP                bool          `json:"http,omitempty"`
}

type resolveRecord struct {
	Retry    bool            `json:"retry,omitempty"`
	Next     *requestRecord  `json:"next,omitempty"`
	Error    string          `json:"error,omitempty"`
	Calls    []requestRecord `json:"calls,omitempty"`
	// json.Marshal's bytes, as a string.
	Input    string   `json:"input,omitempty"`
	Unknowns []string `json:"unknowns,omitempty"`
	Logs     []string `json:"logs,omitempty"`
}

type record struct {
	Name     string          `json:"name"`
	Meta     metaCase        `json:"meta"`
	Platform *platformCase   `json:"platform,omitempty"`
	Input    string        `json:"input,omitempty"`
	Unknowns []string      `json:"unknowns,omitempty"`
	Error    string        `json:"error,omitempty"`
	Logs     []string        `json:"logs,omitempty"`
	Resolve  *resolveCase    `json:"resolve,omitempty"`
	Resolved *resolveRecord  `json:"resolved,omitempty"`
}

// esc writes ~u as a JSON escape's \\u.
func esc(s string) string { return strings.ReplaceAll(s, "~u", string(rune(92))+"u") }

func b64(s string) string { return base64.StdEncoding.EncodeToString([]byte(s)) }

func stmt(pt, predicate string) string {
	return `{"_type":"https://in-toto.io/Statement/v0.1","predicateType":"` + pt + `","subject":[],"predicate":` + predicate + `}`
}

func slsaBlob(key, pt, data string) blobCase {
	return blobCase{Key: key, MediaType: "application/vnd.in-toto+json", Annotations: map[string]string{ptKey: pt}, Data: b64(data)}
}

const dgA = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
const dgB = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
const hexC = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
const dgC = "sha256:" + hexC
const attMan = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

func imageWith(blobs ...blobCase) *imageCase {
	return &imageCase{Digest: dgA, Chain: &chainCase{AttestationManifest: attMan, Blobs: blobs}}
}

func one(pt, predicate string) *imageCase {
	return imageWith(slsaBlob("sha256:1111111111111111111111111111111111111111111111111111111111111111", pt, stmt(pt, predicate)))
}

func raw(pt, data string) *imageCase {
	return imageWith(slsaBlob("sha256:1111111111111111111111111111111111111111111111111111111111111111", pt, data))
}

// A v1 predicate with the given resolved dependencies.
func v1With(deps string) string {
	return `{"buildDefinition":{"buildType":"https://mobyproject.org/buildkit@v1","externalParameters":{"configSource":{},"request":{"frontend":"dockerfile.v0"}},"internalParameters":{"builderPlatform":"linux/arm64"},"resolvedDependencies":` + deps + `},"runDetails":{"builder":{"id":"b"}}}`
}

func dep(uri, digests string) string {
	return `{"uri":` + strings.ReplaceAll(fmt.Sprintf("%q", uri), `\\`, `\`) + `,"digest":{` + digests + `}}`
}

func jsonStr(s string) string {
	b, _ := json.Marshal(s)
	return string(b)
}

func depJ(uri, digests string) string { return `{"uri":` + jsonStr(uri) + `,"digest":{` + digests + `}}` }

func nest(n int, inner string) string {
	return strings.Repeat("[", n) + inner + strings.Repeat("]", n)
}

var arm64 = &platformCase{OS: "linux", Arch: "arm64"}

func cases() []record {
	img := sourceCase{Identifier: "docker-image://docker.io/library/alpine:3.20"}
	var out []record
	add := func(name string, im *imageCase) {
		out = append(out, record{Name: name, Meta: metaCase{Source: img, Image: im}, Platform: arm64})
	}

	fullV1 := `{
	 "buildDefinition": {
	  "buildType": "https://github.com/moby/buildkit/blob/master/docs/attestations/slsa-definitions.md",
	  "externalParameters": {
	   "configSource": {"uri": "https://github.com/moby/buildkit.git#refs/tags/v0.28.1", "digest": {"sha1": "39d4f985e9f126e7821acaa920cfb460e185d4a1", "sha256": "` + hexC + `"}, "path": "Dockerfile"},
	   "request": {"frontend": "gateway.v0", "args": {"build-arg:A": "1", "build-arg:": "empty", "build-arg:B<&>": "x", "cmdline": "docker/dockerfile:1", "label:x": "y"}, "secrets": [{"id": "s", "optional": true}], "ssh": [{"id": "default"}], "locals": [{"name": "context"}], "compatibilityVersion": 2}
	  },
	  "internalParameters": {"builderPlatform": "linux/arm64", "dockerfileVersion": "1.27.1", "custom": {"a": [1, 2.5, -3e5, true, null, "s"]}},
	  "resolvedDependencies": [
	   ` + depJ("pkg:docker/alpine@3.20?platform=linux%2Farm64", `"sha256":"`+hexC+`"`) + `,
	   ` + depJ("pkg:docker/docker/dockerfile@1", `"sha256":"`+dgB+`"`) + `,
	   ` + depJ("pkg:docker/library/golang@1.25-alpine?platform=linux%2Famd64", ``) + `,
	   ` + depJ("pkg:docker/ghcr.io/foo/bar@v1.0?platform=linux%2Farm%2Fv7", `"sha256":"`+hexC+`"`) + `,
	   ` + depJ("https://github.com/moby/buildkit.git#refs/tags/v0.28.1", `"sha1":"39d4f985e9f126e7821acaa920cfb460e185d4a1"`) + `,
	   ` + depJ("https://example.com/file.tar.gz?x=1&y=2", `"sha256":"`+hexC+`"`) + `,
	   ` + depJ("git://github.com/a/b", ``) + `,
	   ` + depJ("ftp://example.com/x", ``) + `
	  ]
	 },
	 "runDetails": {
	  "builder": {"id": "https://github.com/docker/buildx", "version": {"buildkit": "v0.28.1"}},
	  "metadata": {
	   "invocationId": "inv1",
	   "startedOn": "2026-03-25T13:39:02.690932831Z",
	   "finishedOn": "2026-03-25T15:41:10.35+02:00",
	   "buildkit_metadata": {"vcs": {"source": "x"}},
	   "buildkit_hermetic": true,
	   "buildkit_completeness": {"request": true, "resolvedDependencies": false},
	   "buildkit_reproducible": false
	  }
	 }
	}`
	add("v1 full", one(slsa1, fullV1))

	fullV02 := `{
	 "builder": {"id": "https://example.com/builder"},
	 "buildType": "https://mobyproject.org/buildkit@v1",
	 "materials": [
	  {"uri": "pkg:docker/alpine@3.20?platform=linux%2Farm64", "digest": {"sha256": "` + hexC + `"}},
	  {"uri": "https://github.com/a/b.git#main:sub/dir", "digest": {"sha1": "abc"}},
	  {"uri": "git@github.com:a/b.git", "digest": {}},
	  {"uri": "local://context"}
	 ],
	 "invocation": {
	  "configSource": {"uri": "https://github.com/a/b.git", "digest": {"sha1": "abc"}, "entryPoint": "Dockerfile"},
	  "parameters": {"frontend": "dockerfile.v0", "args": {"build-arg:FOO": "bar", "target": "t"}, "locals": [{"name": "context"}]},
	  "environment": {"platform": "linux/arm64"}
	 },
	 "buildConfig": {"llbDefinition": [{"id": "step0", "op": {"Op": {"source": {"identifier": "docker-image://docker.io/library/alpine:3.20"}}, "platform": {"Architecture": "arm64", "OS": "linux"}}}]},
	 "metadata": {
	  "buildInvocationID": "inv02",
	  "buildStartedOn": "2024-02-29T23:59:59.999999999-00:30",
	  "buildFinishedOn": "2024-01-01T00:00:00Z",
	  "completeness": {"parameters": true, "environment": true, "materials": false},
	  "reproducible": true,
	  "https://mobyproject.org/buildkit@v1#metadata": {},
	  "https://mobyproject.org/buildkit@v1#hermetic": true
	 }
	}`
	add("v0.2 full", one(slsa02, fullV02))

	add("v1 builder only", one(slsa1, `{"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 buildType only", one(slsa1, `{"buildDefinition":{"buildType":"t"}}`))
	add("v1 neither", one(slsa1, `{"buildDefinition":{"externalParameters":{"request":{"frontend":"f"}}},"runDetails":{}}`))
	add("v0.2 neither", one(slsa02, `{"invocation":{"parameters":{"frontend":"f"}}}`))
	add("v1 metadata null", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":null}}`))
	add("v1 metadata then null", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"invocationId":"x"},"metadata":null}}`))
	add("v1 metadata empty", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{}}}`))
	add("v1 metadata merged", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"invocationId":"x","buildkit_hermetic":true},"metadata":{"startedOn":"2020-01-01T00:00:00Z"}}}`))
	add("v0.2 metadata empty", one(slsa02, `{"buildType":"t","metadata":{}}`))
	add("v1 buildType number", one(slsa1, `{"buildDefinition":{"buildType":1},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 builder string", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":"b"}}`))
	add("v1 args not strings", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"args":{"a":1}}}}}`))
	add("v1 args merged", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"args":{"build-arg:a":"1","build-arg:b":"2"}},"request":{"args":{"build-arg:a":"3"}}}}}`))
	add("v1 args null", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"args":{"build-arg:a":"1"},"args":null}}}}`))
	add("v1 args empty", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"args":{}}}}}`))
	add("v1 args no build-arg", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"args":{"build-arg:":"x","target":"y"}}}}}`))
	add("v1 config digest merged", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"configSource":{"digest":{"sha1":"a"}},"configSource":{"uri":"u","digest":{"sha256":"b"}}}}}`))
	add("v1 config digest empty", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"configSource":{"digest":{}}}}}`))
	add("v1 folded keys", one(slsa1, `{"BUILDDEFINITION":{"BuildType":"t","externalParameterſ":{"Request":{"FRONTEND":"f"}}},"runDetails":{"builder":{"ID":"b"},"metadata":{"buildKit_hermetic":true,"buıldkit_reproducible":true,"INVOCATIONID":"i","invİcationId":"dotted"}}}`))
	add("v1 escaped keys", one(slsa1, esc(`{"~u0062uildDefinition":{"buildType":"~u0074~ud83d~ude00~ud800x~udc00~udc00~ud800y"},"runDetails":{"builder":{"id":"b"}}}`)))
	add("v1 exact beats folded", one(slsa1, `{"buildDefinition":{"buildType":"exact","BUILDTYPE":"folded"},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 later wins", one(slsa1, `{"buildDefinition":{"BUILDTYPE":"folded","buildType":"exact","buildtype":"last"},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 invalid utf8", raw(slsa1, "{\"predicateType\":\""+slsa1+"\",\"predicate\":{\"buildDefinition\":{\"buildType\":\"a\xffb\xf0\x90\x80c\xed\xa0\x80d\"},\"runDetails\":{\"builder\":{\"id\":\"\xc3\"}}}}"))
	add("v1 control in string", raw(slsa1, "{\"predicateType\":\""+slsa1+"\",\"predicate\":{\"buildDefinition\":{\"buildType\":\"a\x01b\"}}}"))
	add("v1 html in strings", one(slsa1, esc(`{"buildDefinition":{"buildType":"<a>&~u2028\u2029~u003c\/\b\f\n\r\t~u0000"},"runDetails":{"builder":{"id":"b"}}}`)))

	// Times.
	timeCase := func(name, v string) {
		add("v1 time "+name, one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"startedOn":`+v+`}}}`))
	}
	timeCase("utc", `"2026-03-25T13:39:02Z"`)
	timeCase("offset", `"2026-03-25T01:00:00+05:30"`)
	timeCase("negative offset day", `"2026-12-31T22:00:00-03:00"`)
	timeCase("fraction", `"2026-03-25T13:39:02.123456789123Z"`)
	timeCase("comma fraction", `"2026-03-25T13:39:02,5Z"`)
	timeCase("one digit hour", `"2026-03-25T1:39:02Z"`)
	timeCase("offset 24", `"2026-03-25T13:39:02+24:00"`)
	timeCase("offset minutes 60", `"2026-03-25T13:39:02+01:60"`)
	timeCase("leap day", `"2024-02-29T00:00:00Z"`)
	timeCase("bad leap day", `"2023-02-29T00:00:00Z"`)
	timeCase("second 60", `"2026-03-25T13:39:60Z"`)
	timeCase("lowercase t", `"2026-03-25t13:39:02Z"`)
	timeCase("year zero back", `"0000-01-01T00:30:00+01:00"`)
	timeCase("year 9999 forward", `"9999-12-31T23:30:00-01:00"`)
	timeCase("escaped", esc(`"2026~u002d03-25T13:39:02Z"`))
	timeCase("null", `null`)
	timeCase("number", `1`)
	timeCase("empty", `""`)
	timeCase("no zone", `"2026-03-25T13:39:02"`)
	add("v0.2 time offset", one(slsa02, `{"buildType":"t","metadata":{"buildStartedOn":"2026-03-25T01:00:00+05:30","buildFinishedOn":null}}`))
	add("v0.2 time bad", one(slsa02, `{"buildType":"t","metadata":{"buildStartedOn":"yesterday"}}`))

	// Fields elsewhere in the types, which only fail the decoding.
	add("v1 compatibilityVersion float", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"compatibilityVersion":1.5}}}}`))
	add("v1 compatibilityVersion exp", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"compatibilityVersion":1e2}}}}`))
	add("v1 compatibilityVersion big", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"compatibilityVersion":9223372036854775808}}}}`))
	add("v1 compatibilityVersion negative zero", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"compatibilityVersion":-0}}}}`))
	add("v1 custom env huge number", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"x":{"y":[1e400]}}}}`))
	add("v1 custom env tiny number", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"x":1e-400}}}`))
	add("v1 builderPlatform number", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"builderPlatform":1}}}`))
	add("v1 content base64", one(slsa1, v1With(`[{"uri":"https://a/b","content":"aGVs\r\nbG8="}]`)))
	add("v1 content bad base64", one(slsa1, v1With(`[{"uri":"https://a/b","content":"!!!"}]`)))
	add("v1 content array", one(slsa1, v1With(`[{"uri":"https://a/b","content":[1,2,255]}]`)))
	add("v1 content array overflow", one(slsa1, v1With(`[{"uri":"https://a/b","content":[256]}]`)))
	add("v1 annotations any", one(slsa1, v1With(`[{"uri":"https://a/b","annotations":{"a":{"b":[1,"x",null]}}}]`)))
	add("v1 secrets not array", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"secrets":{}}}}}`))
	add("v1 secrets null element", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"secrets":[null,{"id":"x"}]}}}}`))
	add("v1 inputs recursive", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"inputs":{"a":{"request":{"inputs":{"b":{"configSource":{"uri":"x"},"request":{"frontend":1}}}}}}}}}}`))
	add("v1 inputs recursive ok", one(slsa1, `{"buildDefinition":{"buildType":"t","externalParameters":{"request":{"inputs":{"a":{"request":{"inputs":{"b":{"configSource":{"uri":"x"},"request":{"frontend":"f"}}}}}}}}}}`))
	add("v1 llb op exec", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"id":"s","op":{"Op":{"exec":{"meta":{"args":["sh"],"user":"root"},"mounts":[{"input":0,"dest":"/","output":0}]}},"inputs":[{"digest":"sha256:x","index":0}]}}],"digestMapping":{"sha256:x":"step0"}}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 llb op bad", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"id":"s","op":{"Op":{"exec":{"meta":{"args":"sh"}}}}}]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 llb op string", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"id":"s","op":"x"}]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 llb file action", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"op":{"Op":{"file":{"actions":[{"input":0,"secondaryInput":-1,"output":0,"Action":{"mkdir":{"path":"/x","mode":493,"makeParents":true,"owner":{"user":{"User":{"byName":{"name":"u"}}}}}}}]}}}}]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 llb file action bad owner", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"op":{"Op":{"file":{"actions":[{"Action":{"mkdir":{"owner":{"user":{"User":{"byId":-1}}}}}}]}}}}]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 llb op null", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"op":null},null]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 source infos data", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"buildkit_metadata":{"source":{"infos":[{"filename":"Dockerfile","data":"RlJPTSBhbHBpbmU=","language":"Dockerfile"}],"locations":{"step0":{"locations":[{"ranges":[{"start":{"line":1},"end":{"line":1}}]}]}}},"layers":{"step0:0":[[{"mediaType":"m","digest":"sha256:x","size":1}]]},"sysUsage":[{"cpuStat":{"usage_usec":1}}]}}}}`))
	add("v1 source infos bad data", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"buildkit_metadata":{"source":{"infos":[{"data":"=="}]}}}}}`))
	add("v1 resource usage", one(slsa1, `{"buildDefinition":{"buildType":"t","internalParameters":{"buildConfig":{"llbDefinition":[{"id":"s","resourceUsage":{"cpuStat":{"usage_nanos":1},"memoryStat":{"total":2},"sysCPUStat":{"user":1.5}}}]}}},"runDetails":{"builder":{"id":"b"}}}`))
	add("v1 network metadata", one(slsa1, `{"buildDefinition":{"buildType":"t"},"runDetails":{"builder":{"id":"b"},"metadata":{"buildkit_metadata":{"network":{"mode":"proxy","proxy":{"incomplete":[{"op":"x","reason":"y"}]}}}}}}`))

	// Statements.
	add("statement type from annotation", raw(slsa1, `{"predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement type wins over annotation", raw(slsa1, `{"predicateType":"`+slsa02+`","predicate":{"buildType":"v02","buildDefinition":{"buildType":"v1"}}}`))
	add("statement type unknown", raw(slsa1, `{"predicateType":"https://spdx.dev/Document","predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement no predicate", raw(slsa1, `{"predicateType":"`+slsa1+`"}`))
	add("statement predicate null", raw(slsa1, `{"predicateType":"`+slsa1+`","predicate":null}`))
	add("statement predicate string", raw(slsa1, `{"predicateType":"`+slsa1+`","predicate":"x"}`))
	add("statement predicate array", raw(slsa1, `{"predicateType":"`+slsa1+`","predicate":[]}`))
	add("statement type number", raw(slsa1, `{"predicateType":1,"predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement broken", raw(slsa1, `{"predicateType":"`+slsa1+`","predicate":{"buildDefinition":{"buildType":"t"}}`))
	add("statement trailing", raw(slsa1, `{"predicateType":"`+slsa1+`","predicate":{"buildDefinition":{"buildType":"t"}}} x`))
	add("statement trailing space", raw(slsa1, " \t\r\n{\"predicateType\":\""+slsa1+"\",\"predicate\":{\"buildDefinition\":{\"buildType\":\"t\"}}} \n"))
	add("statement bom", raw(slsa1, "\xef\xbb\xbf{\"predicateType\":\""+slsa1+"\",\"predicate\":{\"buildDefinition\":{\"buildType\":\"t\"}}}"))
	add("statement array", raw(slsa1, `[]`))
	add("statement leading zero", raw(slsa1, `{"x":01,"predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement bad escape", raw(slsa1, `{"x":"\x","predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement bad literal", raw(slsa1, `{"x":tru,"predicate":{"buildDefinition":{"buildType":"t"}}}`))
	add("statement depth 10000", raw(slsa1, `{"predicate":{"buildDefinition":{"buildType":"t"},"x":`+nest(9998, "1")+`}}`))
	add("statement depth 10001", raw(slsa1, `{"predicate":{"buildDefinition":{"buildType":"t"},"x":`+nest(9999, "1")+`}}`))
	add("predicate deep any", raw(slsa1, `{"predicate":{"buildDefinition":{"buildType":"t","internalParameters":{"x":`+nest(9990, "1e999")+`}}}}`))

	// The chain.
	add("no chain", &imageCase{Digest: dgA})
	add("chain no manifest no blobs", &imageCase{Digest: dgA, Chain: &chainCase{}})
	add("chain manifest no blobs", &imageCase{Digest: dgA, Chain: &chainCase{AttestationManifest: attMan}})
	add("chain no manifest provenance", &imageCase{Digest: dgA, Chain: &chainCase{Blobs: []blobCase{slsaBlob("sha256:2222222222222222222222222222222222222222222222222222222222222222", slsa1, stmt(slsa1, `{"buildDefinition":{"buildType":"t"}}`))}}})
	add("chain no manifest broken provenance", &imageCase{Digest: dgA, Chain: &chainCase{Blobs: []blobCase{slsaBlob("sha256:2222222222222222222222222222222222222222222222222222222222222222", slsa1, `{`)}}})
	add("blob without annotation", imageWith(blobCase{Key: "sha256:1111111111111111111111111111111111111111111111111111111111111111", Data: b64(stmt(slsa1, `{"buildDefinition":{"buildType":"t"}}`))}))
	add("blob sbom annotation", imageWith(slsaBlob("sha256:1111111111111111111111111111111111111111111111111111111111111111", "https://spdx.dev/Document", stmt(slsa1, `{"buildDefinition":{"buildType":"t"}}`))))
	add("blob empty", imageWith(blobCase{Key: "sha256:1111111111111111111111111111111111111111111111111111111111111111", Annotations: map[string]string{ptKey: slsa1}}))
	add("blob no descriptor", imageWith(blobCase{Key: "sha256:1111111111111111111111111111111111111111111111111111111111111111", NoDesc: true, Data: b64(stmt(slsa1, `{"buildDefinition":{"buildType":"t"}}`))}))
	add("blob first broken second ok", imageWith(
		slsaBlob("sha256:1111111111111111111111111111111111111111111111111111111111111111", slsa1, `{"predicate":`),
		slsaBlob("sha256:2222222222222222222222222222222222222222222222222222222222222222", slsa02, stmt(slsa02, `{"buildType":"second"}`)),
	))
	add("blob first neither second ok", imageWith(
		slsaBlob("sha256:1111111111111111111111111111111111111111111111111111111111111111", slsa1, stmt(slsa1, `{}`)),
		slsaBlob("sha256:2222222222222222222222222222222222222222222222222222222222222222", slsa1, stmt(slsa1, `{"runDetails":{"builder":{"id":"second"}}}`)),
	))
	add("image with config", &imageCase{Digest: dgA, Config: b64(`{"created":"2024-01-01T00:00:00Z","config":{"User":"u","Labels":{"a":"b"}}}`), Chain: &chainCase{AttestationManifest: attMan}})

	// An image's config, as json.Unmarshal reads it into an ocispecs.Image: keys folded,
	// repeated keys merged, slices filled in place, nulls, every field's type checked, a
	// time read from its text as written. At most one volume each: Go's map order is
	// random.
	cfg := func(name, config string) {
		add("config "+name, &imageCase{Digest: dgA, Config: b64(esc(config)), Chain: &chainCase{AttestationManifest: attMan}})
	}
	cfg("keys folded", `{"CREATED":"2024-01-01T00:00:00Z","Config":{"ENV":["A=1"],"labels":{"a":"b"},"user":"u","workingdir":"/w","volumes":{"/v":{}}}}`)
	cfg("keys folded from non-ASCII", `{"config":{"Wor~u212aingDir":"/k","Label~u017f":{"a":"b"}}}`)
	cfg("key unfolded non-ASCII", `{"config":{"Usér":"u"}}`)
	cfg("env exact then folded", `{"config":{"Env":[],"env":["X"]}}`)
	cfg("env folded then exact", `{"config":{"env":["X"],"Env":["Y"]}}`)
	cfg("config repeated merges", `{"config":{"User":"u"},"config":{"WorkingDir":"/w"}}`)
	cfg("labels merged", `{"config":{"Labels":{"a":"1"},"Labels":{"b":"2"},"labels":{"a":"3"}}}`)
	cfg("labels null clears", `{"config":{"Labels":{"a":"1"},"Labels":null,"Labels":{"b":"2"}}}`)
	cfg("label null", `{"config":{"Labels":{"a":null}}}`)
	cfg("labels empty", `{"config":{"Labels":{}}}`)
	cfg("env null element", `{"config":{"Env":["A",null]}}`)
	cfg("env shorter later", `{"config":{"Env":["A","B"],"Env":["C"]}}`)
	cfg("env reused in place", `{"config":{"Env":["A","B","C"],"Env":["X"],"Env":[null,null,null]}}`)
	cfg("env grown past capacity", `{"config":{"Env":["A","B","C"],"Env":[null,null,null,null,null]}}`)
	cfg("env long then nulls", `{"config":{"Env":[`+strings.Repeat(`"e",`, 32)+`"e"],"Env":["x"],"Env":[`+strings.Repeat(`null,`, 71)+`null]}}`)
	cfg("env emptied then null", `{"config":{"Env":["A"],"Env":[],"Env":[null]}}`)
	cfg("env null", `{"config":{"Env":["A"],"Env":null}}`)
	cfg("user null kept", `{"config":{"User":"u","User":null}}`)
	cfg("volume null", `{"config":{"Volumes":{"/v":null}}}`)
	cfg("volume repeated", `{"config":{"Volumes":{"/v":{}},"volumes":{"/v":{"x":[1]}}}}`)
	cfg("volumes null clears", `{"config":{"Volumes":{"/v":{}},"Volumes":null}}`)
	cfg("volumes empty", `{"config":{"Volumes":{}}}`)
	cfg("created offset", `{"created":"2024-01-01T10:00:00+02:00"}`)
	cfg("created fraction", `{"created":"2024-01-01T10:00:00.123456789Z"}`)
	cfg("created null", `{"created":"2024-01-01T00:00:00Z","created":null}`)
	cfg("created repeated", `{"created":"2024-01-01T00:00:00Z","Created":"2025-01-01T00:00:00Z"}`)
	cfg("created escaped", `{"created":"2024-01-01T00:00:00~u005a"}`)
	cfg("created not UTF-8", "{\"created\":\"2024-01-01T00:00:00\xffZ\"}")
	cfg("created bad", `{"created":"2024-13-01T00:00:00Z"}`)
	cfg("created number", `{"created":5}`)
	cfg("created array", `{"created":[]}`)
	cfg("history created bad", `{"history":[{"created":"x"}]}`)
	cfg("history created null", `{"history":[{"created":null,"empty_layer":true}]}`)
	cfg("history wrong element", `{"history":[1]}`)
	cfg("history wrong", `{"history":"x"}`)
	cfg("history field wrong", `{"history":[{"comment":1}]}`)
	cfg("time error over earlier type error", `{"config":{"Env":5},"created":"x"}`)
	cfg("first time error", `{"history":[{"created":"a"}],"created":"b"}`)
	cfg("first type error kept", `{"config":{"User":1,"Env":"x"}}`)
	cfg("type error after values", `{"config":{"User":"u","Env":["A"],"WorkingDir":true}}`)
	cfg("env wrong", `{"config":{"Env":"A=1"}}`)
	cfg("env element wrong", `{"config":{"Env":[1]}}`)
	cfg("env object", `{"config":{"Env":{}}}`)
	cfg("label wrong", `{"config":{"Labels":{"a":1}}}`)
	cfg("labels wrong", `{"config":{"Labels":[]}}`)
	cfg("volume wrong", `{"config":{"Volumes":{"/v":"x"}}}`)
	cfg("volume array", `{"config":{"Volumes":{"/v":[]}}}`)
	cfg("volumes wrong", `{"config":{"Volumes":"x"}}`)
	cfg("exposed ports wrong", `{"config":{"Expo~u017fedPorts":5}}`)
	cfg("cmd wrong", `{"config":{"Cmd":"x"}}`)
	cfg("entrypoint element wrong", `{"config":{"Entrypoint":[true]}}`)
	cfg("stop signal wrong", `{"config":{"StopSignal":9}}`)
	cfg("args escaped wrong", `{"config":{"ArgsEscaped":"true"}}`)
	cfg("user wrong", `{"config":{"User":["u"]}}`)
	cfg("config wrong", `{"config":"x"}`)
	cfg("config null", `{"config":null}`)
	cfg("author wrong", `{"author":{}}`)
	cfg("architecture wrong", `{"architecture":5}`)
	cfg("os.features wrong", `{"os.features":"x"}`)
	cfg("variant wrong", `{"variant":false}`)
	cfg("rootfs wrong", `{"rootfs":"x"}`)
	cfg("rootfs type wrong", `{"rootfs":{"type":1}}`)
	cfg("diff_ids wrong", `{"rootfs":{"diff_ids":"x"}}`)
	cfg("diff_id wrong", `{"rootfs":{"diff_ids":[1]}}`)
	cfg("unknown fields any type", `{"x":[1,{"y":null}],"config":{"Healthcheck":5,"Shell":"x"}}`)
	cfg("document null", `null`)
	cfg("document array", `[]`)
	cfg("document string", `"x"`)
	cfg("syntax error", `{"config":{"Env":5},`)
	cfg("syntax error late", `{"created":"x","config":{}} x`)
	cfg("bad escape", `{"config":{"User":"~uZZZZ"}}`)
	cfg("depth 10000", `{"x":`+nest(9999, "1")+`}`)
	cfg("depth 10001", `{"x":`+nest(10000, "1")+`}`)

	// Materials, each kind.
	mat := func(name string, deps ...string) {
		add("material "+name, one(slsa1, v1With("["+strings.Join(deps, ",")+"]")))
	}
	mat("purl plain", depJ("pkg:docker/alpine", ``))
	mat("purl tag", depJ("pkg:docker/alpine@3.20", ``))
	mat("purl namespace", depJ("pkg:docker/library/alpine@3.20", ``))
	mat("purl registry", depJ("pkg:docker/ghcr.io/foo/bar@v1", ``))
	mat("purl registry port", depJ("pkg:docker/localhost%3A5000/foo@v1", ``))
	mat("purl digest map", depJ("pkg:docker/alpine@3.20", `"sha256":"`+hexC+`"`))
	mat("purl digest map prefixed", depJ("pkg:docker/alpine@3.20", `"sha256":"`+dgC+`"`))
	mat("purl digest map spaces", depJ("pkg:docker/alpine@3.20", `"sha256":"  `+hexC+`\n"`))
	mat("purl digest map bad", depJ("pkg:docker/alpine@3.20", `"sha256":"zz"`))
	mat("purl digest map sha512 only", depJ("pkg:docker/alpine@3.20", `"sha512":"`+hexC+hexC+`"`))
	mat("purl digest version", depJ("pkg:docker/alpine@"+strings.ReplaceAll(dgC, ":", "%3A"), `"sha256":"`+hexC+`"`))
	mat("purl digest version mismatch", depJ("pkg:docker/alpine@"+strings.ReplaceAll(dgC, ":", "%3A"), `"sha256":"`+strings.Repeat("e", 64)+`"`))
	mat("purl digest qualifier", depJ("pkg:docker/alpine@3.20?digest="+dgC, ``))
	mat("purl digest qualifier mismatch", depJ("pkg:docker/alpine@3.20?digest="+dgC, `"sha256":"`+strings.Repeat("e", 64)+`"`))
	mat("purl digest qualifier and version", depJ("pkg:docker/alpine@"+dgC+"?digest="+dgB, ``))
	mat("purl digest qualifier bad", depJ("pkg:docker/alpine@3.20?digest=sha256:zz", ``))
	mat("purl platform", depJ("pkg:docker/alpine@3.20?platform=linux%2Farm%2Fv7", ``))
	mat("purl platform arm64", depJ("pkg:docker/alpine@3.20?platform=linux/arm64", ``))
	mat("purl platform aarch64", depJ("pkg:docker/alpine@3.20?platform=linux%2Faarch64", ``))
	mat("purl platform windows version", depJ("pkg:docker/alpine@3.20?platform=windows(10.0.17763)%2Famd64", ``))
	mat("purl platform bad", depJ("pkg:docker/alpine@3.20?platform=linux%2Farm64%2Fv8%2Fx", ``))
	mat("purl platform wildcard", depJ("pkg:docker/alpine@3.20?platform=linux%2F*", ``))
	mat("purl platform empty value", depJ("pkg:docker/alpine@3.20?platform=", ``))
	mat("purl two platforms", depJ("pkg:docker/alpine@3.20?platform=linux%2Famd64&platform=linux%2Fs390x", ``))
	mat("purl qualifier upper key", depJ("pkg:docker/alpine@3.20?PLATFORM=linux%2Famd64", ``))
	mat("purl qualifier bad key", depJ("pkg:docker/alpine@3.20?9x=1", ``))
	mat("purl qualifier escaped key", depJ("pkg:docker/alpine@3.20?%70latform=linux%2Famd64", ``))
	mat("purl qualifier bad escape", depJ("pkg:docker/alpine@3.20?platform=%zz", ``))
	// packageurl-go indexes kv[1] of a qualifier without `=`: buildx panics.
	mat("purl qualifier without value", depJ("pkg:docker/alpine@3.20?platform", ``))
	mat("purl subpath", depJ("pkg:docker/alpine@3.20#sub/path", ``))
	mat("purl uppercase name", depJ("pkg:docker/Alpine@3.20", ``))
	mat("purl bad tag", depJ("pkg:docker/alpine@-x", ``))
	mat("purl no name", depJ("pkg:docker/", ``))
	mat("purl escaped name", depJ("pkg:docker/foo%2Fbar@1", ``))
	mat("purl bad escape name", depJ("pkg:docker/alpine@%zz", ``))
	mat("purl type upper", depJ("pkg:DOCKER/alpine", ``))
	mat("purl npm", depJ("pkg:npm/foo@1", ``))
	mat("purl slashes", depJ("pkg:docker///library//alpine@3.20", ``))
	mat("purl at in namespace", depJ("pkg:docker/a@b/alpine@3.20", ``))
	mat("git https .git", depJ("https://github.com/a/b.git", ``))
	mat("git https .GIT", depJ("https://github.com/a/b.GIT", ``))
	mat("git https ref", depJ("https://github.com/a/b.git#v1.0", ``))
	mat("git https ref subdir", depJ("https://github.com/a/b.git#main:sub/dir", ``))
	mat("git https subdir only", depJ("https://github.com/a/b.git#:sub", ``))
	mat("git https empty fragment", depJ("https://github.com/a/b.git#", ``))
	mat("git https query", depJ("https://github.com/a/b.git?ref=main&subdir=x", ``))
	mat("git https user", depJ("https://user:pw@github.com/a/b.git", ``))
	mat("git https dotdot", depJ("https://github.com/a/../b.git", ``))
	mat("git https escaped", depJ("https://github.com/a/%62.git", ``))
	mat("git https escaped bad", depJ("https://github.com/a/%ff.git", ``))
	mat("git http", depJ("http://example.com/x.git", ``))
	mat("git upper scheme", depJ("HTTPS://github.com/a/b.git", ``))
	mat("git ssh", depJ("ssh://git@github.com/a/b.git", ``))
	mat("git ssh no .git", depJ("ssh://git@github.com/a/b", ``))
	mat("git git", depJ("git://github.com/a/b", ``))
	mat("git scp", depJ("git@github.com:a/b.git", ``))
	mat("git scp ref", depJ("git@github.com:a/b.git#main", ``))
	mat("git plus ssh", depJ("git+ssh://github.com/a/b.git", ``))
	mat("http plain", depJ("https://example.com/a/b", ``))
	mat("http query", depJ("https://example.com/a?x=1&x=2&y", ``))
	mat("http fragment", depJ("https://example.com/a#frag", ``))
	mat("http bare", depJ("http://", ``))
	mat("http upper", depJ("HTTP://example.com/a", ``))
	mat("unsupported ftp", depJ("ftp://example.com/x", ``))
	mat("unsupported s3", depJ("s3://bucket/x.git", ``))
	mat("unsupported word", depJ("context", ``))
	mat("unsupported empty", depJ("", ``))
	mat("unsupported local", depJ("local://context", ``))
	mat("unsupported spaced", depJ(" https://example.com/a", ``))
	mat("uri missing", `{"digest":{"sha256":"`+hexC+`"}}`)
	mat("duplicates", depJ("pkg:docker/alpine@3.20", ``), depJ("pkg:docker/alpine@3.20", ``))
	mat("same as root", depJ("pkg:docker/alpine@3.20?platform=linux%2Farm64", ``))
	mat("all skipped", depJ("ftp://x", ``), depJ("context", ``))
	add("material none", one(slsa1, v1With(`[]`)))
	add("material null", one(slsa1, v1With(`null`)))
	add("material merged", one(slsa1, `{"buildDefinition":{"buildType":"t","resolvedDependencies":[`+depJ("pkg:docker/alpine@3.20", `"sha512":"x"`)+`,`+depJ("https://a/b", ``)+`],"resolvedDependencies":[{"digest":{"sha256":"`+hexC+`"}}]}}`))
	add("material stale element", one(slsa1, `{"buildDefinition":{"buildType":"t","resolvedDependencies":[`+depJ("https://a/one", ``)+`,`+depJ("https://a/two", `"sha256":"`+hexC+`"`)+`],"resolvedDependencies":[{"uri":"https://a/three"}],"resolvedDependencies":[{},{"uri":"pkg:docker/alpine@3.20"}]}}`))
	add("material emptied then grown", one(slsa1, `{"buildDefinition":{"buildType":"t","resolvedDependencies":[`+depJ("https://a/one", ``)+`,`+depJ("https://a/two", `"sha256":"`+hexC+`"`)+`],"resolvedDependencies":[],"resolvedDependencies":[{},{"uri":"pkg:docker/alpine@3.20"}]}}`))
	add("material nulled then grown", one(slsa1, `{"buildDefinition":{"buildType":"t","resolvedDependencies":[`+depJ("https://a/one", ``)+`,`+depJ("https://a/two", `"sha256":"`+hexC+`"`)+`],"resolvedDependencies":null,"resolvedDependencies":[{},{"uri":"pkg:docker/alpine@3.20"}]}}`))
	add("material digest nulled", one(slsa1, `{"buildDefinition":{"buildType":"t","resolvedDependencies":[{"uri":"pkg:docker/alpine@3.20","digest":{"sha256":"`+hexC+`"},"digest":null}]}}`))
	add("v0.2 materials", one(slsa02, `{"buildType":"t","materials":[`+depJ("pkg:docker/alpine@3.20", `"sha256":"`+hexC+`"`)+`,`+depJ("git://github.com/a/b#main", ``)+`,`+depJ("nope", ``)+`]}`))

	// Other sources pass through.
	out = append(out, record{Name: "git source", Meta: metaCase{Source: sourceCase{Identifier: "git://github.com/a/b#main", Attrs: map[string]string{"git.fullurl": "https://github.com/a/b.git"}}}, Platform: arm64})
	out = append(out, record{Name: "image source no meta", Meta: metaCase{Source: img}, Platform: arm64})

	// The real moby/buildkit v0.28.1 arm64 attestations: its SBOM and its SLSA v1
	// provenance.
	add("real buildkit v0.28.1", imageWith(
		blobCase{Key: "sha256:523418043650e344fce4c599084fbbf1adfe7e5d00db6076292ad58e0456389f", MediaType: "application/vnd.in-toto+json", Annotations: map[string]string{ptKey: "https://spdx.dev/Document"}, Data: b64(stmt("https://spdx.dev/Document", `{"spdxVersion":"SPDX-2.3"}`))},
		blobCase{Key: "sha256:14c95411788ad54aa780bf35951a7d941ccc0592dc4478e9d399e29462e8c380", MediaType: "application/vnd.in-toto+json", Annotations: map[string]string{ptKey: slsa1}, DataFile: "real/buildkit-v0.28.1-arm64.provenance.json"},
	))

	// Resolving the materials' unknowns.
	resolveImage := func(name string, deps string, unknowns []string, replies map[string]resolverReply) {
		out = append(out, record{
			Name:     "resolve " + name,
			Meta:     metaCase{Source: img, Image: one(slsa1, v1With(deps))},
			Platform: arm64,
			Resolve:  &resolveCase{Unknowns: unknowns, Replies: replies},
		})
	}
	twoDeps := "[" + depJ("pkg:docker/golang@1.25?platform=linux%2Famd64", ``) + "," + depJ("https://github.com/a/b.git#main", ``) + "," + depJ("https://example.com/f", ``) + "]"
	golangReply := resolverReply{Image: &imageCase{Digest: dgB, Config: b64(`{"config":{"User":"go","Env":["A=1"]}}`), Chain: &chainCase{AttestationManifest: attMan, Blobs: []blobCase{
		slsaBlob("sha256:3333333333333333333333333333333333333333333333333333333333333333", slsa1, stmt(slsa1, v1With("["+depJ("pkg:docker/alpine@3.20", ``)+","+depJ("git://github.com/c/d", ``)+"]"))),
	}}}}
	resolveImage("material checksum", twoDeps, []string{"input.image.provenance.materials[0].image.checksum"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("material provenance", twoDeps, []string{"input.image.provenance.materials[0].image.provenance.builderID", "input.image.provenance.materials[0].image.hasProvenance"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("material labels and signatures", twoDeps, []string{"input.image.provenance.materials[0].image.labels", "input.image.provenance.materials[0].image.signatures"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("material git", twoDeps, []string{"input.image.provenance.materials[1].git.commit"}, map[string]resolverReply{"git://github.com/a/b.git#main": {}})
	resolveImage("material git checksum", twoDeps, []string{"input.image.provenance.materials[1].git.checksum", "input.image.provenance.materials[1].git.tag"}, map[string]resolverReply{"git://github.com/a/b.git#main": {}})
	resolveImage("material http", twoDeps, []string{"input.image.provenance.materials[2].http.checksum"}, nil)
	resolveImage("material out of range", twoDeps, []string{"input.image.provenance.materials[7].image.checksum"}, nil)
	resolveImage("material whole", twoDeps, []string{"input.image.provenance.materials[0]", "input.image.provenance.materials"}, nil)
	resolveImage("material error", twoDeps, []string{"input.image.provenance.materials[0].image.checksum"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": {Error: "boom"}})
	resolveImage("material unhandled", twoDeps, []string{"input.image.provenance.materials[0].image.ref"}, nil)
	resolveImage("material second only", twoDeps, []string{"input.image.provenance.materials[1].git.ref", "input.image.provenance.materials[0]", "input.image.provenance.materials[7].git.ref", "input.image.provenance.materials[-1].git.ref", "input.image.provenance.materials[+1].git.ref", "input.image.provenance.materials[x].git.ref"},
		map[string]resolverReply{"git://github.com/a/b.git#main": {}})
	resolveImage("direct first", twoDeps, []string{"input.image.provenance.materials[0].image.checksum", "image.signatures", "input.image"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("direct no request", twoDeps, []string{"image", "http.checksum", "input.image.provenance.materials[0].image.checksum"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("nested", twoDeps, []string{"input.image.provenance.materials[0].image.provenance.materials[1].git.checksum"},
		map[string]resolverReply{"docker-image://docker.io/library/golang:1.25": golangReply})
	resolveImage("canonical material", "["+depJ("pkg:docker/alpine@3.20", `"sha256":"`+hexC+`"`)+"]", []string{"input.image.provenance.materials[0].image.checksum", "input.image.provenance.materials[0].image.labels"},
		map[string]resolverReply{"docker-image://docker.io/library/alpine:3.20@" + dgC: {Image: &imageCase{Digest: dgC}}})
	resolveImage("skipped material index", "["+depJ("ftp://x", ``)+","+depJ("pkg:docker/alpine@3.20", ``)+"]", []string{"input.image.provenance.materials[0].image.checksum", "input.image.provenance.materials[1].image.checksum"},
		map[string]resolverReply{"docker-image://docker.io/library/alpine:3.20": {Image: &imageCase{Digest: dgC}}})
	out = append(out, record{
		Name:     "resolve root without metadata",
		Meta:     metaCase{Source: img},
		Platform: &platformCase{OS: "linux", Arch: "arm", Variant: "v7"},
		Resolve:  &resolveCase{Unknowns: []string{"input.image.checksum", "input.image.provenance", "input.image.provenance.materials[0].image.checksum"}},
	})
	return out
}

// ---- running them ----

type fakeResolver struct {
	replies map[string]resolverReply
	calls   []requestRecord
	dir     string
}

func (f *fakeResolver) ResolveSourceMetadata(_ context.Context, op *pb.SourceOp, opt sourceresolver.Opt) (*sourceresolver.MetaResponse, error) {
	rec := requestRecord{Source: op.Identifier}
	if opt.ImageOpt != nil {
		rec.Image = true
		rec.NoConfig = opt.ImageOpt.NoConfig
		rec.AttestationChain = opt.ImageOpt.AttestationChain
		rec.ResolveAttestations = opt.ImageOpt.ResolveAttestations
		if p := opt.ImageOpt.Platform; p != nil {
			rec.Platform = &platformCase{OS: p.OS, Arch: p.Architecture, Variant: p.Variant}
		}
	}
	if opt.GitOpt != nil {
		rec.Git = true
		rec.ReturnObject = opt.GitOpt.ReturnObject
	}
	if opt.HTTPOpt != nil {
		rec.HTTP = true
	}
	f.calls = append(f.calls, rec)
	r, ok := f.replies[op.Identifier]
	if !ok {
		return nil, fmt.Errorf("no reply for %s", op.Identifier)
	}
	if r.Error != "" {
		return nil, fmt.Errorf("%s", r.Error)
	}
	resp := &sourceresolver.MetaResponse{Op: op}
	if r.Image != nil {
		im, err := gatewayImage(r.Image, f.dir)
		if err != nil {
			return nil, err
		}
		resp.Image = &sourceresolver.ResolveImageResponse{Digest: digest.Digest(im.Digest), Config: im.Config}
		if c := im.AttestationChain; c != nil {
			ac := &sourceresolver.AttestationChain{AttestationManifest: digest.Digest(c.AttestationManifest), Blobs: map[digest.Digest]sourceresolver.Blob{}}
			for k, b := range c.Blobs {
				var d ocispecs.Descriptor
				if b.Descriptor_ != nil {
					d = ocispecs.Descriptor{MediaType: b.Descriptor_.MediaType, Digest: digest.Digest(b.Descriptor_.Digest), Size: b.Descriptor_.Size, Annotations: b.Descriptor_.Annotations}
				}
				ac.Blobs[digest.Digest(k)] = sourceresolver.Blob{Descriptor: d, Data: b.Data}
			}
			resp.Image.AttestationChain = ac
		}
	}
	if strings.HasPrefix(op.Identifier, "git://") {
		resp.Git = &sourceresolver.ResolveGitResponse{Checksum: "1111111111111111111111111111111111111111", Ref: "refs/heads/main"}
	}
	return resp, nil
}

func gatewayImage(c *imageCase, dir string) (*gwpb.ResolveSourceImageResponse, error) {
	im := &gwpb.ResolveSourceImageResponse{Digest: c.Digest}
	if c.Config != "" {
		cfg, err := base64.StdEncoding.DecodeString(c.Config)
		if err != nil {
			return nil, err
		}
		im.Config = cfg
	}
	if c.Chain != nil {
		ac := &gwpb.AttestationChain{AttestationManifest: c.Chain.AttestationManifest, Blobs: map[string]*gwpb.Blob{}}
		for _, b := range c.Chain.Blobs {
			var data []byte
			var err error
			if b.DataFile != "" {
				data, err = os.ReadFile(dir + "/" + b.DataFile)
			} else {
				data, err = base64.StdEncoding.DecodeString(b.Data)
			}
			if err != nil {
				return nil, err
			}
			blob := &gwpb.Blob{Data: data}
			if !b.NoDesc {
				blob.Descriptor_ = &gwpb.Descriptor{MediaType: b.MediaType, Digest: b.Key, Size: int64(len(data)), Annotations: b.Annotations}
			}
			ac.Blobs[b.Key] = blob
		}
		im.AttestationChain = ac
	}
	return im, nil
}

func ociPlatform(p *platformCase) *ocispecs.Platform {
	if p == nil {
		return nil
	}
	return &ocispecs.Platform{OS: p.OS, Architecture: p.Arch, Variant: p.Variant}
}

func requestOf(r *gwpb.ResolveSourceMetaRequest) *requestRecord {
	if r == nil {
		return nil
	}
	rec := &requestRecord{}
	if r.Source != nil {
		rec.Source = r.Source.Identifier
	}
	if p := r.Platform; p != nil {
		rec.Platform = &platformCase{OS: p.OS, Arch: p.Architecture, Variant: p.Variant}
	}
	if r.Image != nil {
		rec.Image = true
		rec.NoConfig = r.Image.NoConfig
		rec.AttestationChain = r.Image.AttestationChain
		rec.ResolveAttestations = r.Image.ResolveAttestations
	}
	if r.Git != nil {
		rec.Git = true
		rec.ReturnObject = r.Git.ReturnObject
	}
	if r.HTTP != nil {
		rec.HTTP = true
	}
	return rec
}

func run(c *record, dir string) {
	var logs []string
	logf := func(l logrus.Level, msg string) { logs = append(logs, l.String()+": "+msg) }
	resp := &gwpb.ResolveSourceMetaResponse{Source: &pb.SourceOp{Identifier: c.Meta.Source.Identifier, Attrs: c.Meta.Source.Attrs}}
	if c.Meta.Image != nil {
		im, err := gatewayImage(c.Meta.Image, dir)
		if err != nil {
			panic(err)
		}
		resp.Image = im
	}
	platform := ociPlatform(c.Platform)
	var inp policy.Input
	func() {
		defer func() {
			if r := recover(); r != nil {
				c.Error = fmt.Sprintf("panic: %v", r)
			}
		}()
		var err error
		inp, err = policy.SourceToInput(context.Background(), nil, resp, platform, logf)
		if err != nil {
			c.Error = err.Error()
		}
	}()
	c.Logs = logs
	if c.Error != "" {
		return
	}
	dt, err := json.Marshal(inp)
	if err != nil {
		panic(err)
	}
	c.Input = string(dt)
	c.Unknowns = inp.Unknowns()
	if c.Resolve == nil {
		return
	}
	logs = nil
	f := &fakeResolver{replies: c.Resolve.Replies, dir: dir}
	rec := &resolveRecord{}
	var rootPlatform *pb.Platform
	if platform != nil {
		rootPlatform = &pb.Platform{OS: platform.OS, Architecture: platform.Architecture, Variant: platform.Variant}
	}
	retry, next, err := policy.ResolveInputUnknowns(context.Background(), &inp, resp.Source, c.Resolve.Unknowns, rootPlatform, platform, f, nil, logf)
	rec.Retry = retry
	rec.Next = requestOf(next)
	if err != nil {
		rec.Error = err.Error()
	}
	rec.Calls = f.calls
	if dt, err := json.Marshal(inp); err == nil {
		rec.Input = string(dt)
	}
	rec.Unknowns = inp.Unknowns()
	rec.Logs = logs
	c.Resolved = rec
}

func TestShardsProvenanceOracle(t *testing.T) {
	dir := os.Getenv("SHARDS_PROVENANCE_DIR")
	cs := cases()
	for i := range cs {
		run(&cs[i], dir)
	}
	out, err := json.MarshalIndent(cs, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_PROVENANCE_OUT"), append(out, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
	_ = platforms.Format
}
