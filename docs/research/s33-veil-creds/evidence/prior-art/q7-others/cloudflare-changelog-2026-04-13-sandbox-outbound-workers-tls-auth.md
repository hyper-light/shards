---
description: Outbound Workers now intercept HTTPS traffic, inject credentials, and enforce dynamic allow/deny policies per sandbox instance.
title: Secure credential injection and dynamic egress policies for Sandboxes
image: https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/og.png?v=e82b6f7bf4958159
---

[Skip to content](#main-content)

[View RSS feeds](https://developers.cloudflare.com/fundamentals/new-features/available-rss-feeds/) [ Subscribe to RSS](https://developers.cloudflare.com/changelog/rss/index.xml)

[Back to all posts](https://developers.cloudflare.com/changelog)

April 13, 2026

## Secure credential injection and dynamic egress policies for Sandboxes

[Containers](https://developers.cloudflare.com/containers/) [Agents](https://developers.cloudflare.com/agents/)

Copy as Markdown| [View as Markdown](https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/index.md)| [Agent setup](https://developers.cloudflare.com/agent-setup/)

Outbound Workers for [Sandboxes](https://developers.cloudflare.com/sandbox/) and [Containers](https://developers.cloudflare.com/containers/) now support zero-trust credential injection, TLS interception, allow/deny lists, and dynamic per-instance egress policies. These features give platforms running agentic workloads full control over what leaves the sandbox, without exposing secrets to untrusted workloads, like user-generated code or coding agents.

#### Credential injection

Because outbound handlers run in the Workers runtime, outside the sandbox, they can hold secrets the sandbox never sees. A sandboxed workload can make a plain request, and credentials are transparently attached before a request is forwarded upstream.

For instance, you could run an agent in a sandbox and ensure that any requests it makes to Github are authenticated. But it will never be able to access the credentials:

```ts
export class MySandbox extends Sandbox {}

MySandbox.outboundByHost = {
	"github.com": (request: Request, env: Env, ctx: OutboundHandlerContext) => {
		const requestWithAuth = new Request(request);
		requestWithAuth.headers.set("x-auth-token", env.SECRET);
		return fetch(requestWithAuth);
	},
};
```

You can easily inject unique credentials for different instances by using `ctx.containerId`:

```ts
MySandbox.outboundByHost = {
	"my-internal-vcs.dev": async (
		request: Request,
		env: Env,
		ctx: OutboundHandlerContext,
	) => {
		const authKey = await env.KEYS.get(ctx.containerId);

		const requestWithAuth = new Request(request);
		requestWithAuth.headers.set("x-auth-token", authKey);
		return fetch(requestWithAuth);
	},
};
```

No token is ever passed into the sandbox. You can rotate secrets in the Worker environment and every request will pick them up immediately.

#### TLS interception

Outbound Workers now intercept HTTPS traffic. A unique ephemeral certificate authority (CA) and private key are created for each sandbox instance. The CA is placed into the sandbox and trusted by default. The ephemeral private key never leaves the container runtime sidecar process and is never shared across instances.

With TLS interception active, outbound Workers can act as a transparent proxy for both HTTP and HTTPS traffic.

#### Allow and deny hosts

Easily filter outbound traffic with `allowedHosts` and `deniedHosts`. When `allowedHosts` is set, it becomes a deny-by-default allowlist. Both properties support glob patterns.

```ts
export class MySandbox extends Sandbox {
	allowedHosts = ["github.com", "npmjs.org"];
}
```

#### Dynamic outbound handlers

Define named outbound handlers then apply or remove them at runtime using `setOutboundHandler()` or `setOutboundByHost()`. This lets you change egress policy for a running sandbox without restarting it.

```ts
export class MySandbox extends Sandbox {}

MySandbox.outboundHandlers = {
	allowHosts: async (req: Request, env: Env, ctx: OutboundHandlerContext ) => {
		const url = new URL(req.url);
		if (ctx.params.allowedHostnames.includes(url.hostname)) {
			return fetch(req);
		}
		return new Response(null, { status: 403 });
	},

	noHttp: async () => {
		return new Response(null, { status: 403 });
	},
};
```

Apply handlers programmatically from your Worker:

```ts
const sandbox = getSandbox(env.Sandbox, userId);

// Open network for setup
await sandbox.setOutboundHandler("allowHosts", {
	allowedHostnames: ["github.com", "npmjs.org"],
});
await sandbox.exec("npm install");

// Lock down after setup
await sandbox.setOutboundHandler("noHttp");
```

Handlers accept `params`, so you can customize behavior per instance without defining separate handler functions.

#### Get started

Upgrade to `@cloudflare/containers@0.3.0` or `@cloudflare/sandbox@0.8.9` to use these features.

For more details, refer to [Sandbox outbound traffic](https://developers.cloudflare.com/sandbox/sdk/guides/outbound-traffic/) and [Container outbound traffic](https://developers.cloudflare.com/containers/configuration/outbound-traffic/).

```json
{"@context":"https://schema.org","@type":"BlogPosting","@id":"https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/#page","headline":"Secure credential injection and dynamic egress policies for Sandboxes","description":"Outbound Workers now intercept HTTPS traffic, inject credentials, and enforce dynamic allow/deny policies per sandbox instance.","url":"https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/","inLanguage":"en","image":"https://developers.cloudflare.com/changelog/post/2026-04-13-sandbox-outbound-workers-tls-auth/og.png?v=e82b6f7bf4958159","dateModified":"2026-04-13","datePublished":"2026-04-13","publisher":{"@type":"Organization","name":"Cloudflare","description":"One platform for your apps, agents, and workforce. Build, secure, and scale without managing infrastructure","url":"https://www.cloudflare.com/","sameAs":["https://github.com/cloudflare","https://www.linkedin.com/company/cloudflare","https://x.com/cloudflare"],"logo":{"@type":"ImageObject","url":"https://developers.cloudflare.com/logo.svg"},"address":{"@type":"PostalAddress","streetAddress":"101 Townsend St","addressLocality":"San Francisco","addressRegion":"CA","postalCode":"94107","addressCountry":"US"},"contactPoint":[{"@type":"ContactPoint","contactType":"Customer Support","url":"https://support.cloudflare.com/","availableLanguage":["English"]},{"@type":"ContactPoint","contactType":"Sales","url":"https://www.cloudflare.com/contact/","availableLanguage":["English"]}]},"isPartOf":{"@type":"WebSite","@id":"https://developers.cloudflare.com/#website","name":"Cloudflare Docs","url":"https://developers.cloudflare.com/"}}
```
