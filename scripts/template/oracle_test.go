package oracle

// What Go's text/template makes of shards-template's corpus, for
// crates/template/tests/oracle.rs: each case parsed as docker/cli's templates.Parse
// parses a --format (templates.go's basicFunctions, fetched by generate at docker/cli
// v29.8.1), and run against its data with Execute, or, for header cases, as the CLI's
// formatter runs a table's header (Funcs(templates.HeaderFunctions)). Data is JSON with
// tags for the Go types JSON has no word for: {"$uint": n}, {"$strings": [...]},
// {"$strmap": {...}}, and {"$object": "Person" | "Container", ...} for the structs below,
// which oracle.rs mirrors as Objects.

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"testing"
	"text/template"

	"shards.oracle/templates"
)

// Person is a struct value with value-receiver methods.
type Person struct {
	Name string
	Age  int
	Tags []string
	Meta map[string]string
}

func (p Person) Greet(greeting string) string { return greeting + ", " + p.Name }
func (p Person) Initial() string {
	if p.Name == "" {
		return ""
	}
	return p.Name[:1]
}
func (p Person) Fail() (string, error)   { return "", errors.New("person failed") }
func (p Person) Sum(a, b int) int        { return a + b }
func (p Person) Label(key string) string { return p.Meta[key] }
func (p Person) Scale(f float64) float64 { return float64(p.Age) * f }
func (p Person) Older(n uint) bool       { return uint(p.Age) > n }
func (p Person) Pick(b bool) string {
	if b {
		return "yes"
	}
	return "no"
}
func (p Person) Kind(v any) string { return fmt.Sprintf("%T", v) }

// Container is a pointer with unexported fields and methods only, as docker/cli's
// formatter contexts are.
type Container struct {
	id     string
	names  []string
	labels map[string]string
}

func (c *Container) ID() string    { return c.id }
func (c *Container) Names() string { return strings.Join(c.names, ",") }
func (c *Container) Label(name string) string {
	if c.labels == nil {
		return ""
	}
	return c.labels[name]
}
func (c *Container) Labels() string {
	var out []string
	for k, v := range c.labels {
		out = append(out, k+"="+v)
	}
	sort.Strings(out)
	return strings.Join(out, ",")
}

func convert(v any) any {
	switch x := v.(type) {
	case json.Number:
		if strings.ContainsAny(x.String(), ".eE") {
			f, _ := x.Float64()
			return f
		}
		i, _ := x.Int64()
		return int(i)
	case []any:
		out := make([]any, len(x))
		for i, e := range x {
			out[i] = convert(e)
		}
		return out
	case map[string]any:
		if n, ok := x["$uint"]; ok {
			u, _ := strconv.ParseUint(n.(json.Number).String(), 10, 64)
			return uint(u)
		}
		if l, ok := x["$strings"]; ok {
			return strs(l)
		}
		if m, ok := x["$strmap"]; ok {
			return strmap(m)
		}
		switch x["$object"] {
		case "Person":
			p := Person{Name: x["Name"].(string), Age: convert(x["Age"]).(int)}
			if t, ok := x["Tags"]; ok {
				p.Tags = strs(t)
			}
			if m, ok := x["Meta"]; ok {
				p.Meta = strmap(m)
			}
			return p
		case "Container":
			return &Container{id: x["ID"].(string), names: strs(x["Names"]), labels: strmap(x["Labels"])}
		}
		out := make(map[string]any, len(x))
		for k, e := range x {
			out[k] = convert(e)
		}
		return out
	}
	return v
}

func strs(v any) []string {
	l := v.([]any)
	out := make([]string, len(l))
	for i, e := range l {
		out[i] = e.(string)
	}
	return out
}

func strmap(v any) map[string]string {
	out := map[string]string{}
	for k, e := range v.(map[string]any) {
		out[k] = e.(string)
	}
	return out
}

const person = `{"$object": "Person", "Name": "Ann", "Age": 30, "Tags": ["admin", "dev"], "Meta": {"team": "core"}}`
const container = `{"$object": "Container", "ID": "abc123def456", "Names": ["web", "app"], "Labels": {"com.docker.compose.project": "proj", "env": "prod"}}`

var data = map[string]string{
	"nil":       `null`,
	"int":       `3`,
	"str":       `"just a string"`,
	"list":      `["x", "y"]`,
	"person":    person,
	"container": container,
	"header":    `{"$strmap": {"ID": "CONTAINER ID", "Names": "NAMES", "Image": "IMAGE"}}`,
	"m": `{
		"str": "hello", "empty": "", "int": 42, "neg": -7, "zero": 0, "n5": 5,
		"float": 3.5, "fint": 2.0, "big": 1e21, "small": 0.000001234, "t": true, "f": false, "null": null,
		"list": ["a", "b", "c"], "ints": [1, 2, 3], "emptylist": [],
		"mixed": [1, "two", 3.5, true, null, {"k": "v"}, [1, 2]],
		"map": {"b": 2, "a": 1, "c": 3}, "emptymap": {},
		"nested": {"inner": {"deep": "value"}, "list": [{"name": "x"}, {"name": "y"}], "null": null},
		"u": {"$uint": 7}, "strs": {"$strings": ["x", "y", "z"]},
		"labels": {"$strmap": {"com.example.a": "1", "b": "two", "empty": ""}},
		"html": "<a href=\"x\">&'</a>", "unicode": "héllo wörld ñ 日本", "spaces": "  padded  ",
		"multi": "line1\nline2\ttab", "csv": "a,b,,c", "with-dash": "dash",
		"person": ` + person + `, "container": ` + container + `,
		"people": [` + person + `, {"$object": "Person", "Name": "Bob", "Age": 5, "Tags": [], "Meta": {}}]
	}`,
}

