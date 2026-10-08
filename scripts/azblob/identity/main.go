// What azidentity v1.13.1 (BuildKit v0.28.1's), as DefaultAzureCredential uses it for the
// azblob cache, asks Azure AD for a Blob Storage token with: the environment's client
// secret (EnvironmentCredential) and a workload identity's federated token
// (WorkloadIdentityCredential), against an authority here that records each request
// (D95). crates/shards/src/build/azblob.rs's tests make each again.
package main

import (
	"context"
	"crypto/tls"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
)

type request struct {
	Op     string              `json:"op"`
	Method string              `json:"method"`
	Path   string              `json:"path"`
	Query  string              `json:"query"`
	Form   map[string][]string `json:"form"`
	Header [][2]string         `json:"headers"`
}

func main() {
	var seen []request
	op := ""
	srv := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		form, _ := url.ParseQuery(string(body))
		var hs [][2]string
		for k, vs := range r.Header {
			k = strings.ToLower(k)
			if k == "user-agent" || k == "x-client-ver" || k == "accept-encoding" || k == "client-request-id" || k == "x-client-os" || k == "x-client-cpu" || k == "content-length" || k == "return-client-request-id" || k == "x-client-sku" {
				continue
			}
			for _, v := range vs {
				hs = append(hs, [2]string{k, v})
			}
		}
		sort.Slice(hs, func(i, j int) bool { return hs[i][0] < hs[j][0] })
		if strings.HasSuffix(r.URL.Path, "/token") {
			seen = append(seen, request{Op: op, Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Form: form, Header: hs})
			w.Header().Set("Content-Type", "application/json")
			json.NewEncoder(w).Encode(map[string]any{"token_type": "Bearer", "expires_in": 3599, "ext_expires_in": 3599, "access_token": "the-token"})
			return
		}
		// Discovery answered as Azure AD answers it, for the tenant here.
		host := r.Host
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"token_endpoint":            "https://" + host + "/tenant/oauth2/v2.0/token",
			"authorization_endpoint":    "https://" + host + "/tenant/oauth2/v2.0/authorize",
			"issuer":                    "https://" + host + "/tenant/v2.0",
			"tenant_discovery_endpoint": "https://" + host + "/tenant/v2.0/.well-known/openid-configuration",
			"metadata":                  []any{},
		})
	}))
	defer srv.Close()
	client := &http.Client{Transport: &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}}}
	ctx := context.Background()
	scope := policy.TokenRequestOptions{Scopes: []string{"https://storage.azure.com/.default"}}

	os.Setenv("AZURE_AUTHORITY_HOST", srv.URL)
	os.Setenv("AZURE_TENANT_ID", "tenant")
	os.Setenv("AZURE_CLIENT_ID", "client")
	os.Setenv("AZURE_CLIENT_SECRET", "s3cr3t")
	op = "environment, a client secret"
	env, err := azidentity.NewEnvironmentCredential(&azidentity.EnvironmentCredentialOptions{
		ClientOptions:            azcore.ClientOptions{Transport: client},
		DisableInstanceDiscovery: true,
	})
	must(err)
	_, err = env.GetToken(ctx, scope)
	must(err)

	os.Unsetenv("AZURE_CLIENT_SECRET")
	dir, _ := os.MkdirTemp("", "wi")
	tokenFile := filepath.Join(dir, "token")
	must(os.WriteFile(tokenFile, []byte("the-federated-token\n"), 0o600))
	os.Setenv("AZURE_FEDERATED_TOKEN_FILE", tokenFile)
	op = "workload identity"
	wi, err := azidentity.NewWorkloadIdentityCredential(&azidentity.WorkloadIdentityCredentialOptions{
		ClientOptions:            azcore.ClientOptions{Transport: client},
		DisableInstanceDiscovery: true,
	})
	must(err)
	_, err = wi.GetToken(ctx, scope)
	must(err)

	out, err := json.MarshalIndent(seen, "", "  ")
	must(err)
	out = []byte(strings.ReplaceAll(string(out), strings.TrimPrefix(srv.URL, "https://"), "HOST"))
	must(os.WriteFile(os.Args[1], append(out, '\n'), 0o644))
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
