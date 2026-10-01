// What BuildKit's Dockerfile parser and shell lexer make of a corpus, for
// crates/dockerfile's tests to expect byte for byte. `generate`, beside this directory,
// runs it inside a moby/buildkit checkout at the pinned tag, whose packages it imports.
// Every string it writes is Go-quoted (strconv.Quote), so that any bytes, valid UTF-8 or
// not, compare exactly.
//
//	go run ./shards-oracle TESTDATA
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"unicode"

	"github.com/moby/buildkit/frontend/dockerfile/parser"
	"github.com/moby/buildkit/frontend/dockerfile/shell"
)

func q(s string) string { return strconv.Quote(s) }

func qs(ss []string) []string {
	out := make([]string, 0, len(ss))
	for _, s := range ss {
		out = append(out, q(s))
	}
	return out
}

type heredoc struct {
	Name    string `json:"name"`
	Fd      uint   `json:"fd"`
	Expand  bool   `json:"expand"`
	Chomp   bool   `json:"chomp"`
	Content string `json:"content"`
}

type child struct {
	Dump        string    `json:"dump"`
	Original    string    `json:"original"`
	Flags       []string  `json:"flags"`
	Attributes  []string  `json:"attributes"`
	StartLine   int       `json:"start_line"`
	EndLine     int       `json:"end_line"`
	PrevComment []string  `json:"prev_comment"`
	Heredocs    []heredoc `json:"heredocs"`
}

type parsed struct {
	File     string     `json:"file"`
	Escape   string     `json:"escape,omitempty"`
	Dump     string     `json:"dump,omitempty"`
	Children []child    `json:"children,omitempty"`
	Warnings []string   `json:"warnings,omitempty"`
	Error    string     `json:"error,omitempty"`
	Location [][][2]int `json:"location,omitempty"`
}

func parseFile(root, rel string) parsed {
	data, err := os.ReadFile(filepath.Join(root, rel))
	if err != nil {
		panic(err)
	}
	out := parsed{File: rel}
	res, err := parser.Parse(bytes.NewReader(data))
	if err != nil {
		out.Error = q(err.Error())
		var le *parser.LocationError
		if errors.As(err, &le) {
			for _, rs := range le.Locations {
				var lines [][2]int
				for _, r := range rs {
					lines = append(lines, [2]int{r.Start.Line, r.End.Line})
				}
				out.Location = append(out.Location, lines)
			}
		}
		return out
	}
	out.Escape = q(string(res.EscapeToken))
	out.Dump = q(res.AST.Dump())
	for _, w := range res.Warnings {
		out.Warnings = append(out.Warnings, q(fmt.Sprintf("%s|%s|%d", w.Short, w.URL, w.Location.Start.Line)))
	}
	for _, n := range res.AST.Children {
		c := child{
			Dump:        q(n.Dump()),
			Original:    q(n.Original),
			Flags:       qs(n.Flags),
			StartLine:   n.StartLine,
			EndLine:     n.EndLine,
			PrevComment: qs(n.PrevComment),
		}
		for k, v := range n.Attributes {
			if v {
				c.Attributes = append(c.Attributes, k)
			}
		}
		sort.Strings(c.Attributes)
		for _, h := range n.Heredocs {
			c.Heredocs = append(c.Heredocs, heredoc{q(h.Name), h.FileDescriptor, h.Expand, h.Chomp, q(h.Content)})
		}
		out.Children = append(out.Children, c)
	}
	return out
}

// A lexer case: the lexer's settings, the environment, and the input.
type lexCase struct {
	Escape     string   `json:"escape"`
	RawQuotes  bool     `json:"raw_quotes"`
	RawEscapes bool     `json:"raw_escapes"`
	SkipUnset  bool     `json:"skip_unset"`
	SkipQuotes bool     `json:"skip_quotes"`
	Env        []string `json:"env"`
	Input      string   `json:"input"`
}

type lexed struct {
	lexCase
	Word      string   `json:"word,omitempty"`
	Matched   []string `json:"matched,omitempty"`
	Unmatched []string `json:"unmatched,omitempty"`
	WordError string   `json:"word_error,omitempty"`
	Words     []string `json:"words,omitempty"`
	WordsErr  string   `json:"words_error,omitempty"`
}

func lex(c lexCase) lexed {
	l := shell.NewLex([]rune(c.Escape)[0])
	l.RawQuotes, l.RawEscapes, l.SkipUnsetEnv, l.SkipProcessQuotes = c.RawQuotes, c.RawEscapes, c.SkipUnset, c.SkipQuotes
	env := shell.EnvsFromSlice(c.Env)
	out := lexed{lexCase: c}
	if r, err := l.ProcessWordWithMatches(c.Input, env); err != nil {
		out.WordError = q(err.Error())
	} else {
		out.Word = q(r.Result)
		for k := range r.Matched {
			out.Matched = append(out.Matched, q(k))
		}
		for k := range r.Unmatched {
			out.Unmatched = append(out.Unmatched, q(k))
		}
		sort.Strings(out.Matched)
		sort.Strings(out.Unmatched)
	}
	if ws, err := l.ProcessWords(c.Input, env); err != nil {
		out.WordsErr = q(err.Error())
	} else {
		out.Words = qs(ws)
		if out.Words == nil {
			out.Words = []string{}
		}
	}
	return out
}

