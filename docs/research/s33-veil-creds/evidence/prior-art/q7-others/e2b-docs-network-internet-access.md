> ## Documentation Index
> Fetch the complete documentation index at: https://docs.e2b.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Internet access

> Every sandbox has outbound internet access by default. Control and restrict it with a simple on/off switch or fine-grained allow and deny lists.

Every sandbox has outbound access to the internet by default. You can control and restrict this access to fit security-sensitive workloads, from a simple on/off switch to fine-grained allow and deny lists.

To expose services running inside the sandbox to the outside world, see [Sandbox public URL](https://docs.e2b.dev/network/public-url.md).

To send the traffic that is allowed through a SOCKS5 proxy you operate, see [Bring your own proxy](https://docs.e2b.dev/network/byop.md).

## Controlling internet access

You can control whether a sandbox has access to the internet by using the `allowInternetAccess` / `allow_internet_access` parameter when creating a sandbox. By default, internet access is enabled, but you can disable it for security-sensitive workloads.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Create sandbox with internet access enabled (default)
const sandbox = await Sandbox.create({ allowInternetAccess: true })

// Create sandbox without internet access
const isolatedSandbox = await Sandbox.create({ allowInternetAccess: false })
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Create sandbox with internet access enabled (default)
sandbox = Sandbox.create(allow_internet_access=True)

# Create sandbox without internet access
isolated_sandbox = Sandbox.create(allow_internet_access=False)
```

When internet access is disabled, the sandbox cannot make outbound network connections, which provides an additional layer of security for sensitive code execution.

> [!NOTE]
>
> Setting `allowInternetAccess` / `allow_internet_access` to a falsy value is equivalent to setting `network.denyOut` / `network.deny_out` to `['0.0.0.0/0']` (denying all traffic).

## Fine-grained network control

For more granular control over network access, you can use the network configuration option to specify allow and deny lists for outbound traffic.

### Allow and deny lists

You can specify IP addresses, CIDR blocks, or domain names that the sandbox is allowed to use:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Deny all traffic except specific IPs
const sandbox = await Sandbox.create({
  network: {
    denyOut: ({ allTraffic }) => [allTraffic], // allTraffic === '0.0.0.0/0'
    allowOut: ['1.1.1.1', '8.8.8.0/24']
  }
})

// Deny specific IPs only
const restrictedSandbox = await Sandbox.create({
  network: {
    denyOut: ['8.8.8.8']
  }
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Deny all traffic except specific IPs
sandbox = Sandbox.create(
    network={
        "deny_out": lambda ctx: [ctx.all_traffic],  # ctx.all_traffic == "0.0.0.0/0"
        "allow_out": ["1.1.1.1", "8.8.8.0/24"]
    }
)

# Deny specific IPs only
restricted_sandbox = Sandbox.create(
    network={
        "deny_out": ["8.8.8.8"]
    }
)
```

> [!NOTE]
>
> The selector callback (`({ allTraffic }) => [allTraffic]` / `lambda ctx: [ctx.all_traffic]`) is the recommended way to express "all traffic" (`0.0.0.0/0`). The `ALL_TRAFFIC` constant remains exported for backward compatibility.

### Domain-based filtering

You can allow traffic to specific domains by specifying hostnames in `allowOut` / `allow_out`. When using domain-based filtering, you must deny all other traffic in `denyOut` / `deny_out`. Domains are not supported in the deny lists.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Allow only traffic to google.com
const sandbox = await Sandbox.create({
  network: {
    allowOut: ['google.com'],
    denyOut: ({ allTraffic }) => [allTraffic]
  }
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Allow only traffic to google.com
sandbox = Sandbox.create(
    network={
        "allow_out": ["google.com"],
        "deny_out": lambda ctx: [ctx.all_traffic]
    }
)
```

> [!NOTE]
>
> When any domain is used, the default nameserver `8.8.8.8` is automatically allowed so that names resolve. To turn this off, see [DNS when you filter by domain](#dns-when-you-filter-by-domain).

You can also use wildcards to allow all subdomains of a domain:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Allow traffic to any subdomain of mydomain.com
const sandbox = await Sandbox.create({
  network: {
    allowOut: ['*.mydomain.com'],
    denyOut: ({ allTraffic }) => [allTraffic]
  }
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Allow traffic to any subdomain of mydomain.com
sandbox = Sandbox.create(
    network={
        "allow_out": ["*.mydomain.com"],
        "deny_out": lambda ctx: [ctx.all_traffic]
    }
)
```

You can combine domain names with IP addresses and CIDR blocks:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Allow traffic to specific domains and IPs
const sandbox = await Sandbox.create({
  network: {
    allowOut: ['api.example.com', '*.github.com', '8.8.8.8'],
    denyOut: ({ allTraffic }) => [allTraffic]
  }
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Allow traffic to specific domains and IPs
sandbox = Sandbox.create(
    network={
        "allow_out": ["api.example.com", "*.github.com", "8.8.8.8"],
        "deny_out": lambda ctx: [ctx.all_traffic]
    }
)
```

> [!NOTE]
>
> Domain-based filtering only works for HTTP traffic on port 80 (via Host header inspection) and TLS traffic on port 443 (via SNI inspection). Traffic on other ports uses CIDR-based filtering only. UDP-based protocols like QUIC/HTTP3 are not supported for domain filtering.

#### Catch-all wildcard

`'*'` matches HTTP `Host` values on port 80 and TLS SNI on port 443. The API requires `0.0.0.0/0` (`ALL_TRAFFIC`) in the deny list, otherwise creation returns HTTP 400.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

const sandbox = await Sandbox.create({
  network: {
    allowOut: ['*'],
    denyOut: ['0.0.0.0/0'],
  },
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

sandbox = Sandbox.create(
    network={
        "allow_out": ["*"],
        "deny_out": ["0.0.0.0/0"],
    }
)
```

Inside this sandbox:

```bash theme={"theme":"css-variables"}
# Allowed: TLS SNI matches the wildcard
curl --max-time 10 https://example.com

# Also allowed: a numeric HTTP Host value matches '*'
curl --max-time 10 http://1.1.1.1

# Blocked: no TLS SNI and no IP allowance
curl --max-time 10 https://1.1.1.1
```

Other ports require an IP/CIDR allowance. Traffic to the allowed DNS servers (`8.8.8.8` by default) also stays open, see [DNS when you filter by domain](#dns-when-you-filter-by-domain).

For proxy routing and IP-literal policies, see [Using a wildcard with BYOP](https://docs.e2b.dev/network/byop.md#using-a-wildcard-with-byop).

> [!WARNING]
>
> On shared infrastructure such as CDNs and object storage, allowing one hostname can also permit access to other destinations on the same endpoint. For stricter isolation, use dedicated endpoints or enforce destination restrictions through [your own proxy](https://docs.e2b.dev/network/byop.md), and scope credentials and validate destinations in your application.

### DNS when you filter by domain

When `allowOut` / `allow_out` contains a domain, E2B also allows traffic to `8.8.8.8`, Google's public resolver, so programs in the sandbox can look up names. The sandbox's `/etc/resolv.conf` points at it. This covers **all traffic to `8.8.8.8`**, not only DNS. It isn't listed in `allowOut`, and adding `8.8.8.8` to `denyOut` doesn't remove it.

You can choose which DNS servers are allowed with `dnsServers` / `dns_servers`:

* **Leave it out** to allow `8.8.8.8`.
* **Pass a list of up to 10 IPv4 addresses** to allow those servers instead of `8.8.8.8`.
* **Pass an empty list** to allow no DNS server at all.

Like `8.8.8.8`, every server you list gets all traffic allowed, not only DNS. Without a domain in `allowOut`, `dnsServers` has no effect.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

const sandbox = await Sandbox.create({
  network: {
    allowOut: ['api.example.com'],
    denyOut: ({ allTraffic }) => [allTraffic],
    dnsServers: [], // allow no DNS server
  },
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

sandbox = Sandbox.create(
    network={
        "allow_out": ["api.example.com"],
        "deny_out": lambda ctx: [ctx.all_traffic],
        "dns_servers": [],  # allow no DNS server
    }
)
```

You can change it on a running sandbox with `updateNetwork` / `update_network`. Like the other network fields, leaving it out of an update resets it to the default (`8.8.8.8`), so pass it on every update to keep a custom list. When you've set a list, `getInfo()` / `get_info()` shows it in the sandbox's network info. When it's missing there, the default applies.

#### Using a different DNS server

`dnsServers` only opens the firewall. It doesn't change the sandbox's `/etc/resolv.conf`, which still points at `8.8.8.8`. To use another server, allow it and point `/etc/resolv.conf` at it:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

const sandbox = await Sandbox.create({
  network: {
    allowOut: ['api.example.com'],
    denyOut: ({ allTraffic }) => [allTraffic],
    dnsServers: ['203.0.113.53'], // your DNS server's IP
  },
})

await sandbox.commands.run("echo 'nameserver 203.0.113.53' > /etc/resolv.conf", {
  user: 'root',
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

sandbox = Sandbox.create(
    network={
        "allow_out": ["api.example.com"],
        "deny_out": lambda ctx: [ctx.all_traffic],
        "dns_servers": ["203.0.113.53"],  # your DNS server's IP
    }
)

sandbox.commands.run("echo 'nameserver 203.0.113.53' > /etc/resolv.conf", user="root")
```

Since all traffic to a listed server is allowed, a public resolver such as `1.1.1.1` opens the same kind of gap as `8.8.8.8`. A resolver you run yourself is the safer choice.

#### Resolving names with no DNS server

Allowed domains keep working with `dnsServers: []` / `dns_servers=[]`. E2B matches HTTP and HTTPS traffic by the `Host` header or TLS SNI and connects to the domain for you, so the sandbox only needs an address to send the request to. Programs in the sandbox still have to look the name up, so pin the names you need in `/etc/hosts`. Any public IP address works, because E2B routes the request by name. Don't use loopback or private addresses.

For example, to pin `api.example.com` and make all other lookups fail right away:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
// Any public IP works here, E2B routes the request by name
await sandbox.commands.run(
  "echo '1.1.1.1 api.example.com' >> /etc/hosts && : > /etc/resolv.conf",
  { user: 'root' }
)
```

```python Python theme={"theme":"css-variables"}
# Any public IP works here, E2B routes the request by name
sandbox.commands.run(
    "echo '1.1.1.1 api.example.com' >> /etc/hosts && : > /etc/resolv.conf",
    user="root",
)
```

> [!NOTE]
>
> When no DNS server in `/etc/resolv.conf` is reachable, clear the file as above. Otherwise lookups wait on the blocked `8.8.8.8` and take 20 to 40 seconds to fail.

SDK features such as running commands and working with files don't use DNS inside the sandbox, so they keep working with no DNS server allowed.

### Behavior of blocked TCP connections

Due to firewall design, blocked connections may appear successful from inside the sandbox.

The firewall has to accept the connection first before it can decide whether the destination is allowed. This means that, from inside the sandbox, a TCP connection can succeed and report the socket as open even when the destination is denied - no packets actually reach the destination.

To verify that traffic is reaching its destination, check for an application-level response (e.g. an HTTP status code, a TLS handshake, or expected protocol bytes) rather than relying on the TCP connection succeeding.

This is a limitation of how outbound traffic is currently routed from the sandbox to our firewall and may change in the future.

### Priority rules

When both allow and deny rules are specified, **allow rules always take precedence** over deny rules. This means if an IP address is in both lists, it will be allowed.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

// Even though all traffic is denied, 1.1.1.1 and 8.8.8.8 are explicitly allowed
const sandbox = await Sandbox.create({
  network: {
    denyOut: ({ allTraffic }) => [allTraffic],
    allowOut: ['1.1.1.1', '8.8.8.8']
  }
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

# Even though all traffic is denied, 1.1.1.1 and 8.8.8.8 are explicitly allowed
sandbox = Sandbox.create(
    network={
        "deny_out": lambda ctx: [ctx.all_traffic],
        "allow_out": ["1.1.1.1", "8.8.8.8"]
    }
)
```

### Per-host request transforms

> [!NOTE]
>
> Per-host request transforms are currently in public beta. You can start using them right away, no need to request access.
>
> Please keep in mind that this feature is still in active development, and the features and ways of interacting with it might change during this period.
>
> If you have any questions or feedback, please [reach out to us](https://console.e2b.dev/?support=true). Any input is greatly appreciated.

You can register per-host rules under `network.rules` to apply transforms (for example, inject HTTP headers) on outbound requests matching a host. Rules are keyed by host and registering one does **not** grant egress on its own — the host must still be referenced via `allowOut` / `allow_out`.

The `transform.headers` object is sent on the wire as-is and injected by the egress proxy on matching HTTPS requests. Transforms apply to intercepted TLS traffic only, so nothing is injected on plain HTTP.

Header values can also reference stored [secrets](https://docs.e2b.dev/secrets.md) with `Secret.fill`. The egress proxy replaces each reference with the secret's current value outside the sandbox when it forwards a matching HTTPS request.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

await Sandbox.create({
  network: {
    // Only allow egress to hosts that have rules registered.
    allowOut: ({ rules }) => [...rules.keys()],
    // Deny all other traffic
    denyOut: ({ allTraffic }) => [allTraffic],
    // Register per-host rules
    rules: {
      'api.example.com': [
        {
          transform: {
            headers: { 'X-Header': 'Content' },
          },
        },
      ],
    },
  },
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

sandbox = Sandbox.create(
    network={
        # Only allow egress to hosts that have rules registered.
        "allow_out": lambda ctx: list(ctx.rules.keys()),
        # Deny all other traffic
        "deny_out": lambda ctx: [ctx.all_traffic],
        # Register per-host rules
        "rules": {
            "api.example.com": [
                {
                    "transform": {
                        "headers": {"X-Header": "Content"},
                    },
                },
            ],
        },
    },
)
```

In JavaScript, `network.rules` accepts either a plain object or a `Map`:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
const rules = new Map([
  ['api.example.com', [{ transform: { headers: { 'X-Trace': 'on' } } }]],
])

await Sandbox.create({
  network: {
    allowOut: ({ rules }) => [...rules.keys()],
    denyOut: ({ allTraffic }) => [allTraffic],
    rules,
  },
})
```

#### TLS certificates

For HTTPS requests to hosts with transform rules, the egress proxy presents a certificate signed by E2B's certificate authority (CA). E2B adds this CA to the sandbox's system bundle at `/etc/ssl/certs/ca-certificates.crt`.

Clients that use their own CA bundle, such as Python Requests and Node.js, may fail certificate verification even though the system bundle trusts the proxy. If your client does not use the system bundle, configure it inside the sandbox before starting your application:

```bash Python Requests theme={"theme":"css-variables"}
export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
python app.py
```

```bash Node.js theme={"theme":"css-variables"}
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
node app.js
```

Replace `app.py` or `app.js` with your application entry point. Node.js reads `NODE_EXTRA_CA_CERTS` only when the process starts, so restart an existing process after setting it.

These settings cover the clients' standard trust configuration. If your application supplies an explicit CA bundle or bypasses environment settings, configure its trust store directly. See the [Requests certificate verification documentation](https://requests.readthedocs.io/en/latest/user/advanced/#ssl-cert-verification) and [Node.js CA configuration](https://nodejs.org/api/cli.html#node_extra_ca_certsfile).

#### Matching hosts

A rule key is either an exact DNS name (`api.example.com`) or a single leading wildcard label (`*.example.com`).

* One rule can transform requests to subdomains at any depth: `*.example.com` covers `api.example.com` and `a.b.example.com`.
* The apex is separate: `*.example.com` does not match `example.com`. Register the apex as its own key if you need to transform it too.
* Exact keys take precedence over wildcard keys.
* When several wildcards match, the most specific one wins, meaning the longest matching suffix.
* Exactly one rule applies per request. Matching rule sets are never merged, so a request only ever receives one rule's headers.
* Keys are stored lowercased, and two keys that differ only in case are rejected.
* A rule still requires matching `allowOut` / `allow_out` access, where wildcards work the same way.

These keys are rejected with `400: Rule domain "..." is not a valid domain name`:

* A bare `*`.
* A leading dot, such as `.example.com`.
* A host with a port, such as `example.com:443`.
* More than one wildcard label, such as `*.*.example.com` or `api.*.example.com`.

> [!WARNING]
>
> Keep wildcard scopes narrow. A broad key such as `*.com` is accepted and matches every subdomain beneath it, so a credential you inject is sent to every matching TLS destination the sandbox contacts.

#### Limits

| Limit | Value |
| - | - |
| Domains per sandbox | 10 |
| Transform rules per domain | 1 |
| Headers per rule | 20 |
| Rule domain length | 128 characters |
| Header name length | 64 characters |
| Header value length | 2048 characters |

* A wildcard key counts as one domain, whatever it matches.
* Exceeding any limit fails the create or update call with a `400` naming the limit.
* These values are fixed and cannot be raised for an individual account.

#### Transform callbacks

A rule's `transform` can also be a callback instead of a static object. The callback receives a context with the sandbox's [workload identity](https://docs.e2b.dev/iam/workload-identity.md) tokens as `iam.tokens.<name>`, letting you inject a workload token into requests to a host:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox, Secret } from 'e2b'

await Sandbox.create({
  iam: {
    tokens: {
      aws: Secret.iamToken({ audience: 'sts.amazonaws.com', tokenType: 'JWT-SVID' }),
    },
  },
  network: {
    allowOut: ({ rules }) => [...rules.keys()],
    denyOut: ({ allTraffic }) => [allTraffic],
    rules: {
      'api.internal.example.com': [
        {
          transform: ({ iam }) => ({
            headers: { Authorization: `Bearer ${iam.tokens.aws}` },
          }),
        },
      ],
    },
  },
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox, Secret

sandbox = Sandbox.create(
    iam={
        "tokens": {
            "aws": Secret.iam_token(audience="sts.amazonaws.com", token_type="JWT-SVID"),
        },
    },
    network={
        "allow_out": lambda ctx: list(ctx.rules.keys()),
        "deny_out": lambda ctx: [ctx.all_traffic],
        "rules": {
            "api.internal.example.com": [
                {
                    "transform": lambda ctx: {
                        "headers": {"Authorization": f"Bearer {ctx.iam.tokens['aws']}"},
                    },
                },
            ],
        },
    },
)
```

The value read from `iam.tokens.<name>` is a placeholder string (`${e2b.identity.tokens.<name>}`) sent on the wire as-is. The egress proxy replaces it with a freshly minted token on each matching request, so the token value never passes through your code or the sandbox. Referencing a name that isn't registered in the sandbox's `iam.tokens` fails at creation with `InvalidArgumentError` (JavaScript) / `InvalidArgumentException` (Python).

The callback must be synchronous and return a plain transform object. A promise (from an `async` callback) or any other return value is rejected with an error. Static `transform` objects keep working unchanged, including hand-written `${e2b.identity.tokens.<name>}` strings.

See [Workload identity](https://docs.e2b.dev/iam/workload-identity.md) for how to register tokens, and [Secrets](https://docs.e2b.dev/secrets.md) for injecting stored secrets the same way.

#### Proxy CA certificate after resume

To modify HTTPS requests, the egress proxy terminates TLS with its own CA. E2B installs that CA into the sandbox trust store at `/etc/ssl/certs/ca-certificates.crt` when the sandbox starts and again on every resume.

* The CA is currently generated on each node, so its private key never has to be sent across the cluster. As a result, the CA can change when a sandbox resumes on a different node, or after the proxy on the same node restarts.
* When the CA changes, the new one is added to the bundle first and the previous one is removed. When it hasn't changed, the bundle is left untouched.
* Clients that read the bundle file when they open a connection keep working. Clients that load it once at startup, or pin the certificate, reject the new CA until they reload the bundle or restart.

> [!WARNING]
>
> Point clients at `/etc/ssl/certs/ca-certificates.crt` instead of copying or pinning the E2B CA. For long-running processes that cache certificates, reload them after the sandbox resumes.

### Updating network settings on a running sandbox

You can update the network configuration of an already running sandbox using `updateNetwork` (JavaScript) or `update_network` (Python). This replaces the current egress rules with the provided configuration without restarting the sandbox.

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox } from 'e2b'

const sandbox = await Sandbox.create()

// Tighten egress on the running sandbox: block 8.8.8.8
await sandbox.updateNetwork({
  denyOut: ['8.8.8.8'],
})

// Replace with an allow-list only
await sandbox.updateNetwork({
  denyOut: ({ allTraffic }) => [allTraffic],
  allowOut: ['api.example.com'],
})

// Toggle internet access without recreating the sandbox
await sandbox.updateNetwork({ allowInternetAccess: false })
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox

sandbox = Sandbox.create()

# Tighten egress on the running sandbox: block 8.8.8.8
sandbox.update_network({"deny_out": ["8.8.8.8"]})

# Replace with an allow-list only
sandbox.update_network({
    "deny_out": lambda ctx: [ctx.all_traffic],
    "allow_out": ["api.example.com"],
})

# Toggle internet access without recreating the sandbox
sandbox.update_network({"allow_internet_access": False})
```

> [!NOTE]
>
> `updateNetwork` / `update_network` **replaces** the current egress configuration — it does not merge with the existing rules. Calling it with an empty object (`updateNetwork({})` / `update_network({})`) clears all allow and deny rules set at create time. The same goes for `dnsServers` / `dns_servers`: leave it out and it goes back to the default, `8.8.8.8`.

Per-host rules in `network.rules` can be changed on a running sandbox, and are replaced rather than merged like the rest of the egress configuration. An update that omits `rules` therefore clears every rule.

Create-only options such as `allowPublicTraffic` / `allow_public_traffic`, `maskRequestHost` / `mask_request_host`, and `httpsPorts` / `https_ports` cannot be changed after the sandbox is created.
