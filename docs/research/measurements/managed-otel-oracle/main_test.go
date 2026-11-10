package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"go.opentelemetry.io/collector/pdata/pmetric"
)

func sample(number string) string {
	return `{"resourceMetrics":[{"resource":{"attributes":[
	{"key":"generation","value":{"stringValue":"18446744073709551615"}},
	{"key":"integer","value":{"intValue":"9007199254740993"}}]},
	"scopeMetrics":[{"scope":{"name":"oracle.fixture","version":"1"},
	"metrics":[{"name":"shards.cgroup.memory.usage","unit":"By",
	"sum":{"aggregationTemporality":2,"isMonotonic":false,"dataPoints":[{
	"timeUnixNano":"18446744073709551615",` + number + `,
	"attributes":[{"key":"direction","value":{"stringValue":"read"}}]}]}}]}]}]}`
}

func TestExactNumbersAndScope(t *testing.T) {
	r, err := decode([]byte(sample(`"asInt":"9223372036854775807"`)))
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Resources) != 1 || len(r.Resources[0].Scopes) != 1 || len(r.Resources[0].Scopes[0].Metrics) != 1 {
		t.Fatalf("unexpected receipt hierarchy")
	}
	resource := r.Resources[0]
	if resource.Attributes[0].Value != "18446744073709551615" || resource.Attributes[1].Value != "9007199254740993" {
		t.Fatalf("attribute integers changed: %+v", resource.Attributes)
	}
	scope := resource.Scopes[0]
	m := scope.Metrics[0]
	if scope.Name != "oracle.fixture" || scope.Version != "1" || m.Name != "shards.cgroup.memory.usage" || m.Unit != "By" || m.Type != "Sum" || m.Temporality != "Cumulative" || m.Monotonic == nil || *m.Monotonic {
		t.Fatal("scope or sum metadata changed")
	}
	p := m.Points[0]
	if p.Time != "18446744073709551615" || p.Start != "0" || p.Kind != "Int" || p.Value != "9223372036854775807" || p.Attributes[0].Value != "read" {
		t.Fatalf("point changed: %+v", p)
	}
}

func TestGaugeDoubleAndSignedBoundary(t *testing.T) {
	for _, field := range []string{`"asInt":"-9223372036854775808"`, `"asDouble":0.000001`} {
		payload := strings.Replace(sample(field), `"sum":{"aggregationTemporality":2,"isMonotonic":false,`, `"gauge":{`, 1)
		r, err := decode([]byte(payload))
		if err != nil {
			t.Fatal(err)
		}
		m := r.Resources[0].Scopes[0].Metrics[0]
		if m.Type != "Gauge" || m.Temporality != "" || m.Monotonic != nil {
			t.Fatal("gauge acquired sum metadata")
		}
	}
}

func TestOfficialParserVersusAddedSemanticChecks(t *testing.T) {
	cases := []struct {
		name    string
		payload string
		want    error
	}{
		{"absent", sample(`"flags":1`), oneofError},
		{"both-number-fields", sample(`"asInt":"7","asDouble":8`), oneofError},
		{"duplicate-point-attribute", strings.Replace(sample(`"asInt":"1"`), `"attributes":[{"key":"direction","value":{"stringValue":"read"}}]`, `"attributes":[{"key":"direction","value":{"stringValue":"read"}},{"key":"direction","value":{"stringValue":"write"}}]`, 1), duplicateError},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := (&pmetric.JSONUnmarshaler{}).UnmarshalMetrics([]byte(tc.payload)); err != nil {
				t.Fatalf("expected official parse acceptance before added check: %v", err)
			}
			if _, err := decode([]byte(tc.payload)); err != tc.want {
				t.Fatalf("got %v, want %v", err, tc.want)
			}
		})
	}
}

func TestRejectMalformedAndUnsupported(t *testing.T) {
	cases := []struct {
		payload string
		want    error
	}{
		{`{`, jsonError},
		{`null`, jsonError},
		{`{} {}`, jsonError},
		{strings.Replace(sample(`"asInt":"1"`), `"timeUnixNano":"18446744073709551615"`, `"timeUnixNano":"0"`, 1), timeError},
		{strings.Replace(sample(`"asInt":"1"`), `"aggregationTemporality":2`, `"aggregationTemporality":0`, 1), temporalityError},
		{strings.Replace(sample(`"asInt":"1"`), `"sum":{"aggregationTemporality":2,"isMonotonic":false,`, `"histogram":{`, 1), typeError},
		{strings.Replace(sample(`"asInt":"1"`), `"name":"shards.cgroup.memory.usage",`, `"gauge":{},"name":"shards.cgroup.memory.usage",`, 1), oneofError},
		{sample(`"asInt":"9223372036854775808"`), decodeError},
	}
	for _, tc := range cases {
		if _, err := decode([]byte(tc.payload)); err != tc.want {
			t.Errorf("got %v, want %v", err, tc.want)
		}
	}
}

func TestCallerBoundAndInputModes(t *testing.T) {
	for _, n := range []string{"", "0", "-1", "9223372036854775807", "9223372036854775808"} {
		args := []string{}
		if n != "" {
			args = []string{"-max-bytes", n}
		}
		if err := run(args, strings.NewReader(`{}`), &bytes.Buffer{}); err != inputBound {
			t.Fatalf("bound %q: %v", n, err)
		}
	}
	if err := run([]string{"-max-bytes", "1"}, strings.NewReader(`{}`), &bytes.Buffer{}); err != inputBound {
		t.Fatal("oversized input accepted")
	}
	payload := sample(`"asInt":"1"`)
	args := []string{"-max-bytes", strconv.Itoa(len(payload))}
	var stdin bytes.Buffer
	if err := run(args, strings.NewReader(payload), &stdin); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "fixture.json")
	if err := os.WriteFile(path, []byte(payload), 0600); err != nil {
		t.Fatal(err)
	}
	var file bytes.Buffer
	if err := run(append(args, "-input", path), strings.NewReader("ignored"), &file); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(stdin.Bytes(), file.Bytes()) {
		t.Fatal("stdin and file receipts differ")
	}
	var help bytes.Buffer
	if err := run([]string{"-h"}, nil, &help); err != nil || !strings.Contains(help.String(), "-max-bytes") {
		t.Fatal("-h did not show help")
	}
}
