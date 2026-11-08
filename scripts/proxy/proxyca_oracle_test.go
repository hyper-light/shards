//go:build linux

// Copied by scripts/proxy/generate into BuildKit v0.33.0's executor package: what its
// InjectProxyCA (executor/proxyca_linux.go) makes of each case's trust bundle and proxy
// CA, and what its cleanup makes of the bundle after a step changed it, for
// crates/init/src/proxyca.rs's tests to expect of shards' port.
package executor

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"unicode/utf8"
)

const oracleCA = `-----BEGIN CERTIFICATE-----
MIIBMzCB2gIJAL02WhWyYjSCMAoGCCqGSM49BAMCMCExHzAdBgNVBAMMFnNoYXJk
cyBwcm94eSBvcmFjbGUgQ0EwIBcNMjYxMDEwMDkwODExWhgPMjEyNjA5MTYwOTA4
MTFaMCExHzAdBgNVBAMMFnNoYXJkcyBwcm94eSBvcmFjbGUgQ0EwWTATBgcqhkjO
PQIBBggqhkjOPQMBBwNCAASeh7r7AhxfsnQ3oHuNFekHByHUwqfX4gF57T/EO/jA
B8Y/WQYZWWchyUIhiYilA76JjiG33D54FgfFJ+moApg1MAoGCCqGSM49BAMCA0gA
MEUCIQDtqAssQiFhvoXJF/LOFs6/+SHdbuiXE7eCg1BZd5zOowIgHFXlpG5uMNn5
iAqbyv5pYCO3TMGX/IwlYsG9x/6yNr4=
-----END CERTIFICATE-----
`

const oracleOther = `-----BEGIN CERTIFICATE-----
MIIBOTCB4AIJAP1BncHQw3XIMAoGCCqGSM49BAMCMCQxIjAgBgNVBAMMGXNoYXJk
cyBwcm94eSBvcmFjbGUgb3RoZXIwIBcNMjYxMDEwMDkwODExWhgPMjEyNjA5MTYw
OTA4MTFaMCQxIjAgBgNVBAMMGXNoYXJkcyBwcm94eSBvcmFjbGUgb3RoZXIwWTAT
BgcqhkjOPQIBBggqhkjOPQMBBwNCAATuGfSNzx9Y8Mhx4s/dr4qP6GbeVoYrOp4Q
Nc8zdbCA/BBbp1ZB5XlWf0Ck6LxuqsgslXFBdaS4JTffz49us2n8MAoGCCqGSM49
BAMCA0gAMEUCIQCEMODUF2fHf083IoC91Quk3Xlx8c4m6TtQGUbc6+OSKwIgSqW2
CUmWV25mwSRMEcFQaKKJXMqN7kQqEnszviQXOc0=
-----END CERTIFICATE-----
`

func crlf(s string) string {
	out := ""
	for _, r := range s {
		if r == '\n' {
			out += "\r"
		}
		out += string(r)
	}
	return out
}

// bytesOf says b as text where it is UTF-8, else as {"base64": …}.
func bytesOf(b []byte) any {
	if utf8.Valid(b) {
		return string(b)
	}
	return map[string]string{"base64": base64.StdEncoding.EncodeToString(b)}
}

type caseOut struct {
	Name    string `json:"name"`
	Bundle  any    `json:"bundle"`
	CA      any    `json:"ca"`
	Error   string `json:"error,omitempty"`
	Changed bool   `json:"changed"`
	After   any    `json:"after,omitempty"`
	// What the step left in the bundle, and the bundle after the cleanup.
	Step    any `json:"step,omitempty"`
	Cleaned any `json:"cleaned,omitempty"`
}