type oracleCase struct {
	Name     string `json:"name,omitempty"`
	Data     string `json:"data"`
	Template string `json:"template"`
	Header   bool   `json:"header,omitempty"`
	Output   string `json:"output"`
	Error    string `json:"error,omitempty"`
}

var cases []oracleCase

func add(d string, ts ...string) {
	for _, t := range ts {
		cases = append(cases, oracleCase{Data: d, Template: t})
	}
}

func named(name, d string, ts ...string) {
	for _, t := range ts {
		cases = append(cases, oracleCase{Name: name, Data: d, Template: t})
	}
}

func header(d string, ts ...string) {
	for _, t := range ts {
		cases = append(cases, oracleCase{Data: d, Template: t, Header: true})
	}
}

func corpus() {
	// Text, constants, comments and trim markers.
	add("m", "", "plain text", "unicode ✓ 日本", "{{/* comment */}}x", "a {{- /* c */ -}} b",
		`{{"str"}}`, "{{`raw`}}", "{{`multi\nline`}}", `{{"\t\"quoted\""}}`, `{{"é\U0001F600\x41\101"}}`,
		`{{"\q"}}`, "{{1}}", "{{-1}}", "{{+5}}", "{{-0}}", "{{1.5}}", "{{1.0}}", "{{.5}}", "{{1e3}}", "{{3.0e0}}",
		"{{0x1F}}", "{{0X1f}}", "{{0o17}}", "{{017}}", "{{0b101}}", "{{1_000_000}}", "{{0_1}}", "{{0x1p4}}",
		"{{'a'}}", `{{'\n'}}`, `{{'\x41'}}`, `{{'é'}}`, "{{'ab'}}", "{{''}}", "{{true}}", "{{false}}",
		"{{nil}}", "{{9223372036854775807}}", "{{9223372036854775808}}", "{{-9223372036854775808}}",
		"{{18446744073709551616}}", "{{0x7fffffffffffffff}}", "{{1e400}}", "{{089}}", "{{0x}}", "{{1.2.3}}", "{{3x}}",
		"{{1_}}", "{{_1}}",
		"a  {{- .str -}}  b", "a\n{{- .str}}\n b", "{{- .str}}", "{{.str -}}\n\n", "{{ .str }}", "{{- 3 -}}",
		"x {{-3}}", "{{3 -}} x", "a \t\n{{- /* c */}} b", "{{/* c */ -}}\n\n b", "{{-/* c */}}",
		"{{.str}}{{.str}}", "{{.str}} and {{.int}}")
	// Fields, chains and missing values.
	add("m", "{{.str}}", "{{.Str}}", "{{.missing}}", "{{.null}}", "{{.nested.inner.deep}}",
		"{{.nested.missing.deep}}", "{{.nested.null}}", "{{.null.x}}", "{{.nested.null.x}}", "{{.str.x}}",
		"{{.list.x}}", "{{.int.x}}", "{{.u.x}}", "{{.labels.b}}", "{{.labels.zz}}", "{{.labels.zz.x}}",
		"{{.map}}", "{{.list}}", "{{.ints}}", "{{.mixed}}", "{{.emptylist}}", "{{.emptymap}}", "{{.nested}}",
		"{{.nested.list}}", "{{.float}}", "{{.fint}}", "{{.big}}", "{{.small}}", "{{.neg}}", "{{.u}}", "{{.strs}}",
		"{{.labels}}", "{{.t}} {{.f}}", "{{.html}}", "{{.multi}}", "{{.with-dash}}", "{{.str.x.y}}",
		"{{(.nested).inner.deep}}", `{{(index .nested "inner").deep}}`, "{{(len .list)}}", "{{(1)}}",
		"{{(.missing).x}}", "{{(nil).x}}", "{{nil.x}}", `{{"s".x}}`, "{{1.x}}", "{{..x}}", "{{.str|printf \"%s!\"}}",
		`{{"a" | printf "%s-%s" "b"}}`, "{{.list | len}}", "{{3 | 4}}", `{{. | "x"}}`, "{{. | .str}}",
		"{{.str 1}}", "{{1 2}}", `{{$x := 1}}{{$x 2}}`, "{{(1) 2}}", "{{(.list) 2}}", "{{.missing 1}}",
		"{{.list | .x}}", "{{.nested.inner | .deep}}", "{{len .list | printf \"%d items\"}}")
	// Variables.
	add("m", "{{$x := 1}}{{$x}}", `{{$x := .str}}{{$x = "new"}}{{$x}}`, "{{$.str}}",
		"{{with .nested}}{{$.str}}{{end}}", "{{$x := 1}}{{if true}}{{$x := 2}}{{$x}}{{end}}{{$x}}",
		"{{$x := 1}}{{if true}}{{$x = 2}}{{end}}{{$x}}", "{{$y}}", "{{$x = 1}}", "{{$x := 1}}{{$x.y}}",
		"{{$p := .person}}{{$p.Name}}", "{{$x := .missing}}{{$x}}", "{{$x := .null}}{{$x.y}}",
		"{{$x := .missing}}{{$x.y}}", "{{$x := 3}}{{range $x = .ints}}{{$x}}{{end}}{{$x}}",
		"{{$x, $y := 1}}", "{{$x := 1}}{{$x := 2}}{{$x}}", "{{$x := .nested}}{{$x.inner.deep}}",
		"{{$x := .str}}{{len $x}}", "{{$ := 1}}", "{{$x:=1}}{{$x}}", "{{$x :=}}", "{{:}}", "{{$x := 1 | printf \"%d!\"}}{{$x}}",
		"{{$x := 1}}{{range .list}}{{$x}}{{end}}", "{{range .list}}{{$y := .}}{{end}}{{$y}}")
	// if, else, else if, with.
	for _, f := range []string{".t", ".f", ".empty", ".str", ".zero", ".int", ".float", ".neg", ".emptylist", ".list",
		".emptymap", ".map", ".null", ".missing", ".person", ".container", ".u", "0.0", "nil", ".labels.empty"} {
		add("m", "{{if "+f+"}}yes{{else}}no{{end}}")
	}
	add("m", "{{if .null.x}}y{{end}}", "{{if and .t .f}}y{{else}}n{{end}}", "{{if or .f .str}}y{{end}}",
		"{{if not .f}}y{{end}}", "{{if .f}}a{{else if .zero}}b{{else if .int}}c{{else}}d{{end}}",
		"{{if .f}}a{{else if .zero}}b{{end}}", `{{if eq .str "hello" "x"}}y{{end}}`, "{{if}}", "{{if .t}}x",
		"{{else}}", "{{end}}", "{{if .t}}a{{else}}b{{else}}c{{end}}", "{{if 1}}{{else if}}{{end}}",
		"{{if .t}}{{$v := 1}}{{end}}{{$v}}", "{{if $v := .str}}{{$v}}{{end}}", "{{if 1 2}}{{end}}",
		"{{with .str}}{{.}}{{end}}", "{{with .missing}}x{{else}}none{{end}}", "{{with .nested.inner}}{{.deep}}{{end}}",
		"{{with $x := .str}}{{$x}}{{.}}{{end}}", "{{with .f}}a{{else with .str}}{{.}}{{end}}",
		"{{with .zero}}{{else with .emptylist}}{{else}}nothing{{end}}", "{{with}}{{end}}",
		"{{with .null}}x{{else}}{{.str}}{{end}}", "{{with .person}}{{.Name}} is {{.Age}}{{end}}",
		"{{with .list}}{{index . 0}}{{end}}", "{{with 0}}{{end}}after")
	// range.
	add("m", "{{range .list}}[{{.}}]{{end}}", "{{range .ints}}{{.}},{{end}}", "{{range .emptylist}}x{{else}}empty{{end}}",
		"{{range .map}}{{.}}{{end}}", "{{range $k, $v := .map}}{{$k}}={{$v}};{{end}}", "{{range .emptymap}}x{{else}}none{{end}}",
		"{{range .mixed}}[{{.}}]{{end}}", "{{range .mixed}}{{.k}}{{end}}", "{{range .strs}}{{.x}}{{end}}",
		"{{range .str}}{{end}}", "{{range .n5}}{{.}}{{end}}", "{{range 3}}{{.}}{{end}}", "{{range 0}}x{{else}}empty{{end}}",
		"{{range -2}}x{{else}}neg{{end}}", "{{range $i, $v := 3}}{{end}}", "{{range $v := 3}}{{$v}}{{end}}",
		"{{range .u}}{{.}} {{end}}", "{{range .missing}}x{{else}}none{{end}}", "{{range .null}}x{{else}}none{{end}}",
		"{{range .t}}{{end}}", "{{range .float}}{{end}}", "{{range .person}}{{end}}", "{{range .labels}}{{.}};{{end}}",
		"{{range $k, $v := .labels}}{{$k}}:{{$v}} {{end}}", "{{range $i, $v := .list}}{{$i}}:{{$v}} {{end}}",
		"{{range $v := .list}}{{$v}}{{end}}", "{{range $i, $e := .nested.list}}{{$i}}{{$e.name}}{{end}}",
		"{{range .ints}}{{if eq . 2}}{{break}}{{end}}{{.}}{{end}}", "{{range .ints}}{{if eq . 2}}{{continue}}{{end}}{{.}}{{end}}",
		"{{range .list}}{{range $.ints}}{{if eq . 2}}{{break}}{{end}}{{.}}{{end}}{{.}};{{end}}",
		"{{break}}", "{{continue}}", "{{range .ints}}{{break 1}}{{end}}", "{{if .t}}{{break}}{{end}}",
		"{{range .list}}{{with .}}{{continue}}{{end}}x{{end}}", "{{range .ints}}{{$.n5}}{{end}}",
		"{{range}}{{end}}", "{{range $a, $b, $c := .list}}{{end}}", "{{range $i, := .list}}{{end}}",
		"{{range $i, $v := .list}}{{$i}}{{end}}{{$i}}", "{{$i := 9}}{{range $i = .list}}{{end}}{{$i}}",
		"{{$i := 0}}{{$v := 0}}{{range $i, $v = .ints}}{{end}}{{$i}}{{$v}}", "{{range .list}}{{else}}{{else}}{{end}}",
		"{{range .people}}{{.Name}},{{end}}", "{{range .people}}{{.Greet \"hey\"}};{{end}}",
		"{{range $i, $p := .people}}{{$i}}{{$p.Age}}{{end}}", "{{range .nested}}{{.}};{{end}}")
	// define, template, block.
	add("m", `{{define "T"}}hi {{.}}{{end}}{{template "T" .str}}`, `{{template "T"}}`,
		`{{define "T"}}{{.}}{{end}}{{template "T"}}`, `{{block "B" .str}}[{{.}}]{{end}}`,
		`{{define "a"}}A{{end}}{{define "a"}}B{{end}}`, `{{define "a"}}A{{end}}`, `{{define "a"}}  {{end}}{{define "a"}}B{{end}}{{template "a"}}`,
		`{{$x := 1}}{{define "T"}}{{$x}}{{end}}`, `{{define "T"}}{{$}}{{end}}{{template "T" .int}}`,
		`{{define "r"}}{{if .}}{{len .}}{{template "r" (slice . 1)}}{{end}}{{end}}{{template "r" .list}}`,
		"{{define}}", `{{define 1}}{{end}}`, "{{template .str}}", "{{template}}", `{{define "x"}}`,
		`{{define "T"}}{{.y.z}}{{end}}{{template "T" .nested}}`, `{{define "T"}}{{.str.x}}{{end}}{{template "T" .}}`,
		`{{template "T" .}}{{define "T"}}late{{end}}`, `{{define "T" }}ok{{end}}{{template "T" }}`,
		`{{define "T"}}{{.}}{{end}}{{template "T" 1 | printf "%d!"}}`, `{{template "T" .str .int}}`,
		`{{define "T"}}{{break}}{{end}}`, `{{define "T"}}{{else}}{{end}}`, `{{block "B"}}{{end}}`,
		`{{block "B" .}}{{.str}}{{end}}{{define "B"}}override{{end}}`, `{{define "B"}}first{{end}}{{block "B" .}}second{{end}}`,
		`{{define "a"}}{{template "b" .}}{{end}}{{define "b"}}B{{.int}}{{end}}{{template "a" .}}`,
		"{{define `raw`}}R{{end}}{{template `raw`}}", `{{define "T"}}{{.missing.x}}{{end}}{{template "T" .}}`,
		`{{template "T" $x := 1}}`, `{{define "T"}}{{template "T" .}}{{end}}{{template "T" .}}`)
	// Builtins: and, or, not.
	add("m", "{{and 1 2}}", "{{and 0 2}}", `{{and 1 ""}}`, `{{or 0 "" .str}}`, `{{or 0 ""}}`, "{{or}}", "{{and}}",
		"{{and .missing 1}}", "{{or .missing .null}}", "{{not 0}}", "{{not .list}}", "{{not}}", "{{not 1 2}}",
		"{{.f | and 1}}", "{{.t | or 0}}", "{{.t | and 1}}", "{{and 1 .null.x}}", "{{or 1 .null.x}}",
		"{{and .str .list .map}}", "{{or .emptylist .emptymap .person}}", "{{not .null}}", "{{and nil 1}}")
	// len, index, slice.
	add("m", "{{len .list}}", "{{len .map}}", "{{len .str}}", "{{len .unicode}}", "{{len .emptylist}}",
		"{{len .int}}", "{{len .missing}}", "{{len .null}}", "{{len nil}}", "{{len .person}}", "{{len .strs}}",
		"{{len .labels}}", "{{len}}", "{{len 1 2}}", `{{len "abc"}}`, "{{len .container}}",
		"{{index .list 1}}", "{{index .list 5}}", "{{index .list 3}}", "{{index .list -1}}", `{{index .map "a"}}`,
		`{{index .map "zz"}}`, `{{index .labels "zz"}}`, `{{index .labels "b"}}`, `{{index .labels "com.example.a"}}`,
		`{{index .nested "list" 1 "name"}}`, "{{index .str 1}}", "{{index .int 1}}", `{{index .list "a"}}`,
		"{{index .map 1}}", "{{index .missing 1}}", "{{index .list}}", "{{index .mixed 4 1}}", "{{index .list .u}}",
		"{{index .map nil}}", "{{index .list 1.0}}", "{{index .mixed 6 1}}", "{{index .mixed 5 \"k\"}}",
		"{{index}}", "{{index .person 0}}", "{{index .strs 2}}", "{{index . \"with-dash\"}}", "{{index .null 1}}",
		"{{index .list .int}}", `{{index .nested "null" "x"}}`, "{{index .ints 0 | printf \"%T\"}}", "{{index .str 1 | printf \"%T\"}}",
		"{{slice .list 1}}", "{{slice .list 1 2}}", "{{slice .list}}", "{{slice .str 1 3}}", "{{slice .str 1 2 3}}",
		"{{slice .list 0 1 2}}", "{{slice .list 2 1}}", "{{slice .list 0 4}}", "{{slice .list 0 3}}", "{{slice .int 1}}",
		"{{slice .missing}}", "{{slice .list 1 2 3 4}}", "{{slice .list .u}}", "{{slice .list 1 2 1}}",
		"{{slice .strs 1}}", "{{slice .unicode 0 2}}", "{{slice .str -1}}", "{{slice .str \"a\"}}", "{{slice .null}}")
	// print, println, printf.
	add("m", `{{print 1 2 "a" "b" 3}}`, "{{print}}", `{{println 1 "a"}}`, "{{print .null}}", "{{print .missing}}",
		"{{print .list .map}}", "{{print .person}}", "{{print .str .int .float .t}}", "{{print 1.5 2}}", "{{println}}",
		"{{print .mixed}}", "{{print .nested}}", "{{print nil}}", "{{print .labels .strs .u}}", "{{println .null .missing}}")
	for _, p := range []string{
		`"%d" .int`, `"%5d|%-5d|%05d" 42 42 42`, `"%x %X %o %b %c %U %q" 255 255 8 5 65 9731 65`,
		`"%#x %#o %#U %#b %#X" 255 8 9731 5 255`, `"%+d % d %+d" 5 5 -5`, `"%s|%10s|%-10s|%.2s|" .str .str .str .str`,
		`"%q" .html`, `"%#q" .str`, `"%#q" .multi`, `"%+q" .unicode`, `"%q" .unicode`, `"%x" .str`, `"% x" .str`,
		`"%#X" .str`, `"%# x" .str`, `"%v %v %v %v" .float .fint .big .small`, `"%f %.2f %8.3f %-8.1f|" .float .float .float .float`,
		`"%e %E %.3e" 1234.5678 0.000123 .float`, `"%g %G %.3g %g" 1234.5678 1e-7 3.14159 100000000.0`, `"%t %t" .t .f`,
		`"%v" .list`, `"%q" .list`, `"%d" .ints`, `"%v" .map`, `"%#v" .map`, `"%#v" .list`, `"%#v" .str`, `"%#v" .u`,
		`"%#v" .int`, `"%#v" .mixed`, `"%#v" .float`, `"%#v" .strs`, `"%#v" .labels`,
		`"%T %T %T %T %T %T" .str .int .float .list .map .null`, `"%T" .person`, `"%T" .labels`, `"%T" .container`,
		`"%T" .u`, `"%T" .strs`, `"%T" .missing`, `"%d" .str`, `"%s" .int`, `"%d %d" 1`, `"%d" 1 2`, `"%!" 1`, `"%"`,
		`"%[2]d %[1]d" 1 2`, `"%[3]d" 1 2`, `"%[0]d" 1`, `"%[1]d %d" 1 2`, `"%*d" 5 42`, `"%-*d|" 5 42`, `"%.*f" 2 3.14159`,
		`"%*d" "x" 42`, `"%.*d" "x" 42`, `"%v" nil`, `"%d" nil`, `"%s" .missing`, `"%s" .null`, `"%5t|%-7v|" true false`,
		`"%08.3f|%+.2f|% .2f" 3.14159 3.14159 3.14159`, `"%x" -255`, `"%c%c" 72 105`, `"%q" 'x'`, `"%v" 'x'`,
		`"%U" 0x1F600`, `"%06.2f" -1.5`, `"%10.4v|" .str`, `"%-20s|" .unicode`, `"%5.1q" .str`, `"%%"`, `"%d%%" 50`,
		`"%v" .person`, `"%s" .strs`, `"%v" .labels`, `"%w" 1`, `"%e" 0.0`, `"%g" 0.0`, `"%.0f %.0f %.0f" 0.5 1.5 2.5`,
		`"%5.2s|" "日本語"`, `"%b" 1.0`, `"%v" 1e100`, `"%v" 123456789.0`, `"%v" 0.1`, `"%.3v" 3.14159`, `"%#g" 1.0`,
		`"%+v" 1.5`, `"%o %O" 8 8`, `"%.3d" 7`, `"%.0d|" 0`, `"%5.0d|" 0`, `"%x" .ints`, `"%5v" .ints`, `"%-4d|" .ints`,
		`"%s" .t`, `"%d" .float`, `"%f" .int`, `"%c" .str`, `"%v %v" .neg .u`, `"%08d" -42`, `"%-08d|" 42`, `"%+08d" 42`,
		`"% 8d" 42`, `"%#8x" 255`, `"%010.3f" -3.14159`, `"%-10.3e|" 1234.5`, `"%G" 1e-10`, `"%.10g" 1.0`, `"%.1e" 9.96`,
		`"%5c|" 65`, `"%-5q|" 65`, `"%.2x" .str`, `"%10x|" .str`, `"%#v" .null`, `"%v" .emptylist`, `"%v" .emptymap`,
		`"%t" 1`, `"%s" 'a'`, `"%U" -1`, `"%c" 1114112`, `"%x" 18446744073709551615`, `"%v %d" .u .u`, `"%#v" .t`,
		`"%6.2f%%" 12.345`, `"%s %s" .str`, `"%[2]*[1]d" 42 6`, `"%[1]*d" 3 4`, `"%.[2]d" 7 2`, `"%3[1]d" 1`,
		`"%-+5d|" 3`, `"%T" 1.5`, `"%v" .nested.null`, `"%q" .multi`, `"%x" ""`, `"%5x|" ""`, `"%.0s|" .str`,
		`"%e" 1e21`, `"%f" 1e21`, `"%g" 1e21`, `"%v" 1e20`, `"%v" 1e-5`, `"%v" 0.0001`, `"%v" -0.0`, `"%.2g" 0.000012345`,
		`"%8.2e|" -0.000123`, `"%+.1e" 5.0`, `"%#.0f" 3.0`, `"%#.0e" 3.0`, `"%#x" .str`,
	} {
		add("m", "{{printf "+p+"}}")
	}
	add("m", "{{printf 3}}", "{{printf}}", "{{printf .int}}", "{{1 | printf}}", "{{printf .missing}}", "{{printf nil}}")
	// html, js, urlquery.
	add("m", "{{html .html}}", "{{js .html}}", "{{urlquery .html}}", `{{urlquery "a b&c=d/é~_.-"}}`, "{{html 1 2}}",
		"{{html .missing}}", `{{js "a\nb \t\u0001"}}`, "{{js .unicode}}", `{{html "\x00"}}`, "{{.html | html}}",
		"{{urlquery .missing 1}}", "{{html}}", "{{js .list}}", "{{urlquery .map}}", `{{js "</script>"}}`, `{{html "a" "b"}}`,
		`{{html "a" 1}}`, `{{js "'\"\\="}}`, "{{html .person}}", "{{urlquery .null}}")
	// Comparisons.
	add("m", "{{eq 1 1}}", "{{eq 1 2}}", `{{eq .str "hello"}}`, "{{eq .int 42}}", "{{eq 1 2 3 1}}", "{{eq .u 7}}",
		"{{eq .int .u}}", `{{eq 1 "1"}}`, "{{eq .missing 1}}", "{{eq .missing .null}}", "{{eq .null nil}}",
		"{{eq .list .list}}", "{{eq .list .map}}", "{{eq .float 3.5}}", "{{eq .fint 2}}", "{{eq 1}}", "{{eq}}",
		"{{eq .t true}}", "{{eq .container .container}}", "{{eq .list nil}}", "{{eq 1 .list}}", "{{eq .neg .u}}",
		"{{eq .map .map}}", `{{eq "a" "b" "a"}}`, `{{eq 1 2 "x"}}`, `{{eq 1 1 "x"}}`, "{{eq .strs .list}}",
		"{{ne 1 2}}", `{{ne "a" "a"}}`, `{{ne 1 "a"}}`, "{{ne 1}}", "{{ne .missing .missing}}",
		"{{lt 1 2}}", "{{lt 2 1}}", `{{lt "a" "b"}}`, "{{lt 1.5 2.5}}", "{{lt .u 10}}", "{{lt -1 .u}}", "{{lt .u -1}}",
		"{{lt .t .f}}", "{{lt .list 1}}", `{{lt 1 "a"}}`, "{{lt .missing 1}}", "{{lt 1 .missing}}", "{{lt 1.5 2}}",
		"{{le 2 2}}", "{{le 3 2}}", `{{le "b" "a"}}`, "{{le .t .t}}", "{{gt 3 2}}", "{{gt 2 3}}", "{{gt .int .u}}",
		"{{ge 2 2}}", "{{ge 1 2}}", "{{ge .float .fint}}", "{{lt 1 2 3}}", "{{lt 1}}", `{{lt "B" "a"}}`, `{{lt "é" "z"}}`)
	// call.
	add("m", "{{call .str}}", "{{call .missing}}", "{{call}}", "{{.str | call}}", "{{call .person}}", "{{call .null 1}}")
	// Docker's functions.
	add("m", "{{json .}}", "{{json .str}}", "{{json .html}}", "{{json .list}}", "{{json .map}}", "{{json .null}}",
		"{{json .missing}}", "{{json .float}}", "{{json .big}}", "{{json .small}}", "{{json .fint}}", "{{json .person}}",
		"{{json .container}}", "{{json .labels}}", "{{json .multi}}", `{{json "  \x01\x7f"}}`, "{{json 1e-7}}",
		"{{json 123456789012345678.0}}", "{{json .u}}", "{{json}}", "{{json .strs}}", "{{json .mixed}}", "{{json .neg}}",
		"{{json .unicode}}", "{{json 0.000001}}", "{{json 1e20}}", "{{json -0.0}}", "{{json .people}}", "{{json .emptymap}}",
		"{{json .emptylist}}", "{{json .t}}", "{{json 1 2}}", "{{.list | json}}", `{{json "\b\f\n\r\t\\\""}}`,
		`{{split .csv ","}}`, `{{split .str ""}}`, `{{split "" ","}}`, "{{split .str}}", `{{split .int ","}}`,
		`{{index (split .csv ",") 1}}`, `{{len (split .csv ",")}}`, `{{range split .csv ","}}[{{.}}]{{end}}`,
		`{{split .unicode ""}}`, `{{split .csv ",,"}}`, `{{split .missing ","}}`, `{{split .str "l" | len}}`,
		`{{printf "%T" (split .csv ",")}}`, `{{"," | split .csv}}`,
		`{{join .list ", "}}`, `{{join .strs "-"}}`, `{{join .ints "+"}}`, `{{join .map ","}}`, `{{join .mixed "|"}}`,
		`{{join .missing ","}}`, `{{join .str ","}}`, `{{join .labels ","}}`, `{{join (split .csv ",") "/"}}`,
		`{{join .emptylist ","}}`, `{{join .list}}`, `{{join .list 1}}`, `{{join .null ","}}`, `{{join .int ","}}`,
		`{{join .people ";"}}`, `{{join .nested ","}}`, `{{join .person ","}}`,
		"{{title .str}}", `{{title "hello wORLD-foo_bar baz.qux 3d x'y"}}`, "{{title .unicode}}", `{{lower "HeLLo ÀÉ"}}`,
		"{{upper .unicode}}", `{{upper "ß"}}`, "{{lower .int}}", "{{upper}}", "{{title .missing}}", `{{lower "İ"}}`,
		`{{upper "ǆ"}}`, `{{title "ǆa"}}`, "{{.str | upper}}", `{{title "_a b"}}`, `{{title "élan vital"}}`,
		"{{pad .str 2 3}}|", "{{pad .empty 2 3}}|", "{{pad .str 0 0}}", "{{pad .str -1 0}}", "{{pad .str 1}}",
		`{{pad .str "a" 1}}`, "{{pad .int 1 1}}", "{{pad .str 1 .int}}|", "{{pad .str 1 .u}}",
		"{{truncate .str 3}}", "{{truncate .str 10}}", "{{truncate .str 5}}", "{{truncate .str 0}}", "{{truncate .str -1}}",
		"{{truncate .unicode 2}}", "{{truncate .str 2.0}}", "{{truncate .str 2.5}}", "{{truncate .str .int}}",
		"{{truncate .str .u}}", "{{truncate .missing 3}}", "{{truncate .str}}", `{{truncate "日本" 3}}`, "{{.str | truncate 2}}",
		`{{"abcdef" | truncate 2}}`)
	// Objects.
	add("m", "{{.person.Name}}", "{{.person.Age}}", "{{.person.Tags}}", "{{.person.Meta}}", "{{.person.Meta.team}}",
		"{{.person.Missing}}", `{{.person.Greet "Hi"}}`, "{{.person.Greet}}", "{{.person.Greet 1}}", "{{.person.Greet .int}}",
		"{{.person.Sum 2 3}}", `{{.person.Sum 2 "x"}}`, "{{.person.Fail}}", `{{.person.Label "team"}}`, "{{.person.Scale 1.5}}",
		"{{.person.Scale 2}}", "{{.person.Older 18}}", "{{.person.Older -1}}", "{{.person.Pick true}}", "{{.person.Pick 1}}",
		"{{.person.Kind .str}}", "{{.person.Kind nil}}", "{{.person.Kind .missing}}", "{{.person.Kind 1.5}}",
		"{{.person.Kind .list}}", "{{.person.Kind .null}}", `{{.person.Name "x"}}`, "{{.person.Initial}}", "{{.person.Name.x}}",
		`{{"Yo" | .person.Greet}}`, `{{.person.Greet "a" | printf "%q"}}`, "{{.person}}", "{{.person.Sum 1 2 3}}",
		"{{.person.Fail | printf \"%q\"}}", "{{.person.Greet .missing}}", "{{.person.Greet nil}}", "{{.person.Tags.x}}",
		"{{index .person.Tags 1}}", "{{len .person.Meta}}", "{{.person.Older .u}}", "{{.person.Scale .float}}",
		"{{.person.Sum .int .neg}}", "{{.person | .Name}}", "{{with .person}}{{.Sum 1 1}}{{end}}",
		"{{.container.ID}}", "{{.container.Names}}", `{{.container.Label "env"}}`, `{{.container.Label "none"}}`,
		"{{.container.Labels}}", "{{.container.Nope}}", "{{.container.ID 1}}", "{{.container.Label}}",
		`{{.container.ID | printf "%.4s"}}`, "{{.container.Label .str}}", "{{.container.ID.x}}")
	add("person", "{{.Name}}", "{{.Missing}}", `{{.Greet "Hey"}}`, "{{.}}", "{{.Tags}}", "{{json .}}", "{{.Name.x}}",
		"{{range .Tags}}<{{.}}>{{end}}", "{{.Meta.team}}", "{{.Meta.none}}", `{{index .Meta "team"}}`, "{{.Age | printf \"%03d\"}}")
	add("container", "{{.ID}}\t{{.Names}}", `table {{.ID}}\t{{.Label "com.docker.compose.project"}}`, "{{.Label}}",
		`{{.ID | printf "%.4s"}}`, `{{.Labels | split ","}}`, `{{range split .Labels ","}}[{{.}}]{{end}}`, "{{.}}",
		"{{json .}}", "{{.Missing}}", `{{truncate .ID 6}}`, `{{upper .Names}}`, `{{pad .ID 1 1}}|`, "{{.ID.x}}",
		`{{if eq (.Label "env") "prod"}}P{{end}}`, `{{.Label "env" | len}}`)
	// Other data.
	add("nil", "{{.}}", "{{.x}}", "{{.x.y}}", "{{len .}}", "{{json .}}", "{{range .}}x{{else}}e{{end}}",
		"{{if .}}t{{else}}f{{end}}", "{{$}}", "{{index . 1}}", `{{printf "%v" .}}`, "{{with .}}x{{else}}none{{end}}",
		"{{print .}}", "{{eq . nil}}", "{{and . 1}}", "{{slice .}}", "{{call .}}", "{{.x 1}}", "{{$.x}}", "{{join . \",\"}}")
	add("list", "{{.}}", "{{index . 0}}", "{{.x}}", "{{range .}}{{.}}{{end}}", "{{len .}}", "{{json .}}", "{{slice . 1}}",
		"{{range $i, $e := .}}{{$i}}{{$e}}{{end}}", "{{.x.y}}")
	add("int", "{{.}}", "{{range .}}{{.}}{{end}}", "{{.x}}", "{{if eq . 3}}three{{end}}", "{{json .}}", "{{printf \"%T\" .}}",
		"{{range $i, $v := .}}{{end}}", "{{lt . 4}}")
	add("str", "{{.}}", "{{len .}}", "{{.Foo}}", "{{upper .}}", "{{split . \" \"}}", "{{index . 0}}", "{{slice . 5}}",
		"{{range .}}{{end}}", "{{json .}}")
	// Error positions over lines.
	add("m", "line1\nline2 {{.null.x}}\n{{.str.x}}", "{{if .t}}\n  {{.str.y}}\n{{end}}", "\n\n{{printf}}",
		"{{.str}}{{.int.x}}", "x\n{{template \"nope\"}}", "{{.str\n.x}}", "{{if .t}}\n{{.str", "a\n{{.str\nb}}",
		"{{/* unclosed", "{{/* c */ x}}", "a\n{{/* c\n */ .str}}", "{{\n.str\n}}", "{{.str\n| printf \"%d\"}}",
		"{{(.str\n}}", "x\n\ny{{\"abc\n\"}}", "{{\"unterminated}}", "{{'a}}", "{{`raw}}", "{{(.str}}", "{{.str)}}",
		"{{nofunc}}", "{{.str|}}", "{{|.str}}", "{{.a-b}}", "{{@}}", "{{é}}", "{{€}}", "{{", "{{.str", "{{.str}",
		"{{.str}}}}", "}}", "{{{{.str}}", "{{.str .}}", "{{$x:=}}", "{{range .list}}\n{{.y}}{{end}}", "{{.str|printf \"%d\"|len}}",
		"{{ .str | printf \"%s\" | printf \"%q\" }}", "{{define \"T\"}}\n{{.x.y}}\n{{end}}{{template \"T\" .str}}",
		"{{with .list}}\n\t{{index . 9}}{{end}}", "{{.\x01}}", "{{.str\t}}", "{{.str -}}{{- .int}}", "{{- -}}", "{{ - }}",
		"{{-}}", "{{.str }}", "{{print -}}")
	// More: scoping in else branches, pipes into methods, constants as parameters.
	add("m", "{{with $x := .missing}}{{else}}[{{$x}}]{{end}}", "{{range $i, $e := .emptylist}}{{else}}[{{$i}}]{{end}}",
		"{{if $x := 0}}{{else}}{{$x}}{{end}}", "{{$x := 0}}{{range .ints}}{{$x = .}}{{end}}{{$x}}",
		"{{with $x := 1}}{{$x = 2}}{{$x}}{{end}}", `{{define "T"}}{{.}}{{end}}{{$x := 5}}{{template "T" $x}}`,
		"{{1 | .person.Sum 2}}", `{{"x" | .person.Sum 2}}`, "{{.person.Sum 1 2 | .person.Sum 3}}", "{{or 1 | not}}",
		"{{.person.Sum (len .list) 1}}", `{{(.person.Greet "x") | len}}`, "{{range $i, $e := .people}}{{$e.Sum $i 1}}{{end}}",
		"{{$x := .person}}{{$x.Greet \"a\"}}", "{{.person.Kind .}}", "{{.person.Kind 'a'}}", "{{.person.Kind 1}}",
		"{{.person.Kind true}}", `{{.person.Kind "s"}}`, "{{.person.Kind .u}}", "{{.person.Kind .person}}",
		"{{.person.Kind .container}}", "{{.person.Kind .labels}}", "{{.person.Kind .strs}}", "{{.person.Older 'a'}}",
		"{{.person.Scale 'a'}}", "{{.person.Older 1.0}}", "{{.person.Older 0x10}}", "{{.person.Sum 1e2 1}}",
		"{{.person.Sum 9223372036854775808 1}}", "{{.person.Scale true}}", "{{.person.Pick .t}}", "{{.person.Pick .null}}",
		"{{.person.Greet .null}}", "{{.person.Kind (index .mixed 4)}}", "{{.person.Kind .nested.null}}",
		`{{printf "%d" 1e3}}`, `{{printf "%s" 0x10}}`, `{{len "日本"}}`, `{{slice "日本" 1}}`, `{{index .labels "com.example.a" | printf "%q"}}`,
		"{{0.1}}", "{{1e-7}}", "{{100000000.0}}", "{{123456.0}}", "{{1e6}}", "{{-2.5}}", "{{0x1.8p1}}", "{{1e21}}", "{{1e20}}",
		`{{printf "%v" 100000000000000000000.0}}`, `{{"{{"}}`, `{{"}}"}}`, `{{print "}}" "{{"}}`, "{{()}}",
		"{{1\"long string literal\"}}", "{{if .t}}{{end}}{{end}}", `{{block "x" .}}`, "a\n  {{- /* x */ -}}\n  b",
		"{{.str}}{{/* a */}}{{/* b */}}{{.int}}", "{{ /* not a comment */ }}", "{{/**/}}", "{{/*\n*/}}",
		"{{.héllo}}", "{{.日本}}", "{{$é := 1}}{{$é}}", "{{.x_y}}", "{{._}}", "{{$_ := 2}}{{$_}}",
		`{{index .map "a" | printf "%T"}}`, "{{.map.a | printf \"%T\"}}", "{{range .map}}{{printf \"%T\" .}}{{end}}",
		"{{range $k, $v := .map}}{{printf \"%T\" $k}}{{end}}", "{{range $i, $v := .list}}{{printf \"%T\" $i}}{{end}}",
		"{{range 2}}{{printf \"%T\" .}}{{end}}", "{{range .u}}{{printf \"%T\" .}}{{end}}", "{{len .list | printf \"%T\"}}",
		"{{eq .int 42 | printf \"%T\"}}", `{{printf "%v|%s|%d" .labels .labels .labels}}`, `{{printf "%x" .list}}`,
		`{{printf "%q" .map}}`, `{{printf "%5.1f" .mixed}}`, `{{printf "%v" .nested.list}}`, `{{printf "%+v" .person}}`,
		`{{printf "%s" .person}}`, `{{printf "%v" .people}}`, `{{printf "%v" .container}}`, `{{print .container}}`,
		`{{.container}}`, `{{html .container}}`, `{{range .container}}{{end}}`, `{{index .container 1}}`, `{{slice .container}}`,
		`{{join .container ","}}`, `{{printf "%T" .container.ID}}`, `{{.person.Initial | printf "%q"}}`)
	named("mytmpl", "m", "{{.str.x}}", `{{define "inner"}}{{.y.z}}{{end}}{{template "inner" .str}}`, "{{.str",
		`{{define "mytmpl"}}X{{end}}`, `{{define "mytmpl"}}X{{end}}main`, `{{define "mytmpl"}}X{{end}}  `)
	named("a%b", "m", "{{.str.x}}", "{{.str")
	// Table headers, as docker/cli's formatter runs them.
	header("header", "{{.ID}}\t{{.Names}}", "{{json .ID}}", "{{upper .Names}}", "{{truncate .ID 3}}", `{{split .ID ","}}`,
		`{{join .ID ","}}`, "{{pad .ID 1 1}}|", "{{title .Image}}", `{{.Label "x"}}`, "{{.Missing}}", "{{json 1}}",
		"{{lower .Image}}", `{{printf "%-20s|" .ID}}`, "{{json .Missing}}", "{{truncate .ID}}", "{{len .ID}}",
		`{{split .ID}}`, "{{json .}}")
}

