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

	"github.com/moby/buildkit/frontend/dockerfile/instructions"
	"github.com/moby/buildkit/frontend/dockerfile/linter"
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

// What BuildKit's instructions make of a parsed file: its stages and the ARGs before them,
// with the lint warnings they give, or its error. The linter is set up as
// dockerfile2llb sets it up, from the file's check directive.
func instructionsFile(root, rel string) map[string]any {
	data, err := os.ReadFile(filepath.Join(root, rel))
	if err != nil {
		panic(err)
	}
	out := map[string]any{"file": rel}
	res, err := parser.Parse(bytes.NewReader(data))
	if err != nil {
		out["parse_error"] = true
		return out
	}
	var warnings []string
	checkStr, _, _, _ := parser.ParseDirective("check", data)
	cfg, err := linter.ParseLintOptions(checkStr)
	if err != nil {
		out["error"] = q("failed to parse check options: " + err.Error())
		return out
	}
	cfg.Warn = func(rule, desc, url, msg string, loc []parser.Range) {
		var lines []string
		for _, r := range loc {
			lines = append(lines, fmt.Sprintf("%d-%d", r.Start.Line, r.End.Line))
		}
		warnings = append(warnings, q(fmt.Sprintf("%s|%s|%s|%s|%s", rule, desc, url, msg, strings.Join(lines, ","))))
	}
	lint := linter.New(cfg)
	stages, metaArgs, err := instructions.Parse(res.AST, lint)
	out["warnings"] = warnings
	if err != nil {
		out["error"] = q(err.Error())
		var le *parser.LocationError
		if errors.As(err, &le) {
			var locs [][][2]int
			for _, rs := range le.Locations {
				var lines [][2]int
				for _, r := range rs {
					lines = append(lines, [2]int{r.Start.Line, r.End.Line})
				}
				locs = append(locs, lines)
			}
			out["location"] = locs
		}
		return out
	}
	if lerr := lint.Error(); lerr != nil {
		out["lint_error"] = true
	}
	var meta []any
	for _, a := range metaArgs {
		meta = append(meta, command(&a))
	}
	out["meta_args"] = meta
	var st []any
	for _, s := range stages {
		var cmds []any
		for _, c := range s.Commands {
			cmds = append(cmds, command(c))
		}
		st = append(st, map[string]any{
			"name": q(s.Name), "orig_cmd": q(s.OrigCmd), "base_name": q(s.BaseName), "platform": q(s.Platform),
			"doc_comment": q(s.DocComment), "source_code": q(s.SourceCode), "location": ranges(s.Location),
			"comments": qs(s.Comments), "commands": cmds,
		})
	}
	out["stages"] = st
	return out
}

func ranges(rs []parser.Range) [][2]int {
	out := [][2]int{}
	for _, r := range rs {
		out = append(out, [2]int{r.Start.Line, r.End.Line})
	}
	return out
}

func kvps(kv instructions.KeyValuePairs) []any {
	out := []any{}
	for _, p := range kv {
		out = append(out, []any{q(p.Key), q(p.Value), p.NoDelim})
	}
	return out
}

func optq(s *string) any {
	if s == nil {
		return nil
	}
	return q(*s)
}

func optb(b *bool) any {
	if b == nil {
		return nil
	}
	return *b
}

func optu(u *uint64) any {
	if u == nil {
		return nil
	}
	return *u
}

func shellCmd(c instructions.ShellDependantCmdLine) map[string]any {
	var files []any
	for _, f := range c.Files {
		files = append(files, []any{q(f.Name), q(f.Data), f.Chomp})
	}
	cl := []string{}
	if c.CmdLine != nil {
		cl = qs(c.CmdLine)
	}
	return map[string]any{"cmd_line": cl, "cmd_line_nil": c.CmdLine == nil, "files": files, "prepend_shell": c.PrependShell}
}

func sources(s instructions.SourcesAndDest) map[string]any {
	var contents []any
	for _, c := range s.SourceContents {
		contents = append(contents, []any{q(c.Path), q(c.Data), c.Expand})
	}
	return map[string]any{"dest": q(s.DestPath), "paths": qs(s.SourcePaths), "contents": contents}
}