func TestShardsProxyCA(t *testing.T) {
	ca, other := oracleCA, oracleOther
	cases := []struct {
		name, bundle, ca string
		// What the step makes of the bundle after the CA went in.
		step func(string) string
	}{
		{"empty bundle", "", ca, nil},
		{"one other certificate", other, ca, nil},
		{"no final newline", other[:len(other)-1], ca, nil},
		{"the CA held already", other + ca, ca, nil},
		{"the CA held among text", "# comment\n" + ca + "trailing text", ca, nil},
		{"the CA held with CRLF", crlf(other + ca), ca, nil},
		{"the CA held as a trusted certificate", "-----BEGIN TRUSTED CERTIFICATE-----\n" + ca[28:len(ca)-26] + "-----END TRUSTED CERTIFICATE-----\n", ca, nil},
		{"a key block first", "-----BEGIN PRIVATE KEY-----\nAAEC\n-----END PRIVATE KEY-----\n" + other, ca, nil},
		{"an unterminated block", other + "-----BEGIN CERTIFICATE-----\nAAEC\n", ca, nil},
		{"block headers", "-----BEGIN CERTIFICATE-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: x\n\n" + ca[28:], ca, nil},
		{"the CA under a type line cut short", "-----BEGIN CERTIFICATE\n" + ca[28:], ca, nil},
		{"text not UTF-8", "\xff\xfe junk\n" + other, ca, nil},
		{"CA with leading text", other, "leading\n" + ca, nil},
		{"CA without its final newline", other, ca[:len(ca)-1], nil},
		{"CA after a key block", other, "-----BEGIN PRIVATE KEY-----\nAAEC\n-----END PRIVATE KEY-----\n" + ca, nil},
		{"CA with another after it", other, ca + other, nil},
		{"CA with CRLF", other, crlf(ca), nil},
		{"CA with no certificate", other, "-----BEGIN PRIVATE KEY-----\nAAEC\n-----END PRIVATE KEY-----\n", nil},
		{"CA with bad base64", other, "-----BEGIN CERTIFICATE-----\nAA*C\n-----END CERTIFICATE-----\n", nil},
		{"CA that is no PEM", other, "no PEM", nil},
		{"the step added a certificate", other, ca, func(b string) string { return b + other }},
		{"the step added text without a newline", other, ca, func(b string) string { return b + "tail" }},
		{"the step broke the end marker", other, ca, func(b string) string {
			return b[:len(b)-len("# buildkit proxy CA end\n")] + "# end\n"
		}},
		{"the step copied the CA", other, ca, func(b string) string { return b + ca }},
		{"the step rewrote the bundle with the CA", other, ca, func(string) string { return other + ca + other }},
		{"the step rewrote the bundle without it", other, ca, func(string) string { return other }},
		{"the step put the CA in a block's text", other, ca, func(b string) string { return "x-----BEGIN y\n" + b }},
		{"the step emptied the bundle", other, ca, func(string) string { return "" }},
	}
	var out []caseOut
	for _, c := range cases {
		root := t.TempDir()
		bundle := filepath.Join(root, "etc/ssl/certs/ca-certificates.crt")
		if err := os.MkdirAll(filepath.Dir(bundle), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(bundle, []byte(c.bundle), 0o644); err != nil {
			t.Fatal(err)
		}
		o := caseOut{Name: c.name, Bundle: bytesOf([]byte(c.bundle)), CA: bytesOf([]byte(c.ca))}
		cleanup, err := InjectProxyCA(root, []byte(c.ca))
		if err != nil {
			o.Error = err.Error()
			out = append(out, o)
			continue
		}
		after, err := os.ReadFile(bundle)
		if err != nil {
			t.Fatal(err)
		}
		o.Changed = string(after) != c.bundle
		o.After = bytesOf(after)
		step := string(after)
		if c.step != nil {
			step = c.step(step)
			if err := os.WriteFile(bundle, []byte(step), 0o644); err != nil {
				t.Fatal(err)
			}
		}
		o.Step = bytesOf([]byte(step))
		if err := cleanup(); err != nil {
			t.Fatal(err)
		}
		cleaned, err := os.ReadFile(bundle)
		if err != nil {
			t.Fatal(err)
		}
		o.Cleaned = bytesOf(cleaned)
		out = append(out, o)
	}
	dt, err := json.MarshalIndent(map[string]any{"cases": out}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("SHARDS_PROXYCA_OUT"), append(dt, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