func TestShardsTemplate(t *testing.T) {
	corpus()
	values := map[string]any{}
	for name, raw := range data {
		d := json.NewDecoder(strings.NewReader(raw))
		d.UseNumber()
		var v any
		if err := d.Decode(&v); err != nil {
			t.Fatalf("data %s: %v", name, err)
		}
		values[name] = convert(v)
	}
	for i := range cases {
		c := &cases[i]
		var tmpl *template.Template
		var err error
		if c.Name == "" {
			tmpl, err = templates.Parse(c.Template)
		} else {
			tmpl, err = templates.New(c.Name).Parse(c.Template)
		}
		if err != nil {
			c.Error = err.Error()
			continue
		}
		if c.Header {
			tmpl = tmpl.Funcs(templates.HeaderFunctions)
		}
		var buf bytes.Buffer
		if err := tmpl.Execute(&buf, values[c.Data]); err != nil {
			c.Error = err.Error()
		}
		c.Output = buf.String()
	}
	raw := map[string]json.RawMessage{}
	for name, d := range data {
		raw[name] = json.RawMessage(d)
	}
	out := struct {
		Go        string                     `json:"go"`
		DockerCLI string                     `json:"docker_cli"`
		Data      map[string]json.RawMessage `json:"data"`
		Cases     []oracleCase               `json:"cases"`
	}{runtime.Version(), "v29.8.1", raw, cases}
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", " ")
	if err := enc.Encode(out); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_TEMPLATE_OUT"), buf.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}
