package policy

// Placed in buildx v0.37.1's policy package by scripts/policy/generate-tester: what
// RunPolicyTests and `policy test`'s printing (commands/policy/test.go runTest, copied
// below) make of each tree of a policy and its tests, run in the tree as the command runs
// in the working directory, with a provider that answers every image the same way: the
// builder's platform linux/amd64, a digest of the source's identifier, and one config
// where one is asked for. A file named with a trailing slash is a directory. For
// crates/shards/testdata/policy/tester.json.

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"

	gwpb "github.com/moby/buildkit/frontend/gateway/pb"
	"github.com/moby/buildkit/solver/pb"
	ocispecs "github.com/opencontainers/image-spec/specs-go/v1"
)

const shardsConfig = `{"created":"2024-01-02T03:04:05Z","config":{"Labels":{"org.opencontainers.image.source":"https://github.com/x/y"},"User":"app","Env":["PATH=/bin"],"WorkingDir":"/w","Volumes":{"/data":{}}}}`

type shardsTesterCase struct {
	Name     string            `json:"name"`
	Files    map[string]string `json:"files"`
	Path     string            `json:"path"`
	Run      string            `json:"run"`
	Filename string            `json:"filename"`
	Stdout   string            `json:"stdout"`
	Err      string            `json:"err"`
	Status   int               `json:"status"`
	// What buildx panicked with, where it did.
	Panic string `json:"panic,omitempty"`
}

func shardsDigest(s string) string {
	sum := sha256.Sum256([]byte(s))
	return "sha256:" + hex.EncodeToString(sum[:])
}

func shardsProvider() *TestOptionsProvider {
	return &TestOptionsProvider{
		Platform: func(context.Context) (*ocispecs.Platform, error) {
			return &ocispecs.Platform{OS: "linux", Architecture: "amd64"}, nil
		},
		Resolve: func(_ context.Context, src *pb.SourceOp, req *gwpb.ResolveSourceMetaRequest) (*gwpb.ResolveSourceMetaResponse, error) {
			resp := &gwpb.ResolveSourceMetaResponse{Source: src}
			if strings.HasPrefix(src.Identifier, "docker-image://") {
				img := &gwpb.ResolveSourceImageResponse{Digest: shardsDigest(src.Identifier)}
				if req.Image != nil && !req.Image.NoConfig {
					img.Config = []byte(shardsConfig)
				}
				resp.Image = img
			}
			return resp, nil
		},
	}
}

// runTest's printing (commands/policy/test.go), with what main prints of an error.
func shardsReport(out io.Writer, summary TestSummary) int {
	for _, result := range summary.Results {
		status := "PASS"
		if !result.Passed {
			status = "FAIL"
		}
		allowStr := "n/a"
		if result.Allow != nil {
			allowStr = fmt.Sprintf("%v", *result.Allow)
		}
		if len(result.DenyMessages) > 0 {
			_, _ = fmt.Fprintf(out, "%s: %s (allow=%s, deny_msg=%s)\n", result.Name, status, allowStr, strings.Join(result.DenyMessages, "; "))
		} else {
			_, _ = fmt.Fprintf(out, "%s: %s (allow=%s)\n", result.Name, status, allowStr)
		}
		if result.Passed {
			continue
		}
		if result.Input != nil {
			shardsWriteJSON(out, "input", result.Input)
		} else {
			_, _ = fmt.Fprintln(out, "input: <nil>")
		}
		if result.Decision != nil {
			shardsWriteJSON(out, "decision", result.Decision)
		} else {
			_, _ = fmt.Fprintln(out, "decision: <nil>")
		}
		if len(result.MissingInput) > 0 {
			keys := make([]string, len(result.MissingInput))
			for i, k := range result.MissingInput {
				keys[i] = "input." + k
			}
			_, _ = fmt.Fprintf(out, "missing_input: %s\n", strings.Join(keys, ", "))
		}
		if len(result.MetadataNeeded) > 0 {
			_, _ = fmt.Fprintf(out, "metadata_resolve: %s\n", strings.Join(result.MetadataNeeded, ", "))
		}
	}
	if summary.Failed > 0 {
		return 1
	}
	return 0
}