// BuildKit's own lexer tables, as lex_test.go runs them on Unix.
func buildkitCases(dir string) []lexCase {
	var cases []lexCase
	read := func(name string) []string {
		f, err := os.Open(filepath.Join(dir, name))
		if err != nil {
			panic(err)
		}
		defer f.Close()
		var lines []string
		s := bufio.NewScanner(f)
		for s.Scan() {
			lines = append(lines, s.Text())
		}
		return lines
	}
	env := []string{"PWD=/home", "SHELL=bash", "KOREAN=한국어", "NULL="}
	for _, line := range read("envVarTest") {
		if strings.HasPrefix(line, "#") || strings.TrimSpace(line) == "" {
			continue
		}
		f := strings.Split(strings.TrimSpace(line), "|")
		if p := strings.TrimSpace(f[0]); p != "A" && p != "U" {
			continue
		}
		cases = append(cases, lexCase{Escape: `\`, Env: env, Input: strings.TrimSpace(f[1])})
	}
	for _, only := range []bool{false, true} {
		var envs []string
		for _, line := range read("wordsTest") {
			if strings.HasPrefix(line, "#") {
				continue
			}
			if strings.HasPrefix(line, "ENV ") {
				envs = append(envs, strings.TrimLeft(line[3:], " "))
				continue
			}
			f := strings.Split(line, "|")
			cases = append(cases, lexCase{Escape: `\`, RawQuotes: only, SkipUnset: only,
				Env: append([]string(nil), envs...), Input: strings.TrimSpace(f[0])})
		}
	}
	return cases
}

// The runes for which `in` is true, as sorted inclusive ranges.
func table(name, doc string, in func(r rune) bool) string {
	var b strings.Builder
	fmt.Fprintf(&b, "\n/// %s\npub(crate) static %s: &[(u32, u32)] = &[\n", doc, name)
	start := rune(-1)
	for r := rune(0); r <= unicode.MaxRune+1; r++ {
		inside := r <= unicode.MaxRune && in(r)
		if inside && start < 0 {
			start = r
		}
		if !inside && start >= 0 {
			fmt.Fprintf(&b, "    (0x%X, 0x%X),\n", start, r-1)
			start = -1
		}
	}
	b.WriteString("];\n")
	return b.String()
}

func writeJSON(path string, v any) {
	b, err := json.MarshalIndent(v, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(path, append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}

func main() {
	testdata := os.Args[1]
	var files []string
	for _, dir := range []string{"buildkit/parser", "buildkit/parser-negative", "corpus"} {
		filepath.WalkDir(filepath.Join(testdata, dir), func(p string, d os.DirEntry, err error) error {
			if err == nil && !d.IsDir() && (d.Name() == "Dockerfile" || strings.HasSuffix(d.Name(), ".Dockerfile")) {
				rel, _ := filepath.Rel(testdata, p)
				files = append(files, filepath.ToSlash(rel))
			}
			return nil
		})
	}
	sort.Strings(files)
	var parses []parsed
	for _, f := range files {
		parses = append(parses, parseFile(testdata, f))
	}
	writeJSON(filepath.Join(testdata, "parse.json"), parses)

	cases := buildkitCases(filepath.Join(testdata, "buildkit/shell"))
	extra, err := os.ReadFile(filepath.Join(testdata, "lex-cases.json"))
	if err != nil {
		panic(err)
	}
	var mine []lexCase
	if err := json.Unmarshal(extra, &mine); err != nil {
		panic(err)
	}
	cases = append(cases, mine...)
	var lexes []lexed
	for _, c := range cases {
		lexes = append(lexes, lex(c))
	}
	writeJSON(filepath.Join(testdata, "lex.json"), lexes)

	tables := fmt.Sprintf("//! Generated by scripts/dockerfile/generate with Go %s (Unicode %s), the\n"+
		"//! character classes BuildKit's parser and lexer test with. Do not edit.\n",
		strings.TrimPrefix(runtime.Version(), "go"), unicode.Version)
	tables += table("LETTER", "Go's `unicode.IsLetter`.", unicode.IsLetter)
	tables += table("DIGIT", "Go's `unicode.IsDigit`.", unicode.IsDigit)
	tables += table("SPACE", "Go's `unicode.IsSpace`.", unicode.IsSpace)
	tables += table("NOT_PRINT", "Runes from U+0080 up that Go's `strconv.IsPrint` rejects, which `strconv.Quote` escapes.",
		func(r rune) bool { return r >= 0x80 && !strconv.IsPrint(r) })
	var lower strings.Builder
	lower.WriteString("\n/// Go's `unicode.ToLower`, for the runes it changes: (rune, lower case).\npub(crate) static LOWER: &[(u32, u32)] = &[\n")
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if l := unicode.ToLower(r); l != r {
			fmt.Fprintf(&lower, "    (0x%X, 0x%X),\n", r, l)
		}
	}
	lower.WriteString("];\n")
	tables += lower.String()
	if err := os.WriteFile(filepath.Join(testdata, "../src/tables.rs"), []byte(tables), 0o644); err != nil {
		panic(err)
	}
}
