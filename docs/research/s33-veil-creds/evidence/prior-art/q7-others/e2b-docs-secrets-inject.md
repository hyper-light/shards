> ## Documentation Index
> Fetch the complete documentation index at: https://docs.e2b.dev/llms.txt
> Use this file to discover all available pages before exploring further.

# Injecting secrets into requests

> Reference a stored secret in a network rule so the egress proxy adds it to matching outbound requests.

Stored secrets are injected into the sandbox's outbound **HTTPS** requests through [per-host request transforms](https://docs.e2b.dev/network/internet-access.md#per-host-request-transforms). Plain HTTP requests do not receive injected headers.

If your HTTP client uses its own CA bundle, configure it to trust the egress proxy before making requests. See [TLS certificates](https://docs.e2b.dev/network/internet-access.md#tls-certificates) for Python Requests and Node.js settings.

Use `Secret.fill` to build the header value in a network rule's `transform`:

```js JavaScript & TypeScript theme={"theme":"css-variables"}
import { Sandbox, Secret } from 'e2b'

await Secret.create('stripe_api_key', 'sk_live_...')

const sandbox = await Sandbox.create({
  network: {
    // Only allow egress to hosts that have rules registered.
    allowOut: ({ rules }) => [...rules.keys()],
    // Deny all other traffic
    denyOut: ({ allTraffic }) => [allTraffic],
    rules: {
      'api.stripe.com': [
        {
          transform: {
            headers: { Authorization: `Bearer ${Secret.fill('stripe_api_key')}` },
          },
        },
      ],
    },
  },
})
```

```python Python theme={"theme":"css-variables"}
from e2b import Sandbox, Secret

Secret.create("stripe_api_key", "sk_live_...")

sandbox = Sandbox.create(
    network={
        # Only allow egress to hosts that have rules registered.
        "allow_out": lambda ctx: list(ctx.rules.keys()),
        # Deny all other traffic
        "deny_out": lambda ctx: [ctx.all_traffic],
        "rules": {
            "api.stripe.com": [
                {
                    "transform": {
                        "headers": {"Authorization": f"Bearer {Secret.fill('stripe_api_key')}"},
                    },
                },
            ],
        },
    },
)
```

`Secret.fill('stripe_api_key')` is a local formatting helper that returns a reference to the secret without making a network call. Pass the secret's name, not its `sec_...` identifier. The SDK rejects empty names, braces, and control characters, but does not check whether the secret exists or enforce every API naming rule. Always use a valid [secret name](https://docs.e2b.dev/secrets/create.md#secret-names).

The reference is stored in the sandbox's network configuration. Each time the egress proxy forwards a matching HTTPS request, it resolves the secret's current value and injects it outside the sandbox. Always build references with `Secret.fill`, since their format is an internal detail that can change.

The example allows outbound traffic only to the named host. Network rules alone do not grant or restrict access, so configure `allowOut` / `allow_out` and `denyOut` / `deny_out` as shown. Choose trusted hosts because they receive the injected credential.

> [!WARNING]
>
> Injection is keyed on the destination host. Every process in the sandbox that sends an HTTPS request to that host gets the secret, including a browser and any page it loads. If the sandbox runs a browser or untrusted code that shouldn't use the secret, run it in a separate sandbox.

A header value can mix secret markers with static text and [workload identity](https://docs.e2b.dev/iam/workload-identity.md) token placeholders. Each header is substituted atomically: either every marker in it resolves, or the header is omitted from the forwarded request.

## Resolution failures are silent for traffic

Referencing a valid secret name that doesn't exist does **not** fail at sandbox creation. Secret existence is checked only at request time by the egress proxy. Invalid network rules can still cause sandbox creation to fail. When a reference can't be resolved (a misspelled name, a deleted secret, or resolution being temporarily unavailable), the proxy fails open for traffic:

* The request is still forwarded to the destination.
* Every header containing an unresolved reference is omitted in full. A failure of the whole secrets lookup omits all headers containing secret references for that request. Static headers and unrelated headers are unaffected by a secret lookup failure.
* A configured transform replaces any header of the same name sent by sandbox code. If resolution fails, the proxy also removes that sandbox-supplied header.
* The unresolved reference is never forwarded to the destination.

Resolved values must also be valid HTTP header values. A value containing a newline, carriage return, or another forbidden control character causes the entire header to be omitted.

The destination may return an authentication error such as `401` or `403`, depending on how it handles the missing header. E2B does not replace the forwarded response with a secret-resolution error. If authentication starts failing, check that the secret exists in the sandbox's project, that the reference uses its name, and that its value is valid for an HTTP header. `Secret.exists` checks existence without changing the secret, but does not test runtime injection.
