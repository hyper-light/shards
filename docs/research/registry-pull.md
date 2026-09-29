# Pulling images from OCI registries

Research input for `shards pull`, dated 2026-09-28. Pinned sources are listed in §9.

Citation conventions:

- `[repo@tag:path:lines]` cites source or documentation lines at a pinned tag or commit.
- `[spec §x]` cites a specification section.
- `[calc]` marks arithmetic on cited inputs.
- **UNVERIFIED** marks a claim with no acceptable source.

## 1. References

Docker CLI v29.8.1 and containerd v2.4.1 both pin `github.com/distribution/reference` v0.6.0 (ff14fafe), so one grammar serves both [docker-cli@v29.8.1:vendor.mod:21; containerd@v2.4.1:go.mod:36].

**Grammar** [reference@v0.6.0:reference.go:4-26]:

```
reference        := name [ ":" tag ] [ "@" digest ]
name             := [domain '/'] remote-name
domain           := host [':' port-number]
host             := domain-name | IPv4address | \[ IPv6address \]
domain-name      := domain-component ['.' domain-component]*
remote-name      := path-component ['/' path-component]*
path-component   := alpha-numeric [separator alpha-numeric]*
```

**Regexes, as compiled** [reference@v0.6.0:regexp.go:41-136]. The grammar comment gives `separator := /[_.]|__|[-]*/` (reference.go:16); the code uses `[-]+`, and the code wins.

| Atom | Regex |
|---|---|
| `alphanumeric` | `[a-z0-9]+` |
| `separator` | `(?:[._]\|__\|[-]+)` |
| `pathComponent` | `alphanumeric (?:separator alphanumeric)*` |
| `remoteName` | `pathComponent (?:/ pathComponent)*` |
| `domainNameComponent` | `(?:[a-zA-Z0-9]\|[a-zA-Z0-9][a-zA-Z0-9-]*[a-zA-Z0-9])` |
| `domainName` | `domainNameComponent (?:\. domainNameComponent)*` |
| `ipv6address` | `\[(?:[a-fA-F0-9:]+)\]` (no zone IDs, no IPv4-mapped) |
| `domainAndPort` | `(?:domainName\|ipv6address)(?::[0-9]+)?` |
| `tag` | `[\w][\w.-]{0,127}` (1–128 chars, mixed case allowed) |
| `digestPat` | `[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*[:][[:xdigit:]]{32,}` |
| `identifier` | `([a-f0-9]{64})` |
| `ReferenceRegexp` | `^(namePat)(?::(tag))?(?:@(digestPat))?$`, with `namePat = (?:domainAndPort/)? remoteName` |

**The digest is then re-validated by go-digest v1.0.0**, which is stricter than `digestPat` [reference.go:217-223; go-digest@v1.0.0:digest.go:103-117, algorithm.go:33-61, 179-193]:
- only `sha256`, `sha384` and `sha512` are available; other algorithms fail with "unsupported digest algorithm";
- the hex part must be exactly `2 × hash size` characters and lowercase (`^[a-f0-9]{64}$` for sha256).

**Normalization (`ParseNormalizedNamed`)** [reference@v0.6.0:normalize.go:56-80, 126-171]:
1. A bare 64-hex string is refused ("cannot specify 64-byte hexadecimal strings").
2. Split at the first `/`. With no `/`, the whole input is a Docker Hub official name: `docker.io/library/<input>`. So `localhost:5000` alone parses as `docker.io/library/localhost` with tag `5000`; the FIXME at :135 admits this.
3. The first component is a **domain** if it is exactly `localhost`, contains `.` or `:`, or contains an uppercase letter (uppercase is illegal in paths). `index.docker.io` is rewritten to `docker.io`. Otherwise the input has no domain and gets `docker.io`.
4. On `docker.io` only, a one-component path gets `library/`: `docker.io/ubuntu` → `docker.io/library/ubuntu`. Other hosts are never prefixed. `registry-1.docker.io/…` is *not* canonicalized to `docker.io`; it is just another domain.
5. The path (text after the domain, up to the first `:`) must be lowercase, else "repository name must be lowercase". Domains may be mixed case; tags may be mixed case.

**Default tag.** `latest`, added only to name-only references (`TagNameOnly`) [normalize.go:38-39, 228-242]. `docker pull` calls `ParseNormalizedNamed` then `TagNameOnly`, and prints "Using default tag: latest" [docker-cli@v29.8.1:cli/command/image/pull.go:70-80].

**Tag plus digest.** `name:tag@digest` is legal. `ParseDockerRef` drops the tag and keeps the digest [normalize.go:88-121], and containerd's resolver requests `manifests/<digest>` whenever a digest is present, ignoring the tag [containerd@v2.4.1:core/remotes/docker/resolver.go:257-277]. The digest wins.

**Length.** The path, *excluding* the domain, may be at most 255 characters (`RepositoryNameTotalLengthMax`) [reference.go:37-45, 209-211]; the tests accept `example.com/` + 255 × `a` and reject 256 [reference_test.go:271-273, 366-367]. There is no separate limit on the domain. The tag is at most 128 characters by regex.

**Docker Hub hostnames.** Three names are in play:

| Name | Role | Source |
|---|---|---|
| `docker.io` | Canonical domain in references | normalize.go:22-31 |
| `registry-1.docker.io` | The host actually contacted for `/v2/` | containerd resolver.go:141-146, registry.go:200-202; docker-cli internal/registry/config.go:58-64 |
| `https://index.docker.io/v1/` | Key under which `docker login` stores Hub credentials | docker-cli cli/config/configfile/file.go:24-45; internal/registry/config.go:49-54 |

**Plain HTTP.** HTTPS everywhere except loopback, where containerd's two code paths differ:
- `ConfigureDefaultRegistries` (library default) uses plain HTTP for `localhost`, `127.0.0.0/8` and `::1`, with or without a port [containerd@v2.4.1:core/remotes/docker/registry.go:168-252; resolver.go:201-205].
- The `hosts.toml` path (ctr, CRI) uses HTTPS-only for loopback on no port or 443; on any other port it uses HTTP with HTTPS fallback, and it skips TLS verification for loopback unless the port is 80 [core/remotes/docker/config/hosts.go:101-133].

## 2. Protocol

**Endpoints a pull uses** [dist-spec@v1.1.1:spec.md:757-784]:

| ID | Request | Success | Failure | Notes |
|---|---|---|---|---|
| end-1 | `GET /v2/` | 200 | 404/401 | "MAY be used for authentication" (763). Optional: containerd's pull path never requests it. It starts with a manifest HEAD and authenticates on the first 401 [containerd resolver.go:313-333, 863-876] |
| end-3 | `GET`/`HEAD /v2/<name>/manifests/<reference>` | 200 | 404 | `<reference>` is a digest or a tag, nothing else (153-154) |
| end-2 | `GET`/`HEAD /v2/<name>/blobs/<digest>` | 200 | 404 | Range SHOULD be supported (202) |

- **Names and tags on the wire.** `<name>` must match `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*(\/[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*)*`; a tag must match `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}` [spec.md:155-166]. The spec notes that many clients cap `host[:port]/name` at 255 characters (160-162). distribution/reference caps only the path (§1), so a long hostname can pass our parser and still be refused elsewhere.
- **Errors.** A 4xx body, if JSON, is `{"errors":[{"code","message","detail"}]}`, with codes `BLOB_UNKNOWN` … `TOOMANYREQUESTS` [spec.md:786-825]. The body is optional and "MAY be in any format" (788), so never require it.
- **Warnings.** `Warning: 299 - "…"` headers; at most 4096 bytes in total; clients SHOULD show them and MUST NOT act on them [spec.md:827-845].

**`Content-Type` and `mediaType`.** The response `Content-Type` names the manifest type. Clients SHOULD ignore parameters on it, and SHOULD reject a manifest whose `mediaType` field disagrees with `Content-Type` [spec.md:168-174]. containerd strips parameters, and maps `text/plain` to Docker schema 1 for one old registry [containerd@v2.4.1:core/remotes/docker/resolver.go:217-230]. containerd v2 refuses schema 1 outright [resolver.go:447-450].

**`Docker-Content-Digest`.**
- It is a legacy, OPTIONAL header that clients "SHOULD NOT depend on" [spec.md:49-52].
- On a manifest response it is the canonical digest and "MAY differ from the provided digest"; a client that uses it MUST verify it against the body [spec.md:177-183].
- On a blob response it MUST match the body if present [spec.md:195-198].
- HEAD responses SHOULD carry it and `Content-Length` [spec.md:212-214].

