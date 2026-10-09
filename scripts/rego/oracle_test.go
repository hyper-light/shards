package oracle

// What OPA makes of shards-rego's corpus (crates/rego/testdata/cases.json), for
// crates/rego/tests/oracle.rs: each case compiled and run as buildx v0.37.1's policy
// package compiles and runs a policy (policy/validate.go regoBaseOpts and CheckPolicy):
// Rego v1, the capabilities of builtins.go (fetched by generate) with ast.Features and
// buildx's own functions, modules kept, print statements on, partial namespaces skipped,
// and buildx's builtins.rego beside the case's modules.
//
// buildx's own functions answer from the case's "host" table: a call whose arguments,
// as JSON, are a key there gives that value; any other call is undefined. oracle.rs's
// host answers the same way.

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"math/big"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/open-policy-agent/opa/v1/ast"
	"github.com/open-policy-agent/opa/v1/rego"
	opabuiltins "github.com/open-policy-agent/opa/v1/topdown/builtins"
	"github.com/open-policy-agent/opa/v1/topdown/print"
	"github.com/open-policy-agent/opa/v1/types"
)

//go:embed builtins.rego
var builtinsRego string

type Case struct {
	Name    string      `json:"name"`
	Modules [][2]string `json:"modules"`
	Query   string      `json:"query"`
	// Input is the input document, absent for none.
	Input *json.RawMessage `json:"input,omitempty"`
	// Unknowns, when present, runs partial evaluation as CheckPolicy does.
	Unknowns []string `json:"unknowns,omitempty"`
	// Host maps a function name to {arguments as JSON: result}.
	Host map[string]map[string]json.RawMessage `json:"host,omitempty"`
}

type Result struct {
	Name string `json:"name"`
	// Error is the compile or evaluation error's text, if any.
	Error string `json:"error,omitempty"`
	// Results are each result's first expression's value, as JSON.
	Results []json.RawMessage `json:"results"`
	// Support lists the input refs in the support modules partial evaluation made, in
	// walk order, as buildx's collectUnknowns sees them (before its trimming).
	Support []string `json:"support,omitempty"`
	// Queries are the partial queries, as text.
	Queries []string `json:"queries,omitempty"`
	// SupportModules are the support modules' texts, sorted (OPA lists them from a map).
	SupportModules []string `json:"support_modules,omitempty"`
	// Prints are what print statements printed, "file:row: msg", as buildx logs them.
	Prints []string `json:"prints"`
}

type hook struct{ lines []string }

func (h *hook) Print(ctx print.Context, msg string) error {
	h.lines = append(h.lines, ctx.Location.Format("%s", msg))
	return nil
}

// buildx's functions, declared as policy/funcs.go declares them.
var hostDecls = []struct {
	name string
	decl *types.Function
}{
	{"load_json", types.NewFunction(types.Args(types.S), types.A)},
	{"verify_git_signature", types.NewFunction(types.Args(types.A, types.S), types.B)},
	{"verify_http_pgp_signature", types.NewFunction(types.Args(types.A, types.S, types.S), types.B)},
	{"pin_image", types.NewFunction(types.Args(types.A, types.S), types.B)},
	{"artifact_attestation", types.NewFunction(types.Args(types.A, types.S), types.A)},
	{"github_attestation", types.NewFunction(types.Args(types.A, types.S), types.A)},
}

func answer(table map[string]json.RawMessage, args []*ast.Term) (*ast.Term, error) {
	vals := make([]any, 0, len(args))
	for _, a := range args {
		v, err := ast.JSON(a.Value)
		if err != nil {
			return nil, err
		}
		vals = append(vals, v)
	}
	key, err := json.Marshal(vals)
	if err != nil {
		return nil, err
	}
	got, ok := table[string(key)]
	if !ok {
		return nil, nil
	}
	v, err := ast.ValueFromReader(bytes.NewReader(got))
	if err != nil {
		return nil, err
	}
	return ast.NewTerm(v), nil
}

