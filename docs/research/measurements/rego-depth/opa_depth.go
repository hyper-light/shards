// What OPA v1.14.1 (as buildx v0.37.1 vendors it) takes to parse, compile and evaluate a
// policy whose one rule nests an array N deep, each phase timed: the bound shards'
// policy engine is held to (M126).
package main

import (
	"context"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/open-policy-agent/opa/v1/ast"
	"github.com/open-policy-agent/opa/v1/rego"
)

func main() {
	n, err := strconv.Atoi(os.Args[1])
	if err != nil {
		panic(err)
	}
	src := fmt.Sprintf("package docker\n\nx := %s1%s\n\ndecision := {\"allow\": x == x}\n", strings.Repeat("[", n), strings.Repeat("]", n))
	t := time.Now()
	m, err := ast.ParseModuleWithOpts("p.rego", src, ast.ParserOptions{RegoVersion: ast.RegoV1})
	fmt.Println("parse", time.Since(t), err)
	if err != nil {
		return
	}
	t = time.Now()
	c := ast.NewCompiler()
	c.Compile(map[string]*ast.Module{"p.rego": m})
	fmt.Println("compile", time.Since(t), c.Failed())
	t = time.Now()
	rs, err := rego.New(rego.Query("data.docker.decision"), rego.Compiler(c)).Eval(context.Background())
	fmt.Println("eval", time.Since(t), err, len(rs))
}