func shardsWriteJSON(out io.Writer, label string, v any) {
	dt, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		_, _ = fmt.Fprintf(out, "%s: <error encoding>\n", label)
		return
	}
	_, _ = fmt.Fprintf(out, "%s:\n%s\n", label, string(dt))
}

const shardsAllow = "package docker\n\ndefault allow := false\n\nallow if input.local\n\nallow if input.image.repo == \"alpine\"\n\ndeny_msg contains \"not alpine\" if not allow\n\ndecision := {\"allow\": allow, \"deny_msg\": deny_msg}\n"

func shardsCases() []shardsTesterCase {
	t := func(name, path string, files map[string]string) shardsTesterCase {
		return shardsTesterCase{Name: name, Files: files, Path: path, Filename: "Dockerfile"}
	}
	withPolicy := func(name, tests string) shardsTesterCase {
		return t(name, ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": tests})
	}
	var cs []shardsTesterCase
	add := func(c shardsTesterCase) { cs = append(cs, c) }

	add(withPolicy("pass and fail", "package docker\n\ntest_local if {\n\tallow with input as {\"local\": {\"name\": \"context\"}}\n}\n\ntest_alpine if {\n\tallow with input as {\"image\": {\"repo\": \"alpine\", \"ref\": \"docker.io/library/alpine:3.20\"}}\n}\n\ntest_busybox if {\n\tallow with input as {\"local\": {\"name\": \"x\"}, \"image\": {\"repo\": \"busybox\"}}\n}\n\ntest_none if {\n\tnot allow\n}\n"))
	add(withPolicy("no input", "package docker\n\ntest_nothing if {\n\tallow\n}\n"))
	add(withPolicy("env and http", "package docker\n\ntest_http if {\n\tallow with input as {\"env\": {\"filename\": \"Dockerfile\", \"args\": {\"A\": \"1\", \"B\": null}, \"labels\": {}, \"depth\": 2}, \"http\": {\"url\": \"https://example.com/x\", \"schema\": \"https\", \"host\": \"example.com\", \"path\": \"/x\", \"query\": {\"a\": [\"1\"], \"b\": null, \"c\": []}, \"checksum\": \"sha256:aa\"}}\n}\n"))
	add(withPolicy("git", "package docker\n\ntest_git if {\n\tallow with input as {\"git\": {\"schema\": \"https\", \"host\": \"github.com\", \"remote\": \"https://github.com/a/b.git\", \"tagName\": \"v1\", \"isCommitRef\": false, \"commit\": {\"tree\": \"t\", \"parents\": [\"p\"], \"author\": {\"name\": \"A\", \"when\": \"2024-01-02T03:04:05+00:00\"}, \"committer\": {}, \"pgpSignature\": {\"version\": 4, \"keyID\": \"abc\"}}, \"tag\": {\"tagger\": {\"email\": \"e@x\"}, \"sshSignature\": {}}}}\n}\n"))
	add(withPolicy("unknown fields dropped", "package docker\n\ntest_x if {\n\tallow with input as {\"local\": {\"name\": \"c\", \"extra\": 1}, \"other\": [1, 2]}\n}\n"))
	add(withPolicy("folded and repeated keys", "package docker\n\ntest_x if {\n\tallow with input as {\"LOCAL\": {\"NAME\": \"c\"}, \"Image\": {\"Repo\": \"alpine\"}}\n}\n"))
	add(withPolicy("type error", "package docker\n\ntest_x if {\n\tallow with input as {\"local\": {\"name\": 1}}\n}\n"))
	add(withPolicy("type error nested", "package docker\n\ntest_x if {\n\tallow with input as {\"image\": {\"repo\": \"a\", \"labels\": {\"a\": true}}}\n}\n"))
	add(withPolicy("type error int", "package docker\n\ntest_x if {\n\tallow with input as {\"env\": {\"depth\": 1.5}}\n}\n"))
	add(withPolicy("type error top", "package docker\n\ntest_x if {\n\tallow with input as [1]\n}\n"))
	add(withPolicy("input not json", "package docker\n\ntest_x if {\n\tallow with input as data.other\n}\n"))
	add(withPolicy("input set", "package docker\n\ntest_x if {\n\tallow with input as {1, 2}\n}\n"))
	add(withPolicy("input with var", "package docker\n\ntest_x if {\n\tx := \"a\"\n\tallow with input as {\"local\": {\"name\": x}}\n}\n"))
	add(withPolicy("multiple overrides", "package docker\n\ntest_x if {\n\tallow with input as {\"local\": {\"name\": \"a\"}}\n\tallow with input as {\"local\": {\"name\": \"b\"}}\n}\n"))
	add(withPolicy("equal overrides", "package docker\n\ntest_x if {\n\tallow with input as {\"local\": {\"name\": \"a\"}}\n\tallow with input as {\"local\": {\"name\": \"a\"}}\n}\n"))
	add(withPolicy("input null", "package docker\n\ntest_x if {\n\tallow with input as null\n}\n"))
	add(withPolicy("image resolved", "package docker\n\ntest_alpine if {\n\tallow with input as {\"image\": {\"repo\": \"alpine\", \"tag\": \"3.20\"}}\n}\n\ntest_platform if {\n\tallow with input as {\"image\": {\"ref\": \"docker.io/library/alpine:3.20\", \"platform\": \"linux/arm64/v8\"}}\n}\n\ntest_os if {\n\tallow with input as {\"image\": {\"fullRepo\": \"docker.io/library/busybox\", \"os\": \"linux\", \"arch\": \"x86_64\"}}\n}\n"))
	add(withPolicy("image bad platform", "package docker\n\ntest_x if {\n\tallow with input as {\"image\": {\"repo\": \"alpine\", \"platform\": \"linux/a/b/c\"}}\n}\n"))
	add(t("image labels", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.image.labels[\"org.opencontainers.image.source\"] == \"https://github.com/x/y\"\n\ndecision := {\"allow\": allow}\n",
		"p_test.rego":     "package docker\n\ntest_labels if {\n\tallow with input as {\"image\": {\"repo\": \"alpine\", \"env\": [\"A=1\"]}}\n}\n",
	}))
	add(t("image checksum", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if startswith(input.image.checksum, \"sha256:\")\n\ndecision := {\"allow\": allow}\n",
		"p_test.rego":     "package docker\n\ntest_checksum if {\n\tallow with input as {\"image\": {\"ref\": \"docker.io/library/alpine:3.20\"}}\n}\n",
	}))
	add(t("image never answered", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.image.workingDir == \"/w\"\n\nallow if count(input.image.provenance.materials) > 0\n\ndecision := {\"allow\": allow}\n",
		"p_test.rego":     "package docker\n\ntest_x if {\n\tallow with input as {\"image\": {\"repo\": \"alpine\"}}\n}\n",
	}))
	add(t("missing input", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.git.tagName == \"v1\"\n\nallow if input.http.checksum == \"x\"\n\nallow if input.env.target == \"t\"\n\ndecision := {\"allow\": allow}\n",
		"p_test.rego":     "package docker\n\ntest_git if {\n\tallow with input as {\"git\": {\"branch\": \"main\"}}\n}\n\ntest_none if {\n\tallow\n}\n",
	}))
	add(t("decision shapes", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndecision := {\"allow\": \"yes\", \"deny_msg\": \"one\", \"caps\": {\"exec.proxy\": true, \"x\": 1}}\n",
		"p_test.rego":     "package docker\n\ntest_x if {\n\tfalse\n}\n",
	}))
	add(t("decision set messages", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\nmsgs contains \"b\"\n\nmsgs contains \"a\"\n\ndecision := {\"allow\": false, \"deny_msg\": msgs}\n",
		"p_test.rego":     "package docker\n\ntest_x if {\n\tdecision.allow\n}\n",
	}))
	add(t("decision not object", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndecision := true\n",
		"p_test.rego":     "package docker\n\ntest_x if {\n\tnot decision\n}\n",
	}))
	add(t("decision undefined", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\nallow := true\n",
		"p_test.rego":     "package docker\n\ntest_x if {\n\tnot allow\n}\n",
	}))
	add(t("decision empty", ".", map[string]string{
		"Dockerfile.rego": "package docker\n\ndecision := {}\n",
		"p_test.rego":     "package docker\n\ntest_x := false\n",
	}))
	add(withPolicy("non boolean and functions skipped", "package docker\n\ntest_one := 1\n\ntest_f(x) if x\n\ntest_ok := true\n\nhelper if true\n"))
	add(withPolicy("run filter", "package docker\n\ntest_a if true\n\ntest_b if true\n\ntest_ab if false\n"))
	cs[len(cs)-1].Run = "b"
	add(withPolicy("run filter none", "package docker\n\ntest_a if true\n"))
	cs[len(cs)-1].Run = "zzz"
	add(withPolicy("no tests", "package docker\n\nhelper if true\n"))
	add(t("no test files", ".", map[string]string{"Dockerfile.rego": shardsAllow, "x.rego": "package docker\n"}))
	add(t("single file", "tests/one_test.rego", map[string]string{"Dockerfile.rego": shardsAllow, "tests/one_test.rego": "package docker\n\ntest_one if not allow\n", "tests/two_test.rego": "package docker\n\ntest_two if allow\n"}))
	add(t("directory", "tests/", map[string]string{"Dockerfile.rego": shardsAllow, "tests/b_test.rego": "package docker\n\ntest_b if not allow\n", "tests/a_test.rego": "package docker\n\ntest_a if allow with input as {\"local\": {}}\n", "tests/c.rego": "package docker\n\ntest_c if false\n", "tests/sub/d_test.rego": "package docker\n\ntest_d if false\n", "tests/e_test.rego/": ""}))
	add(t("bad suffix", "tests/one.rego", map[string]string{"Dockerfile.rego": shardsAllow, "tests/one.rego": "package docker\n"}))
	add(t("missing path", "nope", map[string]string{"Dockerfile.rego": shardsAllow}))
	add(t("absolute path", "/etc", map[string]string{"Dockerfile.rego": shardsAllow}))
	add(t("parent path", "../x", map[string]string{"Dockerfile.rego": shardsAllow}))
	add(t("missing policy", ".", map[string]string{"p_test.rego": "package docker\n\ntest_a if true\n"}))
	add(t("policy parse error", ".", map[string]string{"Dockerfile.rego": "package docker\n\nallow if {\n", "p_test.rego": "package docker\n\ntest_a if true\n"}))
	add(t("test parse error", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package docker\n\ntest_a if ][\n"}))
	add(t("compile error", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package docker\n\ntest_a if undefined_function(1)\n"}))
	add(t("import from fs", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.lib.names\n\ndefault allow := false\n\nallow if input.image.repo in names.allowed\n\ndecision := {\"allow\": allow}\n", "lib/names.rego": "package names\n\nallowed := {\"alpine\"}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("import missing", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.lib.gone\n\ndecision := {\"allow\": true}\n", "p_test.rego": "package docker\n\ntest_a if true\n"}))
	add(t("import from tests", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndecision := {\"allow\": true}\n", "p_test.rego": "package docker\n\nimport data.helpers\n\ntest_a if helpers.yes\n", "q_test.rego": "package helpers\n\nyes := true\n"}))
	add(t("other package", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package other\n\ntest_x if true\n\ntest_y if data.docker.allow with input as {\"local\": {}}\n"}))
	add(t("eval error", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package docker\n\nf := 1\n\nf := 2\n\ntest_x if f == 1\n"}))
	add(t("load json", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.image.repo in load_json(\"allowed.json\")\n\ndecision := {\"allow\": allow}\n", "allowed.json": "[\"alpine\"]", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"local\": {}, \"image\": {\"repo\": \"alpine\", \"ref\": \"alpine\"}}\n"}))
	add(t("attestation without verifier", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if artifact_attestation(input.http, \"b.json\")\n\ndecision := {\"allow\": allow}\n", "b.json": "{}", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"http\": {\"url\": \"https://x/y\", \"checksum\": \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"}}\n\ntest_b if allow with input as {\"http\": {\"url\": \"https://x/y\"}}\n"}))
	add(t("verify git without object", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if verify_git_signature(input.git.tag, \"k.pub\")\n\ndecision := {\"allow\": allow}\n", "k.pub": "", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"git\": {\"remote\": \"r\"}}\n"}))
	add(t("pin image", ".", map[string]string{"Dockerfile.rego": "package docker\n\nallow if pin_image(input.image, \"sha256:0000000000000000000000000000000000000000000000000000000000000000\")\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"image\": {\"checksum\": \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"}}\n"}))
	add(withPolicy("time errors", "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"author\": {\"when\": 5}}}}\n"))
	add(withPolicy("time parse error", "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"author\": {\"when\": \"2024-01-02 03:04:05\"}}}}\n"))
	add(withPolicy("time one digit hour", "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"author\": {\"when\": \"2024-01-02T3:04:05Z\"}}}}\n"))
	add(withPolicy("time fraction comma", "package docker\n\ntest_a if allow with input as {\"git\": {\"tag\": {\"tagger\": {\"when\": \"2024-01-02T03:04:05,5Z\"}}}}\n"))
	add(withPolicy("time offset and fraction", "package docker\n\ntest_a if allow with input as {\"git\": {\"tag\": {\"tagger\": {\"when\": \"2024-01-02T03:04:05.120+05:30\"}}}}\n"))
	add(withPolicy("time null", "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"author\": {\"when\": null}}}}\n"))
	add(withPolicy("provenance and signatures", "package docker\n\ntest_a if allow with input as {\"image\": {\"hasProvenance\": true, \"provenance\": {\"predicateType\": \"p\", \"configSource\": {\"uri\": \"u\", \"digest\": {\"sha1\": \"x\"}}, \"buildArgs\": {}, \"reproducible\": false, \"completeness\": {\"materials\": true}, \"materials\": [{\"image\": {\"ref\": \"m\"}}, {}, null]}, \"signatures\": [{\"kind\": \"docker-github-builder\", \"timestamps\": [{\"type\": \"Tlog\"}, {\"uri\": \"u\", \"timestamp\": \"2024-01-02T03:04:05Z\"}], \"isDHI\": true, \"signer\": {\"issuer\": \"i\"}}, {}]}}\n"))
	add(withPolicy("slices merged", "package docker\n\ntest_a if allow with input as {\"image\": {\"env\": [\"a\", \"b\", \"c\"], \"Env\": [\"x\"], \"volumes\": [], \"signatures\": [{\"kind\": \"k\", \"dockerReference\": \"r\"}, {\"kind\": \"j\"}], \"SIGNATURES\": [{\"type\": \"t\"}]}}\n"))
	add(withPolicy("env zero and maps", "package docker\n\ntest_a if allow with input as {\"env\": {\"args\": {}}}\n\ntest_b if allow with input as {\"env\": {}}\n\ntest_c if allow with input as {\"env\": {\"labels\": null, \"capsRequest\": true}}\n"))
	add(withPolicy("html escaping", "package docker\n\ntest_a if not allow with input as {\"local\": {\"name\": \"<a&b>\\u2028\\t\\u00e9\"}}\n"))
	add(withPolicy("numbers exponent", "package docker\n\ntest_a if allow with input as {\"env\": {\"depth\": 1e2}}\n"))
	add(withPolicy("numbers overflow", "package docker\n\ntest_b if allow with input as {\"git\": {\"commit\": {\"pgpSignature\": {\"version\": 9223372036854775808}}}}\n"))
	add(withPolicy("strings bad escapes", "package docker\n\ntest_a if allow with input as {\"local\": {\"name\": \"\\u0001\"}}\n"))
	add(withPolicy("deny messages", "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"busybox\"}}\n"))
	add(t("filename other", ".", map[string]string{"build.rego": shardsAllow, "p_test.rego": "package docker\n\ntest_a if allow with input as {\"local\": {}}\n"}))
	cs[len(cs)-1].Filename = "build"
	add(t("filename empty", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package docker\n\ntest_a if true\n"}))
	cs[len(cs)-1].Filename = ""
	add(t("nested refs", ".", map[string]string{"Dockerfile.rego": "package docker\n\nkeys := {\"a\": \"image\"}\n\ndefault allow := false\n\nallow if input[keys.a].repo == data.lists[input.env.target]\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"local\": {}}\n"}))
	// Keys print sorted (upper case first), so the folded spellings decode in this order:
	// a short array, a long one, a short one, a longer one, reusing what Go's growth kept.
	add(withPolicy("slice capacities", "package docker\n\ntest_a if allow with input as {\"image\": {\"SIGNATURES\": [{\"kind\": \"A\"}], \"SIGNATUREs\": [{\"kind\": \"B\", \"timestamps\": [{\"uri\": \"1\"}]}, {\"kind\": \"C\"}, {\"kind\": \"D\"}], \"SIgnatures\": [{\"dockerReference\": \"E\", \"TIMESTAMPS\": [{\"type\": \"x\"}, {\"type\": \"y\"}, {\"type\": \"z\"}], \"timestamps\": [{}]}], \"Signatures\": [{}, {}, {}, {}, {}], \"provenance\": {\"MATERIALS\": [{\"local\": {\"name\": \"a\"}}], \"MATERIALs\": [{\"local\": {\"name\": \"b\"}}, {\"local\": {\"name\": \"c\"}}, {\"local\": {\"name\": \"d\"}}], \"MAterials\": [{}], \"Materials\": [{}, {}, {}, {}, {}]}, \"ENV\": [\"1\"], \"ENv\": [\"2\", \"3\", \"4\"], \"Env\": [\"5\"], \"env\": [\"6\", \"7\", \"8\", \"9\", \"10\"]}}\n"))
	add(withPolicy("with on input member", "package docker\n\ntest_a if not allow with input.local as {\"name\": \"x\"}\n"))
	add(t("env target resolves", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if {\n\tinput.env.target == \"t\"\n\tinput.image.user == \"app\"\n}\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"env\": {\"target\": \"t\"}, \"image\": {\"ref\": \"localhost:5000/x\", \"tag\": \"1\"}}\n\ntest_b if allow with input as {\"env\": {\"depth\": 3}, \"image\": {\"repo\": \"alpine\", \"tag\": \"1\"}}\n"}))
	add(t("rule shapes", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package docker\n\ndefault test_d := false\n\ntest_d if allow\n\ntest_e if false else := true\n\ntest_s contains 1\n\ntest_ns.a if true\n\ntest_k[\"k\"] := true\n"}))
	// Go's map makes either package's the test: both answer alike.
	add(t("same name two packages", ".", map[string]string{"Dockerfile.rego": shardsAllow, "a_test.rego": "package pa\n\ntest_same if true\n", "b_test.rego": "package pb\n\ntest_same if true\n"}))
	add(withPolicy("map null after", "package docker\n\ntest_a if allow with input as {\"env\": {\"LABELS\": {\"a\": \"b\"}, \"labels\": null}, \"image\": {\"provenance\": {\"REPRODUCIBLE\": true, \"reproducible\": null, \"BUILDARGS\": {\"a\": \"b\"}, \"buildArgs\": null}}}\n"))
	add(withPolicy("multiple overrides across rules", "package docker\n\ntest_x if allow with input as {\"local\": {\"name\": \"a\"}}\n\ntest_x if allow with input as {\"local\": {\"name\": \"b\"}}\n"))
	add(withPolicy("equal overrides across rules", "package docker\n\ntest_x if allow with input as {\"local\": {\"name\": \"a\"}}\n\ntest_x if not allow with input as {\"local\": {\"name\": \"a\"}}\n"))
	add(withPolicy("platform os only", "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"alpine\", \"os\": \"linux\"}}\n"))
	add(withPolicy("platform arch only", "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"alpine\", \"arch\": \"amd64\"}}\n"))
	add(withPolicy("platform variant", "package docker\n\ntest_a if not allow with input as {\"image\": {\"repo\": \"alpine\", \"os\": \"linux\", \"arch\": \"aarch64\", \"variant\": \"8\"}}\n\ntest_b if not allow with input as {\"image\": {\"repo\": \"alpine\", \"platform\": \"windows(10.0.17763)/amd64\"}}\n"))
	add(t("image input reads git", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.image.repo == \"alpine\"\n\nallow if input.git.tagName == \"v1\"\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("image input unhandled field", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if input.image.tag == \"1\"\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"image\": {\"ref\": \"alpine@sha256:0000000000000000000000000000000000000000000000000000000000000000\"}}\n"}))
	add(t("image decision undefined", ".", map[string]string{"Dockerfile.rego": "package docker\n\nallow := true\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("image decision not object", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndecision := 1\n", "p_test.rego": "package docker\n\ntest_a if decision with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("policy imports a test package", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.helpers\n\ndecision := {\"allow\": helpers.yes}\n", "p_test.rego": "package docker\n\ntest_a if decision.allow with input as {\"local\": {}}\n\ntest_b if decision.allow with input as {\"image\": {\"repo\": \"alpine\"}}\n", "q_test.rego": "package helpers\n\nyes := true\n"}))
	add(t("policy calls a test function", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndecision := {\"allow\": helper(1)}\n", "p_test.rego": "package docker\n\nhelper(x) := x == 1\n\ntest_a if decision.allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("import parse error", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.lib.bad\n\ndecision := {\"allow\": true}\n", "lib/bad.rego": "package bad\n\nx if {\n", "p_test.rego": "package docker\n\ntest_a if true\n"}))
	add(t("import directory", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.lib.dir\n\ndecision := {\"allow\": true}\n", "lib/dir.rego/": "", "p_test.rego": "package docker\n\ntest_a if true\n"}))
	add(t("verify git runtime unknown", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if verify_git_signature(input.git, \"k.pub\")\n\ndecision := {\"allow\": allow}\n", "k.pub": "", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"git\": {\"remote\": \"r\"}}\n"}))
	add(t("verify git commit", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if verify_git_signature(input.git.commit, \"k.pub\")\n\ndecision := {\"allow\": allow}\n", "k.pub": "", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"tree\": \"t\"}}}\n"}))
	add(withPolicy("time offset 24 hours", "package docker\n\ntest_a if allow with input as {\"git\": {\"commit\": {\"author\": {\"when\": \"2024-01-02T03:04:05+24:00\"}}}}\n"))
	add(withPolicy("time timestamp offset 24 hours", "package docker\n\ntest_a if allow with input as {\"image\": {\"signatures\": [{\"timestamps\": [{\"timestamp\": \"2024-01-02T03:04:05-23:60\"}]}]}}\n"))
	add(t("load json names", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault a := false\n\na if \"x\" in load_json(\"a.json\")\n\ndefault b := false\n\nb if \"x\" in load_json(\"./a.json\")\n\ndefault c := false\n\nc if \"x\" in load_json(\"cwd://a.json\")\n\ndefault d := false\n\nd if \"x\" in load_json(\"sub/../a.json\")\n\ndecision := {\"allow\": false, \"deny_msg\": [sprintf(\"%v %v %v %v\", [a, b, c, d])]}\n", "a.json": "[\"x\"]", "p_test.rego": "package docker\n\ntest_a if false\n\ntest_b if false with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("github attestation without resolver", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if github_attestation(input.http, \"o/r\")\n\ndecision := {\"allow\": allow}\n", "p_test.rego": "package docker\n\ntest_a if allow with input as {\"http\": {\"url\": \"https://x/y\", \"checksum\": \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"}}\n\ntest_b if allow with input as {\"http\": {\"url\": \"https://x/y\"}}\n"}))
	add(t("other package image", ".", map[string]string{"Dockerfile.rego": shardsAllow, "p_test.rego": "package other\n\ntest_x if not data.docker.allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("policy file is the builtin", ".", map[string]string{"builtin/buildx_defaults.rego": "package docker\n\ndecision := {\"allow\": true}\n", "p_test.rego": "package docker\n\ntest_a if decision.allow\n\ntest_b if not docker_github_builder({}, \"x\")\n"}))
	cs[len(cs)-1].Filename = "builtin/buildx_defaults"
	add(t("other package runtime unknown", ".", map[string]string{"Dockerfile.rego": "package docker\n\ndefault allow := false\n\nallow if verify_git_signature(input.git, \"k.pub\")\n", "k.pub": "", "p_test.rego": "package other\n\ntest_a if data.docker.allow with input as {\"git\": {\"remote\": \"r\"}}\n"}))
	add(t("policy imports its own package", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.docker\n\ndecision := {\"allow\": true}\n", "p_test.rego": "package docker\n\ntest_a if decision.allow with input as {\"local\": {}}\n\ntest_b if decision.allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("policy imports the builtin's file", ".", map[string]string{"Dockerfile.rego": "package docker\n\nimport data.builtin.buildx_defaults\n\ndecision := {\"allow\": true}\n", "p_test.rego": "package docker\n\ntest_a if decision.allow with input as {\"image\": {\"repo\": \"alpine\"}}\n"}))
	add(t("path with a nul", "a\x00b_test.rego", map[string]string{"Dockerfile.rego": shardsAllow}))
	add(t("policy file invalid name", ".",map[string]string{"p_test.rego": "package docker\n\ntest_a if true\n"}))
	cs[len(cs)-1].Filename = "../x"
	return cs
}

func TestShardsTesterOracle(t *testing.T) {
	out := os.Getenv("SHARDS_POLICY_OUT")
	if out == "" {
		t.Skip("SHARDS_POLICY_OUT names the file to write")
	}
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	var cases []shardsTesterCase
	for _, c := range shardsCases() {
		dir := t.TempDir()
		names := make([]string, 0, len(c.Files))
		for n := range c.Files {
			names = append(names, n)
		}
		sort.Strings(names)
		for _, n := range names {
			p := filepath.Join(dir, filepath.FromSlash(n))
			if strings.HasSuffix(n, "/") {
				if err := os.MkdirAll(p, 0o755); err != nil {
					t.Fatal(err)
				}
				continue
			}
			if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(p, []byte(c.Files[n]), 0o644); err != nil {
				t.Fatal(err)
			}
		}
		if err := os.Chdir(dir); err != nil {
			t.Fatal(err)
		}
		root, ok := os.DirFS(".").(fs.StatFS)
		if !ok {
			t.Fatal("no stat")
		}
		func() {
			defer func() {
				if r := recover(); r != nil {
					c.Panic = fmt.Sprint(r)
				}
			}()
			summary, err := RunPolicyTests(context.Background(), c.Path, TestOptions{Run: c.Run, Filename: c.Filename, Root: root, Provider: shardsProvider()})
			if err != nil {
				c.Err = err.Error()
				c.Status = 1
			} else {
				var b strings.Builder
				c.Status = shardsReport(&b, summary)
				c.Stdout = b.String()
			}
		}()
		if err := os.Chdir(cwd); err != nil {
			t.Fatal(err)
		}
		cases = append(cases, c)
	}
	data, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
