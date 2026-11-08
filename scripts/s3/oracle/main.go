// What aws-sdk-go-v2's SigV4 signer, as its S3 client configures it (no path escaping
// beyond what the URL has: DisableURIPathEscaping), adds to requests that
// crates/shards/src/build/s3.rs's tests sign again: each case's request, and the headers
// the signer set (D88).
package main

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	v4 "github.com/aws/aws-sdk-go-v2/aws/signer/v4"
)

type header struct {
	Name  string `json:"name"`
	Value string `json:"value"`
}

type kase struct {
	Method      string   `json:"method"`
	URL         string   `json:"url"`
	Headers     []header `json:"headers"`
	Length      int64    `json:"length"`
	PayloadHash string   `json:"payload_hash"`
	Token       string   `json:"token"`
	Region      string   `json:"region"`
	Time        string   `json:"time"`
	Signed      []header `json:"signed"`
}

func main() {
	const unsigned = "UNSIGNED-PAYLOAD"
	const empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
	cases := []kase{
		{Method: "GET", URL: "https://bucket.s3.us-east-1.amazonaws.com/manifests/buildkit", PayloadHash: empty, Region: "us-east-1"},
		{Method: "HEAD", URL: "https://bucket.s3.eu-west-1.amazonaws.com/blobs/sha256:abc", PayloadHash: empty, Region: "eu-west-1"},
		{Method: "PUT", URL: "https://bucket.s3.us-east-1.amazonaws.com/blobs/sha256:abc", Length: 1234, PayloadHash: unsigned, Region: "us-east-1",
			Headers: []header{{"Content-Type", "application/octet-stream"}}},
		{Method: "PUT", URL: "http://127.0.0.1:9000/bucket/cache/manifests/name%20with%20space", Length: 10, PayloadHash: unsigned, Region: "us-west-2", Token: "the-session-token"},
		{Method: "PUT", URL: "https://bucket.s3.us-east-1.amazonaws.com/blobs/sha256:abc", Length: 0, PayloadHash: empty, Region: "us-east-1",
			Headers: []header{{"X-Amz-Copy-Source", "bucket/blobs/sha256:abc"}, {"X-Amz-Metadata-Directive", "REPLACE"}}},
		{Method: "GET", URL: "https://bucket.s3.us-east-1.amazonaws.com/?list-type=2&prefix=blobs%2F&max-keys=10", PayloadHash: empty, Region: "us-east-1"},
		{Method: "GET", URL: "https://bucket.s3.us-east-1.amazonaws.com/a/b~c-d_e.f/%E2%98%83", PayloadHash: empty, Region: "us-east-1",
			Headers: []header{{"Range", "bytes=0-99"}}},
	}
	creds := aws.Credentials{AccessKeyID: "AKIDEXAMPLE", SecretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}
	signer := v4.NewSigner(func(o *v4.SignerOptions) { o.DisableURIPathEscaping = true })
	at := time.Date(2026, 10, 8, 12, 34, 56, 0, time.UTC)
	for i := range cases {
		c := &cases[i]
		c.Time = at.Format(time.RFC3339)
		req, err := http.NewRequest(c.Method, c.URL, nil)
		if err != nil {
			panic(err)
		}
		for _, h := range c.Headers {
			req.Header.Set(h.Name, h.Value)
		}
		req.ContentLength = c.Length
		if c.PayloadHash != "" {
			req.Header.Set("X-Amz-Content-Sha256", c.PayloadHash)
		}
		cr := creds
		cr.SessionToken = c.Token
		if err := signer.SignHTTP(context.Background(), cr, req, c.PayloadHash, "s3", c.Region, at); err != nil {
			panic(err)
		}
		for name, vs := range req.Header {
			c.Signed = append(c.Signed, header{strings.ToLower(name), strings.Join(vs, ",")})
		}
		// Sorted, so that the record is stable.
		for a := 0; a < len(c.Signed); a++ {
			for b := a + 1; b < len(c.Signed); b++ {
				if c.Signed[b].Name < c.Signed[a].Name {
					c.Signed[a], c.Signed[b] = c.Signed[b], c.Signed[a]
				}
			}
		}
	}
	b, err := json.MarshalIndent(cases, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}