func run(c Case) Result {
	out := Result{Name: c.Name, Results: []json.RawMessage{}, Prints: []string{}}
	caps := &ast.Capabilities{Builtins: builtins(), Features: slices.Clone(ast.Features)}
	for _, h := range hostDecls {
		caps.Builtins = append(caps.Builtins, &ast.Builtin{Name: h.name, Decl: h.decl})
	}
	comp := ast.NewCompiler().WithCapabilities(caps).WithKeepModules(true).WithEnablePrintStatements(true)
	h := &hook{}
	opts := []func(*rego.Rego){
		rego.SetRegoVersion(ast.RegoV1),
		rego.Query(c.Query),
		rego.SkipPartialNamespace(true),
		rego.Compiler(comp),
		rego.Module("builtin/buildx_defaults.rego", builtinsRego),
		rego.EnablePrintStatements(true),
		rego.PrintHook(h),
	}
	for _, m := range c.Modules {
		opts = append(opts, rego.Module(m[0], m[1]))
	}
	for _, hd := range hostDecls {
		table := c.Host[hd.name]
		f := &rego.Function{Name: hd.name, Decl: hd.decl}
		opts = append(opts, rego.FunctionDyn(f, func(_ rego.BuiltinContext, args []*ast.Term) (*ast.Term, error) {
			return answer(table, args)
		}))
	}
	if c.Input != nil {
		var in any
		if err := json.Unmarshal(*c.Input, &in); err != nil {
			out.Error = "bad case input: " + err.Error()
			return out
		}
		opts = append(opts, rego.Input(in))
	}
	ctx := context.Background()
	if c.Unknowns != nil {
		pq, err := rego.New(append(opts, rego.Unknowns(c.Unknowns))...).Partial(ctx)
		if err != nil {
			out.Error = err.Error()
			return out
		}
		for _, mod := range pq.Support {
			ast.WalkRefs(mod, func(ref ast.Ref) bool {
				if ref.HasPrefix(ast.InputRootRef) {
					out.Support = append(out.Support, ref.String())
				}
				return true
			})
		}
		for _, q := range pq.Queries {
			out.Queries = append(out.Queries, q.String())
		}
		for _, mod := range pq.Support {
			out.SupportModules = append(out.SupportModules, mod.String())
		}
		slices.Sort(out.SupportModules)
		h.lines = nil
	}
	rs, err := rego.New(opts...).Eval(ctx)
	out.Prints = append(out.Prints, h.lines...)
	if err != nil {
		out.Error = err.Error()
		return out
	}
	for _, r := range rs {
		dt, err := json.Marshal(r.Expressions[0].Value)
		if err != nil {
			out.Error = "bad result: " + err.Error()
			return out
		}
		out.Results = append(out.Results, dt)
	}
	return out
}

func TestShardsRego(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_REGO_CASES"))
	if err != nil {
		t.Fatal(err)
	}
	var cases []Case
	dec := json.NewDecoder(bytes.NewReader(dt))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&cases); err != nil {
		t.Fatal(err)
	}
	var buf bytes.Buffer
	buf.WriteString("[\n")
	for i, c := range cases {
		r := run(c)
		line, err := json.Marshal(r)
		if err != nil {
			t.Fatal(err)
		}
		buf.Write(line)
		if i+1 < len(cases) {
			buf.WriteString(",")
		}
		buf.WriteString("\n")
	}
	buf.WriteString("]\n")
	if strings.TrimSpace(os.Getenv("SHARDS_REGO_OUT")) == "" {
		t.Fatal("SHARDS_REGO_OUT is not set")
	}
	if err := os.WriteFile(os.Getenv("SHARDS_REGO_OUT"), buf.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}

// What Go's big.Float and OPA make of each number in numbers.json, and of each pair:
// for crates/rego's number.rs.
type numberCase struct {
	S     string `json:"s"`
	Ok    bool   `json:"ok"`
	Text  string `json:"text,omitempty"`
	IsInt bool   `json:"is_int,omitempty"`
	Int   string `json:"int,omitempty"`
	Exact bool   `json:"exact,omitempty"`
	Neg   bool   `json:"neg,omitempty"`
}

