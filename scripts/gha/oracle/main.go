// What go-actions-cache (BuildKit v0.28.1's pin) sends to GitHub's cache service, v2,
// for what BuildKit's gha cache does, against a server here that answers as the service
// and its blob storage do: each request's method, path, query, headers and body (D90).
// crates/shards/src/build/gha.rs's tests make each again.
package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	actionscache "github.com/tonistiigi/go-actions-cache"
)

type request struct {
	Op      string      `json:"op"`
	Method  string      `json:"method"`
	Path    string      `json:"path"`
	Query   string      `json:"query"`
	Headers [][2]string `json:"headers"`
	Length  int64       `json:"length"`
	Body    string      `json:"body,omitempty"`
}

func main() {
	var mu sync.Mutex
	var seen []request
	op := ""
	entries := map[string][]byte{} // committed, by key
	reserved := map[string]bool{}
	staged := map[string][]byte{}
	var ids []string
	var srv *httptest.Server
	srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		defer mu.Unlock()
		var hs [][2]string
		for k, vs := range r.Header {
			k = strings.ToLower(k)
			if k == "x-ms-date" || k == "accept-encoding" || k == "x-ms-client-request-id" {
				continue
			}
			for _, v := range vs {
				hs = append(hs, [2]string{k, v})
			}
		}
		sort.Slice(hs, func(i, j int) bool { return hs[i][0] < hs[j][0] })
		rec := request{Op: op, Method: r.Method, Path: r.URL.Path, Query: r.URL.RawQuery, Headers: hs, Length: r.ContentLength}
		if strings.HasPrefix(r.URL.Path, "/twirp/") || r.URL.Query().Get("comp") == "blocklist" {
			rec.Body = string(body)
		}
		seen = append(seen, rec)
		svc := "/twirp/github.actions.results.api.v1.CacheService/"
		var req struct {
			Key         string   `json:"key"`
			RestoreKeys []string `json:"restore_keys"`
			SizeBytes   int64    `json:"size_bytes"`
		}
		json.Unmarshal(body, &req)
		reply := func(v any) { w.Header().Set("Content-Type", "application/json"); json.NewEncoder(w).Encode(v) }
		switch {
		case r.URL.Path == svc+"GetCacheEntryDownloadURL":
			// The newest committed key with a restore key as its prefix.
			best := ""
			for _, p := range req.RestoreKeys {
				for k := range entries {
					if strings.HasPrefix(k, p) && k > best {
						best = k
					}
				}
				if best != "" {
					break
				}
			}
			if best == "" {
				reply(map[string]any{"ok": false})
				return
			}
			reply(map[string]any{"ok": true, "signed_download_url": srv.URL + "/blob/" + url.PathEscape(best) + "?sig=read", "matched_key": best})
		case r.URL.Path == svc+"CreateCacheEntry":
			if reserved[req.Key] {
				w.WriteHeader(409)
				reply(map[string]any{"code": "already_exists", "msg": "cache entry with the same key, version, and scope already exists"})
				return
			}
			reserved[req.Key] = true
			reply(map[string]any{"ok": true, "signed_upload_url": srv.URL + "/blob/" + url.PathEscape(req.Key) + "?sig=write"})
		case r.URL.Path == svc+"FinalizeCacheEntryUpload":
			entries[req.Key] = staged[req.Key]
			reply(map[string]any{"ok": true, "entry_id": "1"})
		case strings.HasPrefix(r.URL.Path, "/blob/"):
			key := strings.TrimPrefix(r.URL.Path, "/blob/")
			q := r.URL.Query()
			w.Header().Set("x-ms-request-id", "r")
			switch {
			case r.Method == "PUT" && q.Get("comp") == "block":
				staged[key+"#"+q.Get("blockid")] = body
				w.WriteHeader(201)
			case r.Method == "PUT" && q.Get("comp") == "blocklist":
				var whole []byte
				for _, part := range strings.Split(string(body), "<Latest>")[1:] {
					whole = append(whole, staged[key+"#"+strings.Split(part, "</Latest>")[0]]...)
				}
				staged[key] = whole
				w.WriteHeader(201)
			case r.Method == "PUT":
				staged[key] = body
				w.WriteHeader(201)
			case r.Method == "GET":
				b := entries[key]
				w.Header().Set("Content-Length", fmt.Sprint(len(b)))
				w.Header().Set("x-ms-blob-type", "BlockBlob")
				w.WriteHeader(200)
				w.Write(b)
			}
		// The legacy service (v1).
		case r.URL.Path == "/_apis/artifactcache/cache":
			keys := strings.Split(r.URL.Query().Get("keys"), ",")
			best := ""
			for _, p := range keys {
				for k := range entries {
					if strings.HasPrefix(k, p) && k > best {
						best = k
					}
				}
				if best != "" {
					break
				}
			}
			if best == "" {
				w.WriteHeader(204)
				return
			}
			reply(map[string]any{"cacheKey": best, "scope": "refs/heads/main", "archiveLocation": srv.URL + "/v1blob/" + url.PathEscape(best)})
		case r.URL.Path == "/_apis/artifactcache/caches" && r.Method == "POST":
			if reserved[req.Key] {
				w.WriteHeader(409)
				reply(map[string]any{"message": "Cache already exists. Scope: refs/heads/main, Key: " + req.Key, "typeName": "x", "typeKey": "ArtifactCacheItemAlreadyExistsException", "errorCode": 0})
				return
			}
			reserved[req.Key] = true
			ids = append(ids, req.Key)
			reply(map[string]any{"cacheId": len(ids)})
		case strings.HasPrefix(r.URL.Path, "/_apis/artifactcache/caches/"):
			var n int
			fmt.Sscan(strings.TrimPrefix(r.URL.Path, "/_apis/artifactcache/caches/"), &n)
			key := ids[n-1]
			if r.Method == "PATCH" {
				var from, to int
				fmt.Sscanf(r.Header.Get("Content-Range"), "bytes %d-%d/*", &from, &to)
				b := staged[key]
				for len(b) < to+1 {
					b = append(b, 0)
				}
				copy(b[from:], body)
				staged[key] = b
				w.WriteHeader(204)
				return
			}
			entries[key] = staged[key]
			w.WriteHeader(204)
		case strings.HasPrefix(r.URL.Path, "/v1blob/"):
			b := entries[strings.TrimPrefix(r.URL.Path, "/v1blob/")]
			w.Header().Set("Content-Length", fmt.Sprint(len(b)))
			w.WriteHeader(200)
			w.Write(b)
		default:
			w.WriteHeader(404)
		}
	}))
	defer srv.Close()

	claims, _ := json.Marshal(map[string]any{
		"ac":  `[{"Scope":"refs/heads/main","Permission":3},{"Scope":"refs/heads/feature","Permission":1}]`,
		"exp": time.Date(2100, 1, 1, 0, 0, 0, 0, time.UTC).Unix(),
		"nbf": time.Date(2020, 1, 1, 0, 0, 0, 0, time.UTC).Unix(),
	})
	enc := base64.RawURLEncoding.EncodeToString
	token := enc([]byte(`{"alg":"HS256","typ":"JWT"}`)) + "." + enc(claims) + "." + enc([]byte("signature"))
	c, err := actionscache.New(token, srv.URL+"/", true, actionscache.Opt{UserAgent: "buildkit/v0.28.1"})
	must(err)
	ctx := context.Background()
	op = "load, none"
	e, err := c.Load(ctx, "buildkit-blob-1-sha256:small")
	must(err)
	if e != nil {
		panic("an entry before one was saved")
	}
	op = "save, a small layer"
	must(c.Save(ctx, "buildkit-blob-1-sha256:small", actionscache.NewBlob(bytes.Repeat([]byte{7}, 1000))))
	op = "save, a layer of blocks"
	must(c.Save(ctx, "buildkit-blob-1-sha256:big", actionscache.NewBlob(bytes.Repeat([]byte{9}, 5<<20))))
	op = "save, a layer there"
	if err := c.Save(ctx, "buildkit-blob-1-sha256:small", actionscache.NewBlob([]byte("x"))); err == nil {
		panic("a second save")
	}
	op = "load, and download"
	e, err = c.Load(ctx, "buildkit-blob-1-sha256:small")
	must(err)
	var buf bytes.Buffer
	must(e.WriteTo(ctx, &buf))
	if buf.Len() != 1000 {
		panic("the download")
	}
	save := func() error {
		return c.SaveMutable(ctx, "index-buildkit-1-abcd1234", 15*time.Second, func(old *actionscache.Entry) (actionscache.Blob, error) {
			return actionscache.NewBlob([]byte(`{"records":{}}`)), nil
		})
	}
	op = "save mutable, the first"
	must(save())
	op = "save mutable, the next"
	must(save())
	op = "load an index, the newest"
	e, err = c.Load(ctx, "index-buildkit-1-abcd1234")
	must(err)
	if e.Key != "index-buildkit-1-abcd1234#2" {
		panic("not the newest: " + e.Key)
	}

	// The legacy service: a fresh store.
	mu.Lock()
	entries, reserved, staged = map[string][]byte{}, map[string]bool{}, map[string][]byte{}
	mu.Unlock()
	v1, err := actionscache.New(token, srv.URL+"/", false, actionscache.Opt{UserAgent: "buildkit/v0.28.1"})
	must(err)
	op = "v1 load, none"
	e, err = v1.Load(ctx, "buildkit-blob-1-sha256:small")
	must(err)
	if e != nil {
		panic("a v1 entry before one was saved")
	}
	op = "v1 save, a layer of chunks"
	must(v1.Save(ctx, "buildkit-blob-1-sha256:big", actionscache.NewBlob(bytes.Repeat([]byte{9}, 40<<20))))
	op = "v1 save, a layer there"
	if err := v1.Save(ctx, "buildkit-blob-1-sha256:big", actionscache.NewBlob([]byte("x"))); err == nil {
		panic("a second v1 save")
	}
	op = "v1 save mutable"
	must(v1.SaveMutable(ctx, "index-buildkit-1-abcd1234", 15*time.Second, func(old *actionscache.Entry) (actionscache.Blob, error) {
		return actionscache.NewBlob([]byte(`{"records":{}}`)), nil
	}))
	op = "v1 load, and download"
	e, err = v1.Load(ctx, "index-buildkit-1-abcd1234")
	must(err)
	buf.Reset()
	must(e.WriteTo(ctx, &buf))
	if buf.String() != `{"records":{}}` {
		panic("the v1 download: " + buf.String())
	}

	mu.Lock()
	out, err := json.MarshalIndent(map[string]any{"scopes": c.Scopes(), "requests": seen}, "", "  ")
	mu.Unlock()
	must(err)
	// The server's port varies from run to run: the URLs name it as PORT.
	out = bytes.ReplaceAll(out, []byte(strings.TrimPrefix(srv.URL, "http://")), []byte("127.0.0.1:PORT"))
	must(os.WriteFile(os.Args[1], append(out, '\n'), 0o644))
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