// One command, its kind and fields.
func command(c any) map[string]any {
	type common interface {
		Name() string
		Location() []parser.Range
		Comments() []string
		String() string
	}
	out := map[string]any{"kind": fmt.Sprintf("%T", c)}
	if cc, ok := c.(common); ok {
		out["name"] = q(cc.Name())
		out["code"] = q(cc.String())
		out["location"] = ranges(cc.Location())
		out["comments"] = qs(cc.Comments())
	}
	switch c := c.(type) {
	case *instructions.EnvCommand:
		out["env"] = kvps(c.Env)
	case *instructions.MaintainerCommand:
		out["maintainer"] = q(c.Maintainer)
	case *instructions.LabelCommand:
		out["labels"] = kvps(c.Labels)
	case *instructions.AddCommand:
		out["sources"] = sources(c.SourcesAndDest)
		out["chown"], out["chmod"], out["link"] = q(c.Chown), q(c.Chmod), c.Link
		out["exclude"], out["keep_git_dir"], out["checksum"], out["unpack"] = qs(c.ExcludePatterns), optb(c.KeepGitDir), q(c.Checksum), optb(c.Unpack)
	case *instructions.CopyCommand:
		out["sources"] = sources(c.SourcesAndDest)
		out["from"], out["chown"], out["chmod"], out["link"] = q(c.From), q(c.Chown), q(c.Chmod), c.Link
		out["exclude"], out["parents"] = qs(c.ExcludePatterns), c.Parents
	case *instructions.OnbuildCommand:
		out["expression"] = q(c.Expression)
	case *instructions.WorkdirCommand:
		out["path"] = q(c.Path)
	case *instructions.RunCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
		used := append([]string(nil), c.FlagsUsed...)
		sort.Strings(used)
		out["flags_used"] = qs(used)
		var mounts []any
		for _, m := range instructions.GetMounts(c) {
			mounts = append(mounts, map[string]any{
				"type": q(string(m.Type)), "from": q(m.From), "source": q(m.Source), "target": q(m.Target),
				"read_only": m.ReadOnly, "size": m.SizeLimit, "id": q(m.CacheID), "sharing": q(string(m.CacheSharing)),
				"required": m.Required, "env": optq(m.Env), "mode": optu(m.Mode), "uid": optu(m.UID), "gid": optu(m.GID),
			})
		}
		out["mounts"] = mounts
		out["network"] = q(instructions.GetNetwork(c))
		out["security"] = q(instructions.GetSecurity(c))
		var devices []any
		for _, d := range instructions.GetDevices(c) {
			devices = append(devices, []any{q(d.Name), d.Required})
		}
		out["devices"] = devices
	case *instructions.CmdCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
	case *instructions.EntrypointCommand:
		out["shell"] = shellCmd(c.ShellDependantCmdLine)
	case *instructions.HealthCheckCommand:
		h := c.Health
		out["health"] = map[string]any{"test": qs(h.Test), "interval": int64(h.Interval), "timeout": int64(h.Timeout),
			"start_period": int64(h.StartPeriod), "start_interval": int64(h.StartInterval), "retries": h.Retries}
	case *instructions.ExposeCommand:
		out["ports"] = qs(c.Ports)
	case *instructions.UserCommand:
		out["user"] = q(c.User)
	case *instructions.VolumeCommand:
		out["volumes"] = qs(c.Volumes)
	case *instructions.StopSignalCommand:
		out["signal"] = q(c.Signal)
	case *instructions.ArgCommand:
		var args []any
		for _, a := range c.Args {
			args = append(args, []any{q(a.Key), optq(a.Value), q(a.DocComment)})
		}
		out["args"] = args
	case *instructions.ShellCommand:
		out["shell_words"] = qs(c.Shell)
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
	var insts []map[string]any
	for _, f := range files {
		insts = append(insts, instructionsFile(testdata, f))
	}
	writeJSON(filepath.Join(testdata, "instructions.json"), insts)

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
	var upper strings.Builder
	upper.WriteString("\n/// Go's `unicode.ToUpper`, for the runes it changes: (rune, upper case).\npub(crate) static UPPER: &[(u32, u32)] = &[\n")
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if u := unicode.ToUpper(r); u != r {
			fmt.Fprintf(&upper, "    (0x%X, 0x%X),\n", r, u)
		}
	}
	upper.WriteString("];\n")
	tables += upper.String()
	if err := os.WriteFile(filepath.Join(testdata, "../src/tables.rs"), []byte(tables), 0o644); err != nil {
		panic(err)
	}
}
