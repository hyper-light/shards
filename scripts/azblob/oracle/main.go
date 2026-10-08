// What azure-sdk-for-go's azblob v1.5.0 (BuildKit v0.28.1's), signed with a shared key as
// BuildKit's azblob cache signs with `secret_access_key`, sends for each request that
// cache makes, against a server here that answers as Blob Storage does: each request's
// method, URL, headers (Authorization among them) and body where it is the client's own
// text (a block list). crates/shards/src/build/azblob.rs's tests sign each again and
// make each again (D89).
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"sort"
	"strings"
	"sync"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/to"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/blockblob"
	"github.com/Azure/azure-sdk-for-go/sdk/storage/azblob/container"
)

type request struct {
	Op      string      `json:"op"`
	Method  string      `json:"method"`
	URL     string      `json:"url"`
	Headers [][2]string `json:"headers"`
	Length  int64       `json:"length"`
	Body    string      `json:"body,omitempty"`
}

func main() {
	var mu sync.Mutex
	var seen []request
	op := ""
	blobs := map[string][]byte{}
	containers := map[string]bool{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		defer mu.Unlock()
		var hs [][2]string
		for k, vs := range r.Header {
			for _, v := range vs {
				hs = append(hs, [2]string{strings.ToLower(k), v})
			}
		}
		sort.Slice(hs, func(i, j int) bool { return hs[i][0] < hs[j][0] })
		q := r.URL.Query()
		rec := request{Op: op, Method: r.Method, URL: "http://" + r.Host + r.URL.RequestURI(), Headers: hs, Length: r.ContentLength}
		if q.Get("comp") == "blocklist" {
			rec.Body = string(body)
		}
		seen = append(seen, rec)
		w.Header().Set("x-ms-request-id", "r")
		w.Header().Set("x-ms-version", r.Header.Get("x-ms-version"))
		path := r.URL.Path
		switch {
		case q.Get("restype") == "container" && r.Method == "GET":
			if !containers[path] {
				w.Header().Set("x-ms-error-code", "ContainerNotFound")
				w.WriteHeader(404)
				return
			}
			w.WriteHeader(200)
		case q.Get("restype") == "container" && r.Method == "PUT":
			containers[path] = true
			w.WriteHeader(201)
		case q.Get("comp") == "block":
			blobs[path+"#"+q.Get("blockid")] = body
			w.WriteHeader(201)
		case q.Get("comp") == "blocklist":
			if r.Header.Get("If-None-Match") == "*" && blobs[path] != nil {
				w.Header().Set("x-ms-error-code", "BlobAlreadyExists")
				w.WriteHeader(409)
				return
			}
			blobs[path] = []byte("committed")
			w.Header().Set("ETag", "\"0x1\"")
			w.WriteHeader(201)
		case r.Method == "PUT":
			if r.Header.Get("If-None-Match") == "*" && blobs[path] != nil {
				w.Header().Set("x-ms-error-code", "BlobAlreadyExists")
				w.WriteHeader(409)
				return
			}
			blobs[path] = body
			w.Header().Set("ETag", "\"0x1\"")
			w.WriteHeader(201)
		case r.Method == "HEAD" || r.Method == "GET":
			b, ok := blobs[path]
			if !ok {
				w.Header().Set("x-ms-error-code", "BlobNotFound")
				w.WriteHeader(404)
				return
			}
			w.Header().Set("Content-Length", itoa(len(b)))
			w.Header().Set("x-ms-blob-type", "BlockBlob")
			w.Header().Set("ETag", "\"0x1\"")
			w.Header().Set("Last-Modified", "Thu, 08 Oct 2026 12:34:56 GMT")
			w.WriteHeader(200)
			if r.Method == "GET" {
				w.Write(b)
			}
		default:
			w.WriteHeader(400)
		}
	}))
	defer srv.Close()

	// The account's key is the Azurite emulator's published one.
	cred, err := azblob.NewSharedKeyCredential("devstoreaccount1", "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==")
	must(err)
	client, err := azblob.NewClientWithSharedKeyCredential(srv.URL+"/devstoreaccount1", cred, &azblob.ClientOptions{
		ClientOptions: policy.ClientOptions{Retry: policy.RetryOptions{MaxRetries: -1}},
	})
	must(err)
	ctx := context.Background()
	cc := client.ServiceClient().NewContainerClient("buildkit-cache")

	op = "container properties"
	_, err = cc.GetProperties(ctx, &container.GetPropertiesOptions{})
	if err == nil {
		panic("a container before it was made")
	}
	op = "container create"
	_, err = cc.Create(ctx, &container.CreateOptions{})
	must(err)
	op = "blob properties, none"
	_, err = cc.NewBlobClient("cache/blobs/sha256:abc").GetProperties(ctx, &blob.GetPropertiesOptions{})
	if err == nil {
		panic("a blob before it was written")
	}
	op = "upload, a manifest"
	_, err = cc.NewBlockBlobClient("cache/manifests/buildkit").Upload(ctx, readSeekCloser{bytes.NewReader([]byte(`{"records":{}}`))}, &blockblob.UploadOptions{})
	must(err)
	upload := func(key string, n int) error {
		_, err := cc.NewBlockBlobClient(key).UploadStream(ctx, bytes.NewReader(bytes.Repeat([]byte{7}, n)), &blockblob.UploadStreamOptions{
			BlockSize:   32 * 1024 * 1024,
			Concurrency: 4,
			AccessConditions: &blob.AccessConditions{
				ModifiedAccessConditions: &blob.ModifiedAccessConditions{IfNoneMatch: to.Ptr(azcore.ETagAny)},
			},
		})
		return err
	}
	op = "upload stream, a small layer"
	must(upload("cache/blobs/sha256:small", 1000))
	op = "upload stream, a layer of two blocks"
	must(upload("cache/blobs/sha256:two", 32*1024*1024+5))
	op = "upload stream, a layer there"
	if err := upload("cache/blobs/sha256:two", 10); err == nil {
		panic("a second upload committed")
	}
	op = "blob properties"
	_, err = cc.NewBlobClient("cache/blobs/sha256:small").GetProperties(ctx, &blob.GetPropertiesOptions{})
	must(err)
	op = "download"
	res, err := cc.NewBlockBlobClient("cache/manifests/buildkit").DownloadStream(ctx, &blob.DownloadStreamOptions{})
	must(err)
	io.Copy(io.Discard, res.Body)
	res.Body.Close()
	op = "a key with a space and a snowman"
	_, err = cc.NewBlockBlobClient("a dir/☃").Upload(ctx, readSeekCloser{bytes.NewReader([]byte("x"))}, &blockblob.UploadOptions{})
	must(err)

	// Names whose order turns on the SDK's weight tables, which pass over `-` first.
	op = "metadata, the x-ms headers' order"
	_, err = cc.NewBlockBlobClient("m").Upload(ctx, readSeekCloser{bytes.NewReader([]byte("x"))}, &blockblob.UploadOptions{
		Metadata: map[string]*string{"a": to.Ptr("1"), "a-b": to.Ptr("2"), "aa": to.Ptr("3"), "a_c": to.Ptr("4"), "b": to.Ptr("5")},
	})
	must(err)

	mu.Lock()
	out, err := json.MarshalIndent(seen, "", "  ")
	mu.Unlock()
	must(err)
	must(os.WriteFile(os.Args[1], append(out, '\n'), 0o644))
}

type readSeekCloser struct{ *bytes.Reader }

func (readSeekCloser) Close() error { return nil }

func itoa(n int) string {
	b, _ := json.Marshal(n)
	return string(b)
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