**HEAD vs GET (containerd's resolve).** Resolution turns a tag into a descriptor (digest, size, media type) [containerd@v2.4.1:core/remotes/docker/resolver.go:245-494]:
1. `HEAD manifests/<tag or digest>` with the resolve `Accept` list below.
2. With a digest in the reference, the digest comes from the reference. With a tag, it comes from `Docker-Content-Digest`, and only when `Content-Length` is also present; resolution is the one place containerd trusts the registry (393-405).
3. If the header or the length is missing, GET the manifest and hash the body (406-459).
4. A 405 on a manifest HEAD is retried as GET (877-883). A HEAD 403 has no body, so containerd issues a GET only to fetch the error text (362-377).
5. Manifests over `MaxManifestSize` = `4 * 1048 * 1048` (4,393,216 bytes, not 4 MiB) are rejected (53-64, 460-467) [calc].
6. If the manifests endpoint 404s for a *digest*, containerd falls back to `blobs/<digest>` (268-272, 304-312).

Docker Hub counts pulls as manifest GETs and not HEADs (§3), so the HEAD-first shape is also cheaper against Hub's limit.

**`Accept`, in order.** Resolve (the HEAD or GET of the tag) sends exactly [containerd@v2.4.1:core/remotes/docker/resolver.go:170-178]:

```
Accept: application/vnd.docker.distribution.manifest.v2+json,
        application/vnd.docker.distribution.manifest.list.v2+json,
        application/vnd.oci.image.manifest.v1+json,
        application/vnd.oci.image.index.v1+json, */*
```

(one header, comma-joined, no q-values). Every later fetch by descriptor sends `Accept: <descriptor mediaType>, */*` [resolver.go:938-944; fetcher.go:501]. Manifests and indexes are fetched from `manifests/<digest>`, everything else from `blobs/<digest>` [fetcher.go:317-360].

**HTTP compression on blobs.** containerd sends `Accept-Encoding: zstd;q=1.0, gzip;q=0.8, deflate;q=0.5` on fetches, and undoes any `Content-Encoding` (zstd, gzip, deflate, identity; others are errors) before the bytes are hashed [fetcher.go:502, 540-542, 636-660]. This is transport encoding, separate from the layer's own media-type compression. A client that sends no `Accept-Encoding` gets identity bytes, which is simpler.

**Redirects.**
- The blob endpoint "may issue a 307 (302 for <HTTP 1.1) redirect to another service", and clients "should be prepared to handle redirects" [distribution@v3.1.2:docs/content/spec/api.md:477-478, 2427-2440]. distribution's registry does this with `http.StatusTemporaryRedirect` to a storage-driver URL [registry/storage/blobserver.go:37-48]. distribution-spec v1.1.1 does not mention redirects for pulls (grep of spec.md).
- **Go drops `Authorization` across hosts.** On every hop, net/http copies the *initial* request's headers. It drops `Authorization`, `Www-Authenticate`, `Cookie`, `Cookie2`, `Proxy-Authorization` and `Proxy-Authenticate` once a hop's host is neither the initial host nor a subdomain of it. The drop is sticky for later hops, and hosts containing `:` or `%` (IPv6) never match as subdomains [go@go1.27.1:src/net/http/client.go:43-51, 696-705, 772-843, 1017-1062]. 307 and 308 keep the method; 301, 302 and 303 turn non-GET/HEAD into GET [client.go:514-541]. The default limit is 10 redirects [client.go:845-850].
- **containerd does the same.** It keeps the 10-redirect cap and re-runs its authorizer on each hop [containerd@v2.4.1:core/remotes/docker/resolver.go:699-713]. The authorizer keys tokens by `req.URL.Host`, so an object-storage host gets no `Authorization` [authorizer.go:120-151]. For a descriptor's external `urls` (non-distributable layers), containerd deletes `Authorization`, `Proxy-Authorization`, `Cookie` and `Cookie2` unless the URL has the registry's scheme, host and effective port [fetcher.go:218-250, 273-299].
- **Observed** (anonymous probe, 2026-09-28):
  - A Docker Hub blob GET returned `HTTP/2 307` to `production.cloudfront.docker.com`, 10 of 10 times over IPv4 and IPv6. GHCR returned `307` to `pkg-containers.githubusercontent.com`.
  - Following either redirect *without* `Authorization` and with `Range: bytes=0-15` returned `206`, and `bytes=16-` returned `206` with a matching `Content-Range`.
- **Rule for us:** follow 307, 308, 301, 302 and 303 on GET and HEAD, up to 10 hops. Send `Authorization` only to the registry host that issued the token. Never forward it to another host, including to that host's subdomains; this is stricter than Go, and safe because tokens are per host (§3). Presigned object-store URLs carry their own authority in the query string, so log them redacted, as containerd does [resolver.go:904-936].

**Range and resuming.**
- The spec's "Resumable Pull" use case: keep partial data and use `Range` [spec.md:100-104]. Registries SHOULD support RFC 9110 ranges on blobs (202); distribution serves blobs through `http.ServeContent`, which does [blobserver.go:73].
- containerd resumes with `Range: bytes=<offset>-` [resolver.go:946-948]. It accepts a 206, or a `Content-Range` starting at `bytes <offset>-`. On a 200 that ignored the range it reads and discards `offset` bytes, and it then disables parallel fetching [resolver.go:739-770; fetcher.go:503-526].
- Within one download, an unexpected EOF re-opens at the current offset, up to 3 times without progress [httpreadseeker.go:28, 47-92, 145-179].
- Across restarts, the content store keeps the partial ingest and `content.Copy` seeks the new reader to the stored offset [core/content/helpers.go:191-223]. The final commit checks size and digest over the whole blob, so a resumed download is verified like a fresh one [plugins/content/local/writer.go:111-136].
- Optional parallel range fetches (`MaxConcurrentDownloads` × `ConcurrentLayerFetchBuffer` chunks) are used only when a response is not compressed in transit and larger than one chunk [fetcher.go:488-560].

**Retries (containerd).**
- Up to 5 attempts per request [resolver.go:772].
- 401 → re-authorize and retry. 408 and 429 → retry immediately, with no backoff and no `Retry-After` handling. 500, 503 and 504 → retry only on the last mirror, and not twice for the same status [resolver.go:863-898].
- Transport timeouts and EOFs retry after 50 ms, on the last host only [resolver.go:816-861].
- An immediate retry after a 429 cannot help against Docker Hub's pull window (documented as 6 h, observed as 1 h; §3.2). We should surface 429s with the rate-limit headers instead.

## 3. Auth

### 3.1 The challenge–token flow

**Specified flow** [distribution@v3.1.2:docs/content/spec/auth/token.md:13-24, 46-84, 239-248]:
1. Request the resource.
2. On `401`, read the challenge, for example `WWW-Authenticate: Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:samalba/my-app:pull,push"`.
3. `GET <realm>?service=<service>&scope=<scope>`, with one `scope` parameter per scope.
4. Retry with `Authorization: Bearer <token>`.

**Token response** [token.md:134-180]:
- `token` and/or `access_token`; at least one must be present. If they differ, "the client's choice is undefined". containerd prefers `access_token` [containerd@v2.4.1:core/remotes/docker/auth/fetch.go:222-231].
- `expires_in` is optional and **defaults to 60 s**; servers should never issue less than 60 s.
- `issued_at` is optional (RFC 3339); without it, expiry counts from when the exchange completed.
- `refresh_token` is returned only when `offline_token=true` is requested.
- Tokens are opaque: clients "should not" parse them [oauth.md:118-121].

**Scope grammar** [scope.md:94-117]:

```
scope          := resourcescope [ ' ' resourcescope ]*
resourcescope  := resourcetype ":" resourcename ":" action [ ',' action ]*
```

The resource name may contain one `:` (a port), so splitting on the first three colons is wrong (scope.md:112-117). containerd requests `repository:<path>:pull`, where `<path>` is the repository path without the host [core/remotes/docker/scope.go:29-42]. It merges the context's scopes with the challenge's, sorts them and removes exact duplicates [scope.go:75-101].

**Parsing `WWW-Authenticate` (containerd)** [core/remotes/docker/auth/parse.go:97-200]:
- One challenge per header *line*. Several `WWW-Authenticate` lines are allowed, but several challenges in one line are not split.
- The scheme is a case-insensitive token: `basic`, `digest` or `bearer`. Others are skipped. Challenges are sorted bearer > digest > basic.
- Parameters are `key=value` pairs separated by commas. Keys are lowercased; values are an RFC 2616 token or a quoted string with `\` escapes.
- Parsing stops silently at the first malformed byte.
- `realm` is required; `service` and `scope` are optional; `scope` splits on spaces [auth/fetch.go:41-68].

**Which request is sent (containerd `doBearerAuth`)** [core/remotes/docker/authorizer.go:275-348]:

| Credentials | Request | Fallback |
|---|---|---|
| None (anonymous) | `GET realm?service=…&scope=…` | — |
| Username + password | OAuth2 `POST realm`, form `grant_type=password&username=…&password=…&service=…&client_id=containerd-client&scope=<space-joined>` [fetch.go:97-156] | On 404, 401 or 400 (or 405 with a username): `GET` with `Authorization: Basic` [authorizer.go:318-330] |
| Identity token (username empty) | OAuth2 `POST`, `grant_type=refresh_token&refresh_token=<identity token>` [fetch.go:110-117] | Same |

The POST form is specified in [oauth.md:26-190], which notes that "Not all token servers implement oauth2" (13-16). The POST response requires `access_token`, `scope` and `expires_in` (111-159).

**Basic challenge.** For `WWW-Authenticate: Basic`, containerd sends `Authorization: Basic base64(user:secret)` straight to the registry, and only if credentials exist [authorizer.go:195-209, 264-273].

**Caching and expiry (containerd)** [authorizer.go:120-151, 153-213, 275-305, 350-386]:
- Tokens are cached per registry host and per sorted scope string. Concurrent requests for the same scope share one fetch.
- Expiry is `now + expires_in` at receipt; `issued_at` is ignored. **A missing `expires_in` caches the token forever**, where the spec says 60 s. We should use 60 s.
- A 401 whose challenge carries `error=…` drops the cached handler and retries once; a second identical failure is `ErrInvalidAuthorization`.

**Realm safety.** containerd accepts any parseable realm URL, including `http://` or a foreign host, and sends the user's password or Basic credentials to it [fetch.go:43-58, 103-126, 195-197]. **Our rule:** the realm must be `https`, except for a loopback plain-HTTP registry; otherwise we refuse to send credentials. The realm may legitimately sit on another host (Docker Hub: `auth.docker.io`).

### 3.2 Docker Hub specifics

- **Hosts:** `registry-1.docker.io` serves `/v2/`; the token realm is `https://auth.docker.io/token` with `service=registry.docker.io` [docker/docs@87b2b3d1:content/manuals/docker-hub/usage/pulls.md:173-193]. Credentials are stored under `https://index.docker.io/v1/` (§1).
- **Pull rate limits (per 6 hours):**
  - unauthenticated: 100 per IPv4 address or IPv6 /64;
  - Personal (authenticated): 200;
  - Pro, Team and Business: unlimited [pulls.md:12-25; usage/_index.md:15-21].
- **What counts as a pull:** a manifest GET. "Using GET emulates a real pull and counts towards the limit. Using HEAD won't." A multi-arch pull counts once per architecture pulled [pulls.md:27-39, 187-189].
- **Headers:** `ratelimit-limit: 100;w=21600`, `ratelimit-remaining: 20;w=21600` and `docker-ratelimit-source: <ip>`. Their absence means the pull is not limited [pulls.md:195-210].
- **Over the limit:** `429` on the manifest request, with the body "You have reached your pull rate limit…" [pulls.md:158-163].
- **Abuse limit:** a separate limit, "in the order of thousands of requests per minute" per IPv4 or IPv6 /64, which returns a bare `429` [usage/_index.md:33-45].
- **Observed** (anonymous probe from one IPv4 address, 2026-09-28):
  - `GET /v2/` → 401 with `Bearer realm="https://auth.docker.io/token",service="registry.docker.io"` and no `scope`. A manifest HEAD → 401 that adds `scope="repository:library/alpine:pull"`.
  - The anonymous token JSON carried `token` and `access_token` (equal), `expires_in: 300` and `issued_at`.
  - The authorized HEAD returned `ratelimit-limit: 100;w=3600`: a **3600 s window, where the docs say 21600 s**. Service and docs disagree, so parse `w=` at runtime rather than hard-code 6 hours.
  - GHCR challenged with `realm="https://ghcr.io/token",service="ghcr.io"`, and its token JSON had only `token`, with no `expires_in`. The spec's 60 s default must then apply; containerd would cache that token forever (§3.1).
- **Consequences:**
  - Resolve with HEAD.
  - Fetch the index and the one platform manifest by digest.
  - Never re-GET a manifest we already hold by digest.
  - Report `ratelimit-*` on 429 instead of retrying.

### 3.3 Finding `docker login` credentials

The Docker CLI v29.8.1 reads credentials in these steps:

1. **File.** `$DOCKER_CONFIG/config.json`, else `<home>/.docker/config.json`. Home is `os.UserHomeDir()`, falling back to the passwd entry off Windows. A missing file is an empty config [docker-cli@v29.8.1:cli/config/config.go:19-28, 56-84, 134-143].
2. **Key.** `docker.io` and `index.docker.io` map to `https://index.docker.io/v1/`; any other registry uses its `host[:port]` as written in the reference [cli/config/configfile/file.go:23-49, 386-390].
3. **Store.**
   - `credHelpers[key]` if present, else `credsStore`, else the file itself [file.go:318-325, 392-402].
   - Only when the config holds no auth at all (no `credsStore`, `credHelpers` or `auths`) does the CLI fall back to the platform default helper, and only if `docker-credential-<name>` is on `PATH`: `osxkeychain` on macOS, `wincred` on Windows, `pass` if `pass` is installed else `secretservice` on Linux [config.go:167-176; credentials/default_store.go:5-28; default_store_{darwin,windows,linux}.go].
   - `DOCKER_AUTH_CONFIG` (JSON `{"auths":{host:{"auth":…}}}`, only `auth` allowed) overrides, falling back to the configured store [file.go:80-103, 327-379].
4. **File store.**
   - It looks up the exact key in `auths`, else the first key whose `ConvertToHostname` (scheme and path stripped) equals it [credentials/file_store.go:42-57, 102-127].
   - `auth` is base64(`user:password`), split at the first `:`, with trailing NULs trimmed. An empty username is an error [file.go:127-144, 295-316].
   - Entries may also carry `identitytoken` and `registrytoken` [cli/config/types/authconfig.go:4-17].
5. **Helper store.** When a helper is selected for a host, the file's credentials for that host are ignored: a helper "not found" yields empty credentials [credentials/native_store.go:43-57, 118-141].

**Credential-helper protocol** [docker-credential-helpers@v0.9.9]:
- **Invocation:** `docker-credential-<suffix> get`, resolved on `PATH`, with stderr passed through [client/command.go:18-57; native_store.go:9-29].
- **stdin:** the server URL, no newline required; the helper trims whitespace [client/client.go:46-50; credentials/credentials.go:137-152].
- **stdout, success:** one JSON object `{"ServerURL":…,"Username":…,"Secret":…}` [credentials.go:28-32, 159-170; client.go:64-72].
- **Failure:** exit ≠ 0 with the message on **stdout** [credentials.go:80-83]. `credentials not found in native keychain` (whitespace-trimmed) means not found, which is not an error [credentials/error.go:11, 53; client.go:51-55].
- **Identity tokens:** `Username == "<token>"` means `Secret` is an identity token (refresh token) [native_store.go:11, 132-136].
- **Other actions:** `store`, `erase`, `list` and `version` [credentials.go:20-24].

**From stored credentials to the token request** (dockerd v29.8.1 on its containerd image store) [moby@docker-v29.8.1:daemon/containerd/resolver.go:69-123; daemon/pkg/registry/config.go:42-54]:
- `registrytoken` → `Authorization: Bearer <it>`, sent as-is to the registry host.
- `identitytoken` → OAuth2 `refresh_token` grant.
- Otherwise username and password.
- Credentials are released only to the host they were stored for; `index.docker.io`/`docker.io` map to `registry-1.docker.io`.

## 4. Selecting a platform

containerd v2.4.1, docker/cli v29.8.1 and dockerd v29.8.1 all use `containerd/platforms` v1.0.0-rc.5 (94edf533, the latest tag) [containerd@v2.4.1:go.mod:28; docker-cli@v29.8.1:vendor.mod:19].

**What the spec asks.**
- `platform.architecture` and `platform.os` use Go's `GOARCH` and `GOOS` values. `variant` values come from a table: arm64 `v8`, `v8.1`, …; amd64 `v1`, `v2`, `v3`, … (Go analogs `GOARM64`, `GOAMD64`) [image-spec@v1.1.1:image-index.md:52-91, 105-116].
- "If multiple manifests match … the first matching entry SHOULD be used" (91).
- An unknown `mediaType` in `manifests` MUST NOT be an error (50).

**Normalization** (`Normalize`) [platforms@v1.0.0-rc.5:platforms.go:473-487; database.go:62-111]:

| Input | Normalized |
|---|---|
| os `""` | `runtime.GOOS`; otherwise lowercased, and `macos` → `darwin` |
| arch `x86_64`, `x86-64`, `amd64` | `amd64`; variant `v1` → `""` |
| arch `aarch64`, `arm64` | `arm64`; variant `8`, `v8`, `v8.0` → `""`; `9`, `9.0`, `v9.0` → `v9` |
| arch `i386` | `386`, variant cleared |
| `armhf` / `armel` / `arm` | `arm/v7` / `arm/v6` / `arm` with `""`,`7` → `v7` and `5`,`6`,`8` → `v5`,`v6`,`v8` |
| `os.features` | sorted and deduplicated |

**arm64 "v8" normalizes to empty, not to `v8`.** A manifest labelled `arm64/v8` and one with no variant are the same platform to the matcher: both sides are normalized before comparison [platforms.go:150-153, 189-194].

**Match order for one target** (`Only(p)` = `Ordered(platformVector(Normalize(p)))`) [compare.go:62-153, 249-313]:
- **amd64/vN:** `amd64/vN`, `amd64/vN-1`, …, `amd64/v1`, then `386`. With the variant empty or `v1`: `amd64`, then `386`.
- **arm64/v8.x:** `arm64/v8.x`, …, `arm64/v8`, then `arm/v8`, `arm/v7`, `arm/v6`, `arm/v5`. An empty variant is treated as `v8`.
- **arm64/v9.x:** `v9.x` … `v9`, then `v8.min(x+5, 9)` … `v8`, then the 32-bit `arm` list [compare.go:39-60].
- A candidate matches when OS, architecture and variant are equal after normalization, and its `os.features` are a subset of the target's. On Windows the OS version must also match [platforms.go:189-231].
- `Less` ranks candidates by the first matcher each satisfies. Ties prefer more `os.features` [compare.go:290-313].
- `OnlyStrict(p)` matches only `p` itself, with no older variants and no 32-bit fallback [compare.go:237-247].

**How the host platform is chosen.**
- **containerd.** `DefaultSpec()` is `{GOOS, GOARCH, cpuVariant()}` [defaults_unix.go:28-40; defaults_darwin.go:28-44; defaults_windows.go:28-42].
  - `cpuVariant` probes only ARM. On Linux it reads `/proc/cpuinfo` "CPU architecture", falling back to `uname`; on macOS and Windows arm64 is hard-coded to `v8` [cpuinfo.go:32-43; cpuinfo_linux.go:76-158; cpuinfo_other.go:26-55]. **containerd never detects amd64 levels**, so its default on amd64 matches only `amd64`(v1) and `386`, and an index offering only `amd64/v3` fails.
  - On macOS, `Default()` is `Ordered(darwin/<arch>, linux/<arch>)` [defaults_darwin.go:38-44].
- **dockerd (containerd image store).** It uses `DefaultSpec()`, but on amd64 sets the variant from CPUID with `tonistiigi/go-archvariant` [moby@docker-v29.8.1:daemon/containerd/platform_matchers.go:67-83].
  - v2 = SSE3, SSSE3, CX16, SSE4.1, SSE4.2, POPCNT, LAHF.
  - v3 = v2 + AVX, AVX2, BMI1/2, FMA, F16C, MOVBE, ABM and OS XSAVE support for YMM.
  - v4 = v3 + AVX-512 F/DQ/CD/BW/VL with OS ZMM support [vendor/github.com/tonistiigi/go-archvariant/amd64variant.go:17-140].
  - It pulls with `platforms.Only(that)` [daemon/containerd/image_pull.go:107-142]. A v3 host therefore prefers `amd64/v3`, then v2, v1, `386`.
  - `docker pull --platform X` (or `DOCKER_DEFAULT_PLATFORM`) replaces the host platform with `platforms.Parse(X)` [docker-cli@v29.8.1:cli/command/image/pull.go:61, 84-92].

**Choosing from an index** [containerd@v2.4.1:core/images/image.go:153-255; handlers.go:385-414; moby daemon/internal/distribution/pull_v2_unix.go:21-48]:
1. Keep entries whose `platform` matches, plus entries with no `platform`.
2. Stable-sort by `Less`, so ties keep index order and unlabelled entries go last.
3. Take the first. For a bare manifest with no descriptor platform, containerd checks the config's `os`/`architecture`/`variant` instead [image.go:176-190].
4. No match → "no match for platform in manifest".

BuildKit attestation manifests are labelled `unknown/unknown`, so they never match (image-storage §2.5).

**What this means for shards.**
- The target is always **`linux/<guest arch>`**, and the guest arch is the host arch (CLAUDE.md). Never use the host OS: on macOS, containerd's own default would try `darwin` first.
- **arm64: match `arm64` only, with no `arm/v*` fallback, by default.** Our HVF guests expose `ID_AA64PFR0_EL1` with EL0 AArch64-only [platform-measurements.md:204], so 32-bit ARM binaries cannot run, even though our kernel config has `CONFIG_COMPAT=y` [resources/kernel/firecracker-aarch64-6.18.config:477]. KVM hosts with AArch32 EL0 could allow it; gate that on the guest's ID register.
- **amd64: match `amd64/vN` … `v1`, then `386`.** Our x86_64 kernel has `CONFIG_IA32_EMULATION=y` [firecracker-x86_64-6.18.config:657]. Compute N from the **CPUID our VMM exposes to the guest**, using go-archvariant's feature sets, not from the host's CPUID. Until the VMM's CPUID policy is fixed, N = 1 is the conservative choice.
- Implement `Normalize`, `platformVector` and the stable sort as specified above, and honour `--platform` as a strict override (the same vector, built from the user's value).

## 5. Verification

**Digest grammar and algorithms** [image-spec@v1.1.1:descriptor.md:74-103, 135-161]:
- `digest ::= algorithm ":" encoded`, with `algorithm-component ::= [a-z0-9]+` and separators `[+._-]`.
- SHA-256 MUST be implemented, with `encoded` matching `/[a-f0-9]{64}/`; uppercase is forbidden. SHA-512 MAY be, with `/[a-f0-9]{128}/`.
- Unregistered algorithms that fit the grammar SHOULD pass *validation*, but go-digest refuses to *verify* anything other than sha256, sha384 or sha512 (§1).

**What the specs require of a consumer.**
- Content from untrusted sources SHOULD be verified against the descriptor digest before use [descriptor.md:30, 107].
- A length mismatch means the content "SHOULD NOT be trusted" [descriptor.md:32-36].
- Size SHOULD be checked *before* hashing, "to reduce hash collision space", and heavy processing before hashing SHOULD be avoided [descriptor.md:107-109].
- An embedded `data` field must be verified like fetched content [descriptor.md:49-53].
- The distribution-spec rules for `Docker-Content-Digest` and digest references are in §2 [spec.md:177-198].
- On a media-type conflict, the expected (descriptor) type wins if the digest matches; otherwise it is an error [media-types.md:21-33].

**Checks, in pull order** (what we must do, with the containerd v2.4.1 behaviour we match):

| # | Object | Check | Where containerd does it |
|---|---|---|---|
| 1 | Reference by digest | Top-level bytes hash to the reference digest | Fetch by digest, then commit fails with "unexpected commit digest" [plugins/content/local/writer.go:111-136] |
| 2 | Tag resolved by HEAD | The GET body hashes to the `Docker-Content-Digest` used; `Content-Length` present | resolver.go:393-405, then commit as in row 1 |
| 3 | Any manifest or index | Size ≤ cap before parsing. `Content-Type` agrees with the `mediaType` field. Not an index posing as a manifest or the reverse. No schema 1 | `MaxManifestSize` [resolver.go:53-64, 460-467]; `validateMediaType` [core/images/image.go:393-407] |
| 4 | Child manifest, config, layer blobs | Stream size equals `descriptor.size`, then digest equals `descriptor.digest` | `content.Copy` + commit [core/content/helpers.go:191-223; writer.go:111-136] |
| 5 | Config | `rootfs.type == "layers"`, else error [config.md:215-218]; `len(layers) == len(rootfs.diff_ids)` | unpacker.go:388-391 |
| 6 | Each layer, uncompressed | SHA-256 of the **entire** decompressed stream, including bytes after the tar end-of-archive, equals `diff_ids[i]` | apply.go:108-152 (drains trailing data at :134-137); unpacker.go:653-658 |
| 7 | Stack identity | ChainID for caching and keying | unpacker.go:462-492 |

- Rows 1–4 are *blob* digests, computed over bytes as stored, usually compressed. Row 6 is the *DiffID*, over the uncompressed tar. The two "MUST NOT be confused" [config.md:26-31].
- For R5's uncompressed store, row 6 is what makes the stored bytes trustworthy. Row 4 alone would only prove we received what the registry sent under that digest.

**ChainID** [config.md:33-83; image-spec@v1.1.1:identity/chainid.go:30-67]:

```
ChainID(L0)         = DiffID(L0)
ChainID(L0|…|Ln)    = sha256( ChainID(L0|…|Ln-1) + " " + DiffID(Ln) )
```

- The hash input is the ASCII text of the two digest *strings*, prefixes included, joined by one space: `"sha256:<hex> sha256:<hex>"` (chainid.go:61). The raw bytes are not used.
- ChainID(A|B|C) ≠ ChainID(C). That is why image-storage R6 keys flattened EROFS artifacts by ChainID and not by the last DiffID.

**Layer media types** [image-spec@v1.1.1:manifest.md:77-94; media-types.md:10-19, 60-64; layer.md:7-17, 349-361]:

| Media type | Compression | Spec status | containerd v2.4.1 (`DiffCompression`, mediatypes.go:69-110) |
|---|---|---|---|
| `application/vnd.oci.image.layer.v1.tar` | none | MUST support | yes |
| `…layer.v1.tar+gzip` | gzip (RFC 1952) | MUST support | yes |
| `…layer.v1.tar+zstd` | zstd (RFC 8478) | SHOULD support | yes |
| `…layer.nondistributable.v1.tar{,+gzip,+zstd}` | as named | deprecated. Readers are "expected to support" existing images; tar and +gzip are MUST | yes |
| `application/vnd.docker.image.rootfs.diff.tar.gzip` | gzip | "interchangeable" with OCI +gzip | yes |
| `application/vnd.docker.image.rootfs.diff.tar` | "unknown": sniffed | — | yes |
| `application/vnd.docker.image.rootfs.diff.tar.zstd` | zstd | — | yes |
| `application/vnd.docker.image.rootfs.foreign.diff.tar{,.gzip}` | as named | Docker's non-distributable | yes; `IsNonDistributable` [mediatypes.go:133-137] |
| `…+encrypted` | wrapped | — | only with an ocicrypt stream processor |
| anything else | — | MUST NOT error when *storing or copying* | not a layer; the unpack fails |

- **containerd sniffs; it does not trust the media type.** For any compressed type, and for Docker's untyped `diff.tar`, it peeks 10 bytes and picks gzip (`1F 8B 08`), zstd (`28 B5 2F FD`, or a skippable frame `0x184D2A5x`), or uncompressed, whatever the media type says [core/diff/stream.go:91-112; pkg/archive/compression/compression.go:136-242].
- A layer labelled `+gzip` that is really plain tar is therefore accepted, and the DiffID check (row 6) still guards integrity. An OCI `…v1.tar` is never sniffed.
- For gzip, containerd prefers external `igzip`, then `unpigz`, on `PATH`, else klauspost/compress [compression.go:271-286]. That is a hint that Go's in-process inflate was not fast enough for them.
- **Non-distributable layers.** `urls` MAY be present and SHOULD NOT be used to decide distributability [layer.md:361]. containerd tries each http(s) `urls` entry first, stripping credentials for non-registry origins, then falls back to `blobs/` [fetcher.go:273-315].

**Limits we must add (DoS).**
- Manifest and index size: containerd caps them at 4,393,216 bytes.
- Blob size: stop reading at `descriptor.size`, and error if more bytes arrive.
- Uncompressed size: no descriptor records it, so a decompression bomb is caught only by the DiffID check at the end. Enforce a configurable cap and a free-disk check while streaming.
- Count of layers.
- Redirect hops: 10 (§2).

## 6. Rust crates

Versions are the latest on crates.io on 2026-09-28. Crate sources were read from the published `.crate` files, with each file's git commit given in §9.

### 6.1 Measured: which crates can be linted for all 8 targets from a macOS host

**Method.**
- Host: macOS 26.4 (Darwin 25.4.0) arm64, rustc and cargo 1.98.0, Apple clang 21.0.0. No cross sysroots, zig, CMake or NASM are installed.
- Each cell is a one-crate project run with `cargo clippy --target <t>` (codec crates with `-- -D warnings`), 2026-09-28. Cells were re-run with `CC_<triple>=clang`, and one cell was re-run to confirm.
- The harness lives in the session scratchpad and is **not yet committed** (open question M1).

| Crate (configuration) | macOS a64, x64 | linux-gnu x64, a64 | linux-musl x64, a64 | windows-msvc x64, a64 |
|---|---|---|---|---|
| rustls 0.23.45 + **ring** 0.17.14 | pass | FAIL: `x86_64-linux-gnu-gcc` not found. With `CC=clang`: `assert.h` not found | FAIL: `*-linux-musl-gcc` not found. With clang: `stddef.h` | FAIL: `assert.h` not found |
| rustls 0.23.45 + **aws-lc-rs** 1.18.1 (aws-lc-sys 0.45.0) | pass | FAIL: gcc not found. With clang: `stdlib.h` | FAIL: same | FAIL: `stdlib.h` |
| rustls 0.23.45 + rustls-graviola 0.4.0 | pass | pass | pass | pass |
| rustls (no provider) + rustls-platform-verifier 0.7.1 | pass | pass | pass | pass |
| rustls-native-certs 0.8.4; webpki-roots 1.0.9 | pass | pass | pass | pass |
| ureq 3.4.2 (defaults: ring); hyper 1.11.1 + hyper-util + hyper-rustls(ring) + tokio | pass | FAIL (ring) | FAIL (ring) | FAIL (ring) |
| flate2 1.1.10 (miniz_oxide); flate2 + zlib-rs 0.6.8 | pass | pass | pass | pass |
| ruzstd 0.9.0; sha2 0.11.0; serde_json 1.0.151; httparse 1.10.1 | pass | pass | pass | pass |
| zstd 0.14.0 (zstd-sys 2.1.0+zstd.1.5.7) | pass | FAIL: gcc not found. With clang: `string.h` / `mm_malloc.h` | FAIL: `stddef.h` | FAIL: `intrin.h` / `mm_malloc.h` |
| libdeflater 1.26.1 (libdeflate-sys) | pass | FAIL: gcc not found. With clang: `string.h` | FAIL: `stddef.h` | FAIL: `mm_malloc.h` / `intrin.h` |
| libdeflater `freestanding`, `CC=clang` | — | pass | FAIL: `stddef.h` | FAIL: `stdlib.h` |

**Consequence.** Any crate with a C build script (ring, aws-lc-sys, zstd-sys, libdeflate-sys) breaks the CLAUDE.md rule "lint every matrix target before pushing" for 6 of 8 targets. `cargo clippy` runs build scripts, and those need a target C compiler plus a target libc sysroot. **The crypto provider is the only C dependency a pull cannot avoid**: the root store, the platform verifier, JSON, SHA-256, gzip and zstd all have pure-Rust options that pass on all 8 targets.

### 6.2 TLS: rustls with ring vs aws-lc-rs

rustls 0.23.45 is the floor: RUSTSEC-2026-0285 ("TLS 1.3 handshake messages incorrectly accepted across encryption level boundaries") is patched only in ≥ 0.23.45 [rustsec@ef036051:crates/rustls/RUSTSEC-2026-0285.md]. Certificates are verified by rustls-webpki, not by the provider; the provider supplies primitives only [rustls-0.23.45/README.md:46-51; Cargo.toml `[features]` `ring`/`aws_lc_rs` → `webpki/<same>`].

| | **ring 0.17.14** | **aws-lc-rs 1.18.1 / aws-lc-sys 0.45.0** | rustls-graviola 0.4.0 (for contrast) |
|---|---|---|---|
| Build needs | A C (not C++) toolchain for every target. Cross builds need a target sysroot plus `TARGET_CC`/`TARGET_AR`. `.S` files go through the C compiler, except on Windows x86/x64, which use packaged objects. Windows arm64 needs the VS ARM64 tools and clang [ring@2723abbc:BUILDING.md:7-52] | A C/C++ compiler for every target. Non-FIPS never needs CMake, Go or bindgen [aws-lc-rs README.md:22-31; book/src/requirements/linux.md]. Windows x64 needs NASM, or the prebuilt NASM objects that rustls's `aws_lc_rs` feature enables [README.md:98-108; rustls Cargo.toml features]. Windows arm64 needs clang-cl [book/src/requirements/windows.md:7-15]. FIPS adds CMake and Go (plus Ninja on Windows) | rustc only [graviola README.md:15-16] |
| Cross-lint from macOS (§6.1) | 2 of 8 | 2 of 8 | 8 of 8 |
| Third-party review | In scope of Cure53 TLS-01, May–June 2020, sponsored by CNCF, 30 person-days: rustls ≥ 0.16 plus ring, webpki, sct.rs and rustls-native-certs. Four Info/Low findings, none a vulnerability. "No issues were found with regards to the cryptographic engineering of rustls or its underlying ring library." TLS-01-001 recommends formally verified primitives [TLS-01-report.pdf pp. 1–2, 8, 11] | No third-party audit report found in the aws-lc or aws-lc-rs repositories. FIPS 140-3 level 1 certificates #4631, #4816, #5429, #5298 and #5314 cover the **FIPS module only** (the `fips` feature), not the default build [aws-lc@v5.7.0 (vendored by aws-lc-sys 0.45.0):crypto/fipsmodule/FIPS.md:7-14] | None; "This project is very new, so exercise due caution" [README.md:18-22] |
| Formal verification | None claimed; TLS-01-001 notes the absence | SAW proofs in CI for SHA-384/512, HMAC-SHA-384, AES-GCM-256 and AES-KW(P)-256 on x86-64 SandyBridge+, and SHA-384 on Neoverse N1/V1. HOL Light (s2n-bignum) proofs for P-256/384/521, X25519 and Ed25519 arithmetic. **SHA-256 and AES-128-GCM are not in the verified list** [aws-lc@v5.7.0:README.md:161-199] | s2n-bignum assembly for the EC, X25519 and ML-KEM arithmetic [README.md:66-77] |
| Key exchange offered by rustls | X25519, P-256, P-384 [rustls src/crypto/ring/mod.rs:176-181] | **X25519MLKEM768 first**, then X25519, P-256, P-384 (`prefer-post-quantum`) [src/crypto/aws_lc_rs/mod.rs:250-262; src/lib.rs:302-305] | includes ML-KEM-768 |
| Measured against registries (probe, 2026-09-28) | X25519 on every host | X25519MLKEM768 with auth.docker.io, gcr.io, us-docker.pkg.dev, storage.googleapis.com, public.ecr.aws, pkg-containers.githubusercontent.com and production.cloudfront.docker.com; X25519 elsewhere | not probed |
| CPU requirements | Runtime dispatch with portable fallbacks | Runtime dispatch | x86_64 needs AES, SSSE3, AVX, AVX2, ADX, BMI2 and PCLMULQDQ, and **asserts (panics) without them** [src/low/x86_64/cpu.rs:226-260]. Disqualifying under our no-panic rule |
| Maintenance and advisories | Last release 2025-03-11 (crates.io). RUSTSEC-2025-0007 "unmaintained" was withdrawn two days later, after the author gave the rustls team access. RUSTSEC-2025-0009: AES panic with overflow checks, fixed in ≥ 0.17.12 | Release 2026-09-01. aws-lc-sys 2026 advisories: X.509 name constraints, AES-CCM timing, two for PKCS7_verify, and CRL scope, all fixed in ≥ 0.38/0.39 [rustsec crates/aws-lc-sys/RUSTSEC-2026-0044…0048]. None is on rustls's path: rustls verifies certificates with webpki and offers no CCM suites [aws_lc_rs/mod.rs:100-135] | — |
| Default in | ureq [ureq README.md:175-177] | rustls [README.md:46-51, 81-84] | — |

**Keep TLS 1.2 enabled.**
- Docker's allowlist names `production.cloudfront.docker.com` as the pull CDN [docker/docs@87b2b3d1:content/manuals/desktop/setup/allow-list.md:28-29], and Hub redirected there in 10 of 10 samples (probe, 2026-09-28). That host speaks TLS 1.3.
- The Docker-operated `production.cloudflare.docker.com` answers every TLS 1.3-only ClientHello with a `protocol_version` alert. This was reproduced with rustls-ring, rustls-aws-lc-rs and LibreSSL `s_client -tls1_3` (probe, 2026-09-28).
- A client without TLS 1.2 would therefore fail whenever a registry or CDN still runs 1.2 only. rustls implements only ECDHE+AEAD suites for 1.2 [src/crypto/aws_lc_rs/mod.rs:100-135].
- rustls needs its `tls12` feature, which `default-features = false` drops [rustls src/lib.rs:310].

**On the sibling precedent** (`rustls = { version = "0.23", default-features = false, features = ["std", "ring"] }`):
- focal's line omits `tls12`, so it cannot reach TLS 1.2-only endpoints such as the Cloudflare host above. slates adds `tls12`.
- slates records its reason as "the `ring` provider builds without cmake; the default `aws-lc-rs` would need it" [slates/Cargo.toml:81-84]. That was true of older aws-lc-sys. It is not true of aws-lc-sys 0.45 non-FIPS builds (README.md:24-27).
- Both providers now need exactly a C compiler, and both fail the same cross-lint cells. The precedent's reason no longer separates them.

### 6.3 Certificate roots: what Docker does, and what that means for corporate CAs

**Go (dockerd, containerd, the Docker CLI), go1.27.1.**
- **macOS and Windows:** when the pool is the system pool, verification is delegated to the OS: `SecTrustEvaluateWithError` or `CertGetCertificateChain` [src/crypto/x509/verify.go:564-581; root_darwin.go:16-79; root_windows.go:202-249]. Roots that an administrator or MDM installs in the keychain or the Windows store are therefore trusted, along with the OS's distrust decisions.
- Since go1.27, `SSL_CERT_FILE` or `SSL_CERT_DIR` on these OSes replaces the platform verifier with on-disk roots (GODEBUG `x509sslcertoverrideplatform=0` reverts this) [root.go:124-151].
- **Linux:** Go reads the first of six bundle files (Debian, RHEL, OpenSUSE, OpenELEC, CentOS 7, Alpine) plus every file in `/etc/ssl/certs` and `/etc/pki/tls/certs`; `SSL_CERT_FILE`/`SSL_CERT_DIR` override these [root_linux.go:9-23; root.go:153-200].

**dockerd adds per-registry CAs.**
- Each `*.crt` in `/etc/docker/certs.d/<host[:port]>/` is appended to the system pool for that registry. `*.cert`/`*.key` pairs are client certificates [moby@docker-v29.8.1:daemon/pkg/registry/registry.go:22-110].
- Rootless: `$XDG_CONFIG_HOME/docker/certs.d`. Windows: `%ProgramData%\docker\certs.d`, with `:` stripped from the directory name [daemon/pkg/registry/config.go:74-84; registry.go:23-31].

| | rustls-platform-verifier 0.7.1 | rustls-native-certs 0.8.4 | webpki-roots 1.0.9 |
|---|---|---|---|
| Trust source | macOS/Windows: the OS verifier (Security.framework / Windows API), with revocation and the OS's constraints. Linux: the system bundle via rustls-native-certs and openssl-probe, verified by webpki, **no revocation** [README.md:9-47] | `SSL_CERT_FILE` first; otherwise the Windows store, the macOS keychain (trust settings merged), or openssl-probe on Linux [README.md:38-56] | Mozilla's roots (CCADB), compiled in [README.md:1-14] |
| Distrust decisions | Honoured on macOS/Windows | "All roots are treated equally regardless of their status" [platform-verifier README.md:57] | Static; needs a crate update [README.md:58] |
| Corporate/private CA | Yes. Plus `Verifier::new_with_extra_roots` on Apple, Windows and Linux for per-registry CAs [src/verification/{apple,windows,others}.rs] | Yes on all OSes | **No.** Breaks TLS-intercepting proxies and internal registries |
| Matches Docker | Closest: the same OS verifiers Go uses on macOS/Windows, and the same Linux bundle model | Partial: Linux yes; macOS/Windows lose the OS's (dis)trust logic | No |
| Cross-lint | 8 of 8 (without a C provider, §6.1) | 8 of 8 | 8 of 8 |

The rustls and platform-verifier teams recommend the platform verifier as the default for client applications on common OSes [platform-verifier README.md:60-63]. It is also the one option that reproduces Go's behaviour on the two OSes where Go delegates to the platform.

### 6.4 HTTP/1.1 client: hand-written vs ureq vs hyper

**What a pull needs:**
- GET, HEAD and form POST;
- status line and headers;
- `Content-Length` and `Transfer-Encoding: chunked` bodies ("A recipient MUST be able to parse and decode the chunked transfer coding" [RFC 9112 §7.1]; length rules and smuggling hazards [§6.3, §11]);
- streaming bodies into hashers;
- `Range`;
- redirects under our own `Authorization` policy (§2);
- keep-alive to the registry;
- timeouts;
- proxies (`HTTPS_PROXY`/`NO_PROXY` and CONNECT), because Docker honours them: containerd's transport uses `http.ProxyFromEnvironment` [containerd registry.go:254-257].

HTTP/2 is not needed. Every registry, token and CDN endpoint probed answered HTTP/1.1 (`curl --http1.1`: registry-1.docker.io, auth.docker.io, ghcr.io, both Docker Hub CDNs, pkg-containers.githubusercontent.com, quay.io, public.ecr.aws; probe, 2026-09-28).

| | Hand-written on rustls + httparse | ureq 3.4.2 | hyper 1.11.1 (+ hyper-util, tokio) |
|---|---|---|---|
| Model | Blocking, our code | Blocking, pure Rust, forbids `unsafe` [README.md:25-33] | Async; "relatively low-level … a building block"; points users to reqwest [README.md:12-26] |
| Crates on aarch64-apple-darwin (measured, `cargo tree -e normal`, unique incl. root) | rustls+ring alone: 12 (+ httparse) | 28 with defaults (ring, webpki-roots, gzip) | 38 (hyper, hyper-util, hyper-rustls, tokio, http-body-util) |
| Provider other than ring | Direct `ClientConfig` | Only through `rustls-no-provider` and `unversioned_rustls_crypto_provider`, marked **unstable**, "might change … without a major version bump" [README.md:158-163, 187-195] | Direct via hyper-rustls |
| Redirect auth policy | Ours | Default `Never`: `Authorization` is never attached to a redirected call. `SameHost` keeps it for same-host https [src/config.rs:344-348] | Ours: hyper-util 0.1.21 has no redirect handling (no match for "redirect" in `src/`) |
| Proxies | Ours to write (CONNECT, `NO_PROXY` matching) | HTTP CONNECT built in, SOCKS optional. Reads `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` [README.md:373-392] | Ours or a helper crate |
| Transparent decoding | None; we choose | `gzip` default feature decodes `Content-Encoding`; must be off, or bytes reach the hasher decoded | None |
| Panic policy | Ours (workspace lints) | Upstream's | Upstream's |

### 6.5 gzip

| | flate2 1.1.10 + miniz_oxide 0.9.1 (default) | flate2 + zlib-rs 0.6.8 | libdeflater 1.26.1 (libdeflate, C) |
|---|---|---|---|
| Language | Rust; `#![forbid(unsafe_code)]` [miniz_oxide src/lib.rs; README.md:20] | Rust with `unsafe` (456 occurrences in `src/`, counted) | C via cc |
| Streaming | Yes | Yes | **No: "There is currently no support for streaming"** [ebiggers/libdeflate@v1.26:README.md:3, 122-123]. A multi-GB layer would have to sit in memory whole. Disqualified |
| Cross-lint (§6.1) | 8 of 8 | 8 of 8 | 2 of 8 |
| Maintainer performance claims | None published with numbers | "Performance is generally on-par with zlib-ng" [zlib-rs@v0.6.8:README.md:30-32]. flate2 calls it "the fastest overall" but cites only a blog post [flate2 README.md:82], which we do not cite. No numbers in either repository | "significantly faster than the zlib library" [README.md:12]; no numbers in the README |
| Advisories | — | RUSTSEC-2024-0401: stack overflow on malicious input, fixed ≥ 0.4.0 | — |

- **Maintainer numbers for a baseline.** The zstd maintainers report stock zlib 1.2.11 `-1` decompressing at **400 MB/s**, against 1580 MB/s for zstd 1.5.6 `-1`. Method: lzbench, Silesia corpus, one Core i7-9700K at 4.9 GHz, Ubuntu 20.04, gcc 9.4.0; n and variance not stated [facebook/zstd@v1.5.7:README.md:29-53].
- **What containerd does.** It shells out to `igzip`, then `unpigz`, when either is on `PATH` [containerd compression.go:271-286], which suggests in-process Go inflate was a bottleneck for them.
- **No primary numbers compare the Rust backends' decompression throughput.** Measure it (M3).

### 6.6 zstd

| | zstd 0.14.0 (zstd-sys, C zstd 1.5.7) | ruzstd 0.9.0 (Rust) |
|---|---|---|
| Cross-lint (§6.1) | 2 of 8 | 8 of 8 |
| Decoder speed | 1580 MB/s at level `-1` on the i7-9700K above [zstd README.md:42-53] | By its author: "about 3.5 times slower" than C zstd decoding enwik9.zst from a ramfs, and "close to only being 1.4 times slower" on less compressible data (an Ubuntu ISO). Measured with `time`; no hardware or n stated [ruzstd Readme.md:22-29] |
| Hardening | Upstream C | Fuzzed with decodecorpus and malformed inputs that "must not make the decoder panic" [Readme.md:131-144]. RUSTSEC-2024-0400 (uninitialised and out-of-bounds reads) fixed in ≥ 0.7.3 |

zstd layers are a minority case: Docker Hub's `library/alpine` still ships `tar+gzip` (probe, 2026-09-28), and OCI makes zstd only a SHOULD (§5). ruzstd's speed on real layers is unmeasured (M3).

### 6.7 SHA-256

| | sha2 0.11.0 (RustCrypto) | ring 0.17.14 `digest` (or aws-lc-rs) |
|---|---|---|
| x86_64 | SHA-NI when CPUID reports sha+sse2+ssse3+sse4.1, else `soft` [src/sha256.rs:52-72; README.md:78-85] | SHA-NI path (`Sha`+`Ssse3`), else AVX, SSSE3 or scalar asm [src/digest/sha2/sha2_32.rs:28-60] |
| aarch64 Linux | ARMv8 `sha2` via `getauxval` [cpufeatures-0.3.1 src/aarch64.rs:24-100] | `HWCAP_SHA2` via `getauxval` [src/cpu/arm/linux.rs:50-70] |
| aarch64 macOS | Always on: `sha2` is a baseline target feature (`rustc --print cfg`) and cpufeatures returns true [aarch64.rs:105-122] | Static baseline includes SHA-256 [src/cpu/arm/darwin.rs:35] |
| **aarch64 Windows** | **No runtime detection**: cpufeatures returns false off Linux, Android and Apple [aarch64.rs:176-183], and `rustc --print cfg` shows no `sha2` for `aarch64-pc-windows-msvc`. So `soft` unless built with `-C target-feature=+sha2` | `IsProcessorFeaturePresent(PF_ARM_V8_CRYPTO_INSTRUCTIONS_AVAILABLE)` [src/cpu/arm/windows.rs:15-34] |
| Cross-lint | 8 of 8 | as the provider (2 of 8) |
| Advisories | RUSTSEC-2021-0100: AVX2 backend miscomputed long messages, 0.9.7 only | — |

- **Hashing volume.** Each pulled byte is hashed at least twice: once as a blob and once, uncompressed, for the DiffID. At the 2.6× median compression ratio [image-storage §2.1, Zhao19], that is about 3.6 bytes of SHA-256 per compressed byte [calc].
- **No primary numbers compare sha2 0.11 and ring on our hosts.** Measure it (M4).
- **Assurance is equal.** AWS-LC's formal proofs cover SHA-384/512 but not SHA-256 (§6.2), so neither side has a verification advantage for SHA-256.

### 6.8 JSON

serde_json 1.0.151 is pure Rust (8 of 8 targets), returns `Result` for every parse, and caps nesting at 128 by default (`remaining_depth: 128`; `unbounded_depth` is opt-in) [serde_json-1.0.151 src/de.rs:63, 1377; Cargo.toml features]. Combine it with the manifest size cap (§5). No alternative is needed.

## 7. Recommendations for shards (ranked)

### R1. Resolve and fetch as containerd does, and store nothing until every digest checks out

**What.**
- **References.** Port distribution/reference v0.6.0's regexes and normalization plus go-digest's validation (§1). Port its test tables (`normalize_test.go`, `reference_test.go`) as unit tests.
- **Resolve.**
  - `HEAD manifests/<tag>` with containerd's `Accept` list (§2).
  - Treat `Docker-Content-Digest` + `Content-Length` only as a pointer: GET by digest and verify.
  - A digest in the reference wins over its tag.
- **Select and fetch.** Choose `linux/<guest arch>` (R6), then fetch the chosen manifest and the config by digest.
- **Stream each layer as one pass:** body → stop at `descriptor.size` → SHA-256 (blob) → sniff and decompress → SHA-256 (DiffID) → uncompressed file.
- **Commit** into the content store only when the size, the blob digest, the DiffID and `len(layers) == len(diff_ids)` all hold (§5 rows 1–7). Key the EROFS artifact by ChainID (image-storage R6).
- **Resume** with `Range: bytes=N-`. Keep the compressed partial as the ingest and re-hash it on restart; containerd likewise keeps partial ingests and seeks to their offset (§2).

**Stricter than containerd:**
- an https-only realm;
- a 60 s default token lifetime;
- `Authorization` only for the issuing host;
- no immediate retry on 429;
- caps on manifest size, blob size, uncompressed size and layer count (§3, §5).

**Why.** §2 and §5; descriptor.md's "verify before consuming"; image-storage R5 and R6.

**Risk.** Uncompressed-only storage cannot reproduce the registry's blob digests for a later `push` or `save` (image-storage §2.5, "Identity"). This is decided in Q1.

### R2. TLS: rustls ≥ 0.23.45 (`std`, `tls12`) + **aws-lc-rs** (non-FIPS) + rustls-platform-verifier with `certs.d` roots

**Why aws-lc-rs over the ring precedent** (§6.2):
- It is the only provider that gives X25519MLKEM768 against registries that offer it; measured at Docker Hub's auth host, GCP, ECR, GHCR's blob host and Docker's CloudFront CDN. That protects long-lived registry passwords and refresh tokens in OAuth POSTs against harvest-now-decrypt-later attacks.
- Parts are formally verified; ring has none.
- A FIPS-validated module is available later by one feature flag.
- It is actively maintained.
- It is rustls's own default.
- The precedent's reason ("ring builds without cmake") no longer holds, and both providers have the same cross-lint cost (§6.1).
- ring's 2020 audit coverage is its one remaining advantage.

**Why the platform verifier** (§6.3):
- It reproduces Go's delegation to the OS on macOS and Windows, and the system bundle on Linux, so corporate CAs work.
- `Verifier::new_with_extra_roots` fed from `certs.d/<host[:port]>/*.crt` reproduces dockerd's per-registry CAs.

**Why `tls12`.** A Docker-operated CDN host still refuses TLS 1.3 (§6.2).

**Cost and risk.**
- The C build: 2 of 8 local cross-lint targets, the same as ring (R3).
- A larger C surface and build time than ring, not yet measured (M2).
- AWS-LC advisories (five in 2026, none on rustls's path).

**Revisit** graviola when it stops asserting on CPUs without AVX2/ADX/BMI2 and has outside review; it is the only provider that lints 8 of 8.

**Record** as a new decision in `docs/design/architecture.md` that diverges from the siblings, with this evidence.

### R3. Confine C to that one crate, and make its cross-lint work

**What.**
- Every other dependency stays pure Rust: sha2, flate2/miniz_oxide, ruzstd, serde_json, httparse and the platform verifier, all 8 of 8 (§6.1).
- Put registry code in its own crate (say `crates/registry`). `crates/image` stays C-free, and hashes DiffIDs with sha2.
- For the provider's 6 failing targets, either:
  - (a) set `CC_<triple>` on dev Macs to `zig cc` (Linux gnu/musl) and to clang-cl with an MSVC sysroot (cargo-xwin) for Windows; or
  - (b) amend CLAUDE.md so those targets lint that crate in CI only. CI already runs clippy natively per target [.github/workflows/ci.yml:35-61, 98-119].

**Decide after M1.**

### R4. HTTP: a small in-repo HTTP/1.1 client on rustls + httparse

**What.**
- Blocking, one connection per in-flight layer, keep-alive to the registry.
- GET, HEAD and form POST. `Content-Length` and chunked bodies, per RFC 9112 §6.3 and §7.1.
- `Range`; ≤ 10 redirects; `Authorization` never forwarded across hosts.
- Connect and read timeouts.
- `HTTPS_PROXY`/`NO_PROXY` with CONNECT.
- No `Accept-Encoding`, but decode a gzip or zstd `Content-Encoding` before hashing, as containerd does (§2).

**Why.**
- The needed subset is small (§6.4). HTTP/1.1 suffices on every probed endpoint.
- ureq's path for any provider other than ring is an unstable API.
- We need exact control of the auth, redirect, Range and streaming semantics that R1 depends on.
- Our panic-free lints cover it, and it has the fewest crates.

**Cost and risk.**
- We own the parser edge cases (RFC 9112 §11), proxies and timeouts.
- Fuzz the response head and the chunked decoder, and E2E-test against Docker Hub, GHCR and a local `registry:3`.

**Fallback.** ureq 3 with `default-features = false, features = ["rustls-no-provider", "platform-verifier"]`, `RedirectAuthHeaders::Never`, and `gzip` off.

### R5. Credentials and tokens exactly as Docker reads them (read-only this milestone)

**What.**
- §3.3 lookup:
  - `$DOCKER_CONFIG` or `~/.docker/config.json`;
  - the `https://index.docker.io/v1/` key;
  - `credHelpers` > `credsStore` > `auths`;
  - `DOCKER_AUTH_CONFIG`.
- The helper protocol, and `<token>` identity tokens.
- §3.1 token flow:
  - anonymous GET;
  - OAuth2 POST, then a GET+Basic fallback;
  - `registrytoken` as a Bearer token;
  - tokens cached per (host, scope), honouring `issued_at`.
- Redact tokens and query strings in logs. `shards login` (writing credentials) comes later.

**Why.** Users' existing `docker login` state must work unchanged. dockerd releases credentials only to their own host [moby resolver.go:69-104].

### R6. Platform: `linux/<guest arch>`, arm64 strict, amd64 by guest CPUID

**What.**
- Normalize, then vector, then stable sort, exactly as containerd/platforms (§4).
- arm64 without `arm/v*` fallbacks, because HVF exposes no AArch32 EL0.
- amd64 `vN…v1` then `386`, with N from the CPUID the VMM gives guests (N = 1 until that policy exists).
- Skip `unknown/unknown`.
- `--platform` overrides.

**Why.**
- An image the guest cannot execute must not be selected.
- containerd's own default ignores amd64 levels, while dockerd uses the host's; for us, the guest's are the ones that matter.

### R7. Decompress and hash in pure Rust; switch backends only on measured evidence

**What.**
- flate2 with miniz_oxide (`forbid(unsafe_code)`) and ruzstd, decompressing layers concurrently.
- Move flate2 to `zlib-rs` (one feature flag, still pure Rust) only if M3 shows inflate bounds pull time.
- SHA-256 via sha2 0.11.
- On `aarch64-pc-windows-msvc`, where sha2 has no runtime detection, decide by M4 between the soft path, a target-feature build (**UNVERIFIED** that all Windows-on-Arm CPUs have the SHA-2 extension), and the provider's digest.

**Why.**
- 8 of 8 cross-lint.
- Streaming is required: libdeflate is whole-buffer only.
- zstd-sys breaks cross-lint for a minority format.
- No primary numbers exist to justify giving up either (§6.5–6.7).

### R8. Be a good Docker Hub citizen

**What.**
- HEAD to resolve, then GET the index and one manifest.
- Never re-GET a manifest we already hold by digest.
- Parse `ratelimit-limit`/`ratelimit-remaining` (including `w=`), `docker-ratelimit-source` and `Retry-After`.
- Back off exponentially on 429 and 503, and show the limit to the user.

**Why.** Pulls count per manifest GET. The window is documented as 6 h but served as 1 h (§3.2). containerd's immediate retry only spends more of it (§2).

## 8. Open questions needing our own measurement

Benchmarks report n, p50, p90, p99 and max, with host, OS and revision (CLAUDE.md). Harnesses go under `docs/research/measurements/`, and results into `platform-measurements.md`.

- **M1. Cross-lint cost of a C provider.**
  - Commit the §6.1 matrix as a harness.
  - Add cells for `CC_<triple>="zig cc -target …"` (Linux gnu and musl) and cargo-xwin/clang-cl (Windows msvc), for aws-lc-sys and ring.
  - Record setup steps, disk use, and clippy wall time per target, cold and warm.
  - Decides R3.
- **M2. Build and binary cost.**
  - Measure the release `shards` binary size and a clean build time (the workspace uses `lto = "fat"`) with aws-lc-rs vs ring, on each native CI runner.
- **M3. Decompression throughput.**
  - Compare miniz_oxide, zlib-rs and ruzstd, with C `gzip -d` and `zstd -d` as references.
  - Inputs: the largest layers of about 20 popular Hub images plus one multi-GB CUDA/PyTorch layer.
  - Measure single-thread MB/s of uncompressed output, at least n = 10 per input, on M-series macOS, x86_64 Linux and Windows arm64.
- **M4. SHA-256 throughput.**
  - Compare sha2 0.11 (and its `soft` backend), ring and aws-lc-rs digests, on 64 KiB streaming updates over 1 GiB, on every host class.
  - Settles `aarch64-pc-windows-msvc` in R7.
- **M5. End-to-end pull.**
  - Compare `shards pull` with `docker pull` and `ctr pull` on small (alpine), medium (python) and large (CUDA) images, with 1…N concurrent layers.
  - Measure wall time, CPU, peak RSS and bytes written. This shows whether network, TLS, inflate, hashing or disk bounds the pipeline.
- **M6. Resume.**
  - Kill pulls at random offsets. Check that the resumed digest is correct, and measure the re-hash cost of the ingest against a restart from zero.
- **M7. Registry drift.**
  - Re-run the Hub and GHCR probes (`/v2/` challenge, token fields, `ratelimit-*` window, redirect hosts and their TLS versions) on a schedule.
  - Today the documented and served rate-limit windows disagree (§3.2).
- **M8. Post-quantum handshakes.**
  - Measure handshake latency for X25519MLKEM768 vs X25519 to auth.docker.io and registry-1.docker.io.
  - Check whether the larger ClientHello passes common corporate TLS-inspection proxies. This needs a test proxy.
- **Q1. Keep the compressed blobs?** (design question, no measurement)
  - Uncompressed-only storage (image-storage R5) loses the registry's blob digests, so `push`/`save` of a pulled image would produce new digests.
  - Keeping the blobs as well costs roughly 1/2.6 of the uncompressed size at the median ratio [image-storage §2.1; calc].
- **UNVERIFIED:**
  - that every Windows-on-Arm CPU implements the ARMv8 SHA-2 instructions;
  - whether AWS-LC detects them at runtime on Windows arm64 (ring does: `src/cpu/arm/windows.rs:15-34`).

## 9. Sources

All were fetched on 2026-09-28 from raw.githubusercontent.com, `gh api`, or static.crates.io, at the tag or commit shown.

### Specifications

- **[dist-spec]** OCI Distribution Specification v1.1.1 (a139cc42): `spec.md`. https://github.com/opencontainers/distribution-spec
- **[image-spec]** OCI Image Format Specification v1.1.1 (147f9c13): `descriptor.md`, `manifest.md`, `image-index.md`, `config.md`, `media-types.md`, `layer.md`, `identity/chainid.go`. https://github.com/opencontainers/image-spec
- **[token auth]** Distribution Registry token authentication, distribution/distribution v3.1.2 (3220848f): `docs/content/spec/auth/{token,oauth,scope,jwt}.md`, `docs/content/spec/api.md`.
- **[RFC 9112]** HTTP/1.1, https://www.rfc-editor.org/rfc/rfc9112.txt, §6.3, §7.1, §11.

### Source code

- **distribution/reference** v0.6.0 (ff14fafe): `regexp.go`, `normalize.go`, `reference.go`, `helpers.go`, `normalize_test.go`, `reference_test.go`.
- **opencontainers/go-digest** v1.0.0 (ea51bea5): `digest.go`, `algorithm.go`.
- **containerd/containerd** v2.4.1 (f2551031):
  - `core/remotes/docker/{resolver,fetcher,httpreadseeker,registry,authorizer,scope}.go`, `auth/{parse,fetch}.go`, `config/hosts.go`;
  - `core/images/{image,handlers,mediatypes}.go`, `core/unpack/unpacker.go`, `core/diff/{stream.go,apply/apply.go}`, `core/content/helpers.go`;
  - `plugins/content/local/writer.go`, `pkg/archive/compression/compression.go`, `go.mod`.
- **containerd/platforms** v1.0.0-rc.5 (94edf533): `platforms.go`, `database.go`, `compare.go`, `defaults*.go`, `cpuinfo*.go`.
- **docker/cli** v29.8.1 (4a63305d):
  - `vendor.mod`, `cli/command/image/pull.go`, `cli/config/config.go`, `cli/config/configfile/file.go`;
  - `cli/config/credentials/{file_store,native_store,default_store*}.go`, `cli/config/types/authconfig.go`, `internal/registry/config.go`.
- **docker/docker-credential-helpers** v0.9.9 (59e7cc08): `client/{client,command}.go`, `credentials/{credentials,error}.go`.
- **moby/moby** docker-v29.8.1 (464cd50c):
  - `daemon/containerd/{resolver,image_pull,platform_matchers}.go`, `daemon/pkg/registry/{registry,config}.go`;
  - `daemon/internal/distribution/pull_v2_unix.go`, `vendor/github.com/tonistiigi/go-archvariant/amd64variant.go`.
- **distribution/distribution** v3.1.2 (3220848f): `registry/storage/blobserver.go`.
- **golang/go** go1.27.1 (862c888e): `src/net/http/client.go`, `src/crypto/x509/{root,root_linux,root_unix,root_darwin,root_windows,verify}.go`.
- **aws/aws-lc** v5.7.0 (02561621), as vendored by aws-lc-sys 0.45.0 (`AWSLC_VERSION_NUMBER_STRING "5.7.0"`): `README.md`, `crypto/fipsmodule/FIPS.md`.
- **briansmith/ring** 0.17.14 (2723abbc): `BUILDING.md`, `README.md`, and the crate's `src/digest/sha2/sha2_32.rs` and `src/cpu/arm/*.rs`.
- **facebook/zstd** v1.5.7 (f8745da6): `README.md`.
- **ebiggers/libdeflate** v1.26 (92e6a0db): `README.md`.
- **trifectatechfoundation/zlib-rs** v0.6.8 (0254e072): `README.md`.

### Crates (crates.io packages; `.cargo_vcs_info.json` commit)

- rustls 0.23.45 (2976d90f);
- aws-lc-rs 1.18.1 (22e629d5), including its `book/src/requirements/*.md` at the same tag; aws-lc-sys 0.45.0 (7943223c);
- rustls-platform-verifier 0.7.1 (252e2516); rustls-native-certs 0.8.4 (9d1f11e5); webpki-roots 1.0.9 (0a553dbc);
- graviola 0.4.1 (7763d0cc); rustls-graviola 0.4.0 (a942c258);
- ureq 3.4.2 (2e9ef24a); hyper 1.11.1 (6371cd42); hyper-util 0.1.21; httparse 1.10.1;
- flate2 1.1.10 (ed93d4fc); miniz_oxide 0.9.1 (4e582392); zlib-rs 0.6.8 (0254e072); libdeflater 1.26.1 (260e1e9a);
- zstd 0.14.0 (648acb47); zstd-sys 2.1.0+zstd.1.5.7; ruzstd 0.9.0 (f833802b);
- sha2 0.11.0 (ffe09398); cpufeatures 0.3.1; serde_json 1.0.151.

### Official documentation

- Docker docs, docker/docs at 87b2b3d1: `content/manuals/docker-hub/usage/{_index,pulls}.md`, `content/manuals/desktop/setup/allow-list.md`.
- RustSec advisory database, rustsec/advisory-db at ef036051: RUSTSEC-2025-0007, -0009, -0010 (ring); RUSTSEC-2026-0044…0048 (aws-lc-sys); RUSTSEC-2026-0285 (rustls); RUSTSEC-2024-0400 (ruzstd); RUSTSEC-2024-0401 (zlib-rs); RUSTSEC-2021-0100 (sha2).

### Security audits

- **[TLS-01]** Cure53, "Security Review & Audit Report rustls," May–June 2020, sponsored by CNCF. Published in the rustls repository as `audit/TLS-01-report.pdf` (v/0.23.45).

### Our own observations (2026-09-28; harnesses in the session scratchpad, to be committed per M1 and M7)

- **Cross-lint matrix (§6.1):** macOS 26.4 arm64, rustc 1.98.0, Apple clang 21.0.0.
- **Registry probes (§2, §3.2):** anonymous curl against registry-1.docker.io and ghcr.io. Headers only, tokens never printed; four manifest GETs counted against the anonymous limit.
- **TLS probe (§6.2):** a rustls 0.23.45 client with ring (TLS 1.3-only and TLS 1.2-only) and aws-lc-rs (defaults), against 17 registry and CDN hosts, with LibreSSL `s_client -tls1_3` cross-checking the Hub CDNs.
- **HTTP/1.1 check (§6.4):** `curl --http1.1` against 8 endpoints.

### Project files

- `docs/research/image-storage.md` (§2.1, §2.5, R1, R5, R6).
- `docs/research/platform-measurements.md:204` (HVF `ID_AA64PFR0_EL1`).
- `resources/kernel/firecracker-{aarch64,x86_64}-6.18.config`.
- `.github/workflows/ci.yml`.
- Sibling precedent: `../slates/Cargo.toml:81-84`, `../focal/crates/*/Cargo.toml`.