type pairCase struct {
	A       string `json:"a"`
	B       string `json:"b"`
	Add     string `json:"add"`
	Sub     string `json:"sub"`
	Mul     string `json:"mul"`
	Quo     string `json:"quo,omitempty"`
	Cmp     int    `json:"cmp"`
	Compare int    `json:"compare"`
}

func TestShardsNumbers(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_NUMBERS"))
	if err != nil {
		t.Fatal(err)
	}
	var in struct {
		Numbers []string    `json:"numbers"`
		Pairs   [][2]string `json:"pairs"`
		Quote   []string    `json:"quote"`
	}
	if err := json.Unmarshal(dt, &in); err != nil {
		t.Fatal(err)
	}
	var out struct {
		Numbers []numberCase `json:"numbers"`
		Pairs   []pairCase   `json:"pairs"`
		Quote   [][2]string  `json:"quote"`
	}
	for _, s := range in.Numbers {
		c := numberCase{S: s}
		f, ok := new(big.Float).SetString(s)
		if ok {
			c.Ok = true
			c.Text = string(opabuiltins.FloatToNumber(f))
			c.IsInt = f.IsInt()
			i, acc := f.Int(nil)
			c.Int = i.String()
			c.Exact = acc == big.Exact
			c.Neg = f.Signbit()
		}
		out.Numbers = append(out.Numbers, c)
	}
	for _, p := range in.Pairs {
		a := opabuiltins.NumberToFloat(ast.Number(p[0]))
		b := opabuiltins.NumberToFloat(ast.Number(p[1]))
		c := pairCase{A: p[0], B: p[1]}
		c.Add = string(opabuiltins.FloatToNumber(new(big.Float).Add(a, b)))
		c.Sub = string(opabuiltins.FloatToNumber(new(big.Float).Sub(a, b)))
		c.Mul = string(opabuiltins.FloatToNumber(new(big.Float).Mul(a, b)))
		if b.Sign() != 0 {
			c.Quo = string(opabuiltins.FloatToNumber(new(big.Float).Quo(a, b)))
		}
		c.Cmp = a.Cmp(b)
		c.Compare = ast.NumberCompare(ast.Number(p[0]), ast.Number(p[1]))
		out.Pairs = append(out.Pairs, c)
	}
	for _, q := range in.Quote {
		out.Quote = append(out.Quote, [2]string{q, strconv.Quote(q)})
	}
	res, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_NUMBERS_OUT"), append(res, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// OPA's builtins, as the crate's src/builtins.json: each one's name, infix operator,
// whether it is a relation or deprecated, its declaration as OPA marshals it, and
// whether buildx's policies may call it (builtins.go), and whether it is
// nondeterministic.
func TestShardsBuiltins(t *testing.T) {
	allowed := map[string]bool{}
	for _, b := range builtins() {
		allowed[b.Name] = true
	}
	type entry struct {
		Name       string `json:"name"`
		Infix      string `json:"infix,omitempty"`
		Relation   bool   `json:"relation,omitempty"`
		Deprecated bool   `json:"deprecated,omitempty"`
		Allowed    bool   `json:"allowed,omitempty"`
		// Nondeterministic builtins are left for later by partial evaluation.
		Nondeterministic bool            `json:"nondeterministic,omitempty"`
		Decl             json.RawMessage `json:"decl"`
	}
	var out []entry
	for _, b := range ast.Builtins {
		raw, err := json.Marshal(b.Decl)
		if err != nil {
			t.Fatal(err)
		}
		var tree any
		if err := json.Unmarshal(raw, &tree); err != nil {
			t.Fatal(err)
		}
		decl, err := json.Marshal(dropDescriptions(tree))
		if err != nil {
			t.Fatal(err)
		}
		out = append(out, entry{Name: b.Name, Infix: b.Infix, Relation: b.Relation, Deprecated: b.IsDeprecated(), Allowed: allowed[b.Name], Nondeterministic: b.Nondeterministic, Decl: decl})
	}
	res, err := json.Marshal(out)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_BUILTINS_OUT"), append(res, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// dropDescriptions removes the documentation from a declaration: nothing reads it.
func dropDescriptions(x any) any {
	switch v := x.(type) {
	case map[string]any:
		delete(v, "description")
		for k, e := range v {
			v[k] = dropDescriptions(e)
		}
	case []any:
		for i, e := range v {
			v[i] = dropDescriptions(e)
		}
	}
	return x
}

// What OPA's parser makes of each module in parse.json, with buildx's capabilities:
// the module's text (Module.String), or its errors' text (ast.Errors.Error).
func TestShardsParse(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_PARSE"))
	if err != nil {
		t.Fatal(err)
	}
	var in []struct {
		Name string `json:"name"`
		Src  string `json:"src"`
	}
	if err := json.Unmarshal(dt, &in); err != nil {
		t.Fatal(err)
	}
	caps := &ast.Capabilities{Builtins: builtins(), Features: slices.Clone(ast.Features)}
	for _, h := range hostDecls {
		caps.Builtins = append(caps.Builtins, &ast.Builtin{Name: h.name, Decl: h.decl})
	}
	type result struct {
		Name   string `json:"name"`
		Module string `json:"module,omitempty"`
		Error  string `json:"error,omitempty"`
	}
	var out []result
	for _, c := range in {
		r := result{Name: c.Name}
		m, err := ast.ParseModuleWithOpts("p.rego", c.Src, ast.ParserOptions{RegoVersion: ast.RegoV1, Capabilities: caps})
		if err != nil {
			r.Error = err.Error()
		} else {
			r.Module = m.String()
		}
		out = append(out, r)
	}
	res, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_PARSE_OUT"), append(res, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// What OPA's compiler, set up as buildx's (capabilities, modules kept, print
// statements on), makes of each case's modules beside buildx's builtins.rego: each
// compiled module's text, by name, or the compile errors.
func TestShardsCompile(t *testing.T) {
	dt, err := os.ReadFile(os.Getenv("SHARDS_REGO_CASES"))
	if err != nil {
		t.Fatal(err)
	}
	var cases []Case
	if err := json.Unmarshal(dt, &cases); err != nil {
		t.Fatal(err)
	}
	caps := &ast.Capabilities{Builtins: builtins(), Features: slices.Clone(ast.Features)}
	for _, h := range hostDecls {
		caps.Builtins = append(caps.Builtins, &ast.Builtin{Name: h.name, Decl: h.decl})
	}
	type result struct {
		Name    string            `json:"name"`
		Modules map[string]string `json:"modules,omitempty"`
		Error   string            `json:"error,omitempty"`
	}
	var out []result
	for _, c := range cases {
		r := result{Name: c.Name}
		popts := ast.ParserOptions{RegoVersion: ast.RegoV1, Capabilities: caps}
		mods := map[string]*ast.Module{}
		srcs := append([][2]string{{"builtin/buildx_defaults.rego", builtinsRego}}, c.Modules...)
		var perr error
		for _, m := range srcs {
			mod, err := ast.ParseModuleWithOpts(m[0], m[1], popts)
			if err != nil {
				perr = err
				break
			}
			mods[m[0]] = mod
		}
		if perr != nil {
			r.Error = "parse: " + perr.Error()
			out = append(out, r)
			continue
		}
		comp := ast.NewCompiler().WithCapabilities(caps).WithKeepModules(true).WithEnablePrintStatements(true)
		comp.Compile(mods)
		if comp.Failed() {
			r.Error = comp.Errors.Error()
		} else {
			r.Modules = map[string]string{}
			for name, m := range comp.Modules {
				r.Modules[name] = m.String()
			}
		}
		out = append(out, r)
	}
	res, err := json.MarshalIndent(out, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_COMPILE_OUT"), append(res, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
