---
title: "Tokenized Tokens"
---

# Tokenized Tokens

![Sort of the Ghostbusters containment unit? Maybe?](https://fly.io/blog/tokenized-tokens/assets/ghosts.png)

[Annie Ruygt](https://annieruygtillustration.com/)

We’re Fly.io. We run apps for our users on hardware we host around the world. Building security for a platform like this is tricky, and that’s what the post is about. But you don’t have to read any of this to get an app running on here. See how to [speedrun getting an app running on Fly.io here](https://fly.io/speedrun/).

We built some little security thingies. We’re open sourcing them, and hoping you like them as much as we do. In a nutshell: it’s a proxy that injects secrets into arbitrary 3rd-party API calls. We could describe it more completely here, but that wouldn’t be as fun as writing a big long essay about how the thingies came to be, so: buckle up.

The problem we confront is as old as Rails itself. Our application started simple: some controllers, some models. The only secrets it stored were bcrypt password hashes. But not unlike a pet baby alligator, it grew up. Now it’s become more unruly than we’d planned.

That’s because frameworks like Rails make it easy to collect secrets: you just create another model for them, [roll some kind of secret to encrypt them](https://guides.rubyonrails.org/active_record_encryption.html), jam that secret into the deployment environment, and call it a day.

And, at least in less sensitive applications, or even the early days of an app like ours, that can work!

For what it’s worth, and to the annoyance of some of our Heroku refugees, we’ve never stored customer app secrets this way; our Rails API can write customer secrets, but has never been able to read them. We’ll talk more about how this works in a sec.

But for us, not anymore. At the stage we’re at, all secrets are hazmat. And Rails itself is the portion of our attack surface we’re least confident about – the rest of it is either outside of our trust boundaries, or written in Rust and Go, strongly-typed memory-safe languages that are easy to reason about, and which have never accidentally treated YAML as an executable file format.

So, a few months back, during an integration with a 3rd party API that relied on OAuth2 tokens, we drew a line: ⚡ *henceforth, hazmat shall only be removed from Rails, never added* ⚡. This is easier said than done, though: despite prominent “this is not a place of honor” signs all over the codebase, our Rails API is still where much of the action in our system takes place.

### How Apps Use Secrets: 3 Different Approaches

![Diagram: Rails reads a stored secret and uses it to call a 3rd party API, all inside the danger zone](https://fly.io/blog/tokenized-tokens/assets/secrets-1.png?2/3&card&center)

We just gave you one way, probably the most common. Stick ‘em in a model, encrypt them with an environment secret, and watch Dependabot religiously for vulnerabilities in transitively-added libraries you’ve never heard of before.

![Diagram: Rails retrieves a secret from Vault and uses it to call a 3rd party API](https://fly.io/blog/tokenized-tokens/assets/secrets-2.png?2/3&card&center)

Here’s a second way, probably the second-most popular: use a secrets management system, like [KMS](https://aws.amazon.com/kms/) or [Vault](https://www.hashicorp.com/products/vault). These systems, which are great, keep secrets encrypted and allow access based on an intricate access control language, which is great.

That’s what we do for customer app secrets, like `DATABASE_URL` and `API_KEY`. We use [HashiCorp Vault](https://www.hashicorp.com/products/vault) (for the time being). Our Rails API has an access token for Vault that allows it to set secrets, but not read any of them back, like a kind of diode. A game-over Rails vulnerability might allow an attacker to scramble secrets, but not to easily dump them.

In the happiest cases with secrets, systems like Vault can keep secret bits from ever touching the application. Customer app secrets are a happy case: Rails never needs to read them, [just our orchestrator](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/), to inject them into VM environments. In other happy cases, Vault operates on the app’s behalf: signing a time-limited request URL for AWS, or making a direct request to a known 3rd-party service. Vault calls these features “[secret engines](https://developer.hashicorp.com/vault/docs/secrets)”, and when you can get away with using them, it’s hard to do better.

The catch is, sometimes you can’t get away with them. For most 3rd parties, Vault has no idea how to interact with them. And most secrets are bearer tokens, not request signatures. The only way to use those kinds of secrets is to read them into app memory. If good code can read a secret from Vault, so can a YAML vulnerability.

Still: this is better than nothing: even if apps can read raw secrets, systems like Vault can provide an audit trail of which secrets were pulled when, and make it much easier to rotate secrets, which you’ll want to do with raw secrets to contain their blast radius. HashiCorp Vault is great, so is KMS, we recommend them unreservedly.

![Diagram: Rails makes an RPC to a small microservice that gets the secret from Vault and calls the 3rd party API](https://fly.io/blog/tokenized-tokens/assets/secrets-3.png?2/3&card&center)

So that’s why there’s a third way to handle this problem, which is: decompose your application into services so that the parts that have to handle secrets are tiny and well-contained. The bulk of our domain-specific business code can chug along in Rails, and the parts that trade bearer tokens with 3rd parties can be built in a couple hundred lines of Go.

This is a good approach, too. It’s just cumbersome, because a big application ends up dealing with lots of different kinds of secrets, making a trusted microservice for each of them is a drag. What you want is to notice some commonality in how 3rd party API secrets are used, and to come up with some possible way of exploiting that.

We thought long and hard on this and came up with:

### Tokenizer: The Fabled 4th Way

![Diagram: Rails sends an encrypted secret over HTTP to a proxy holding the private key, which rewrites the request to the 3rd party](https://fly.io/blog/tokenized-tokens/assets/secrets-4.png?2/3&card&center)

We developed a multipurpose secret-using service called the `Tokenizer`.

`Tokenizer` is a stateless HTTP proxy that holds the private key of a [Curve25519 keypair.](https://pkg.go.dev/golang.org/x/crypto/nacl/box)

When we get a new 3rd party API secret, we encrypt it to `Tokenizer's` public key; we “tokenize” it. Our API server can handle the (encrypted) tokenized secret, but it can’t read or use it directly. Only `Tokenizer` can.

When it comes time to talk to the 3rd party API, Rails does so via `Tokenizer`. Here’s how that works:

1. The API request is proxied, as an ordinary HTTP 1.1 request, through `Tokenizer`.
2. The request carries one or more additional `Proxy-Tokenizer` headers.
3. Each `Proxy-Tokenizer` header carries an encrypted secret and instructions for `Tokenizer` to rewrite the request in some way, usually by injecting the decrypted plaintext into a header.

You can think of `Tokenizer` as a sort of Vault-style “secret engine” that happens to capture virtually everything an app needs secrets for. It can even use decrypted secrets to selectively HMAC parts of requests, for APIs that authenticate with signatures instead of bearer tokens.

Check it out: [it’s not super complicated](https://github.com/superfly/tokenizer).

Now, our goal is to keep Rails from ever touching secret bits. But, hold on: a game-over Rails vulnerability would give attackers an easy way around `Tokenizer`: you’d just proxy requests for a particular secret to a service you ran that collected the plaintext.

To mitigate that, we built the obvious feature: you can lock requests for specific secrets down to a list of allowed hosts or host regexp patterns.

We think this approach to handling secrets is pretty similar to how payment processors tokenize payment card information, hence the name. The advantages are straightforward:

- Secrets are exposed to a much smaller attack surface that doesn’t include Rails.
- Virtually every usage of secrets we’re likely to run across is captured by HTTP proxying, without us needing to write per-service code.
- The tokenizer is a tiny project that’s easy to audit and reason about.
- Every language we work in already has first-class support for running requests through a proxy (something we already do for [SSRF protection](https://github.com/stripe/smokescreen).)

### SSOkenizer: Tokenizing OAuth SSO

When we created `Tokenizer`, we were motivated by the problem of OAuth2 tokens other services providers gave us, for partnership features we build for mutual customers.

We’d also dearly like our customers to use OAuth2/OIDC to log into Fly.io itself; it’s more secure for them, and gives them the full complement of Google MFA features, meaning we don’t immediately have to implement the full complement of Google MFA features. Letting people log into Fly.io with a Google OAuth token means we have to keep track of people’s OAuth tokens. That sounds like a job for the `Tokenizer`!

But there’s a catch: acquiring those OAuth tokens in the first place means doing the OAuth2 dance, which means that for a brief window of time, Rails is handling hazmat. We’d like to close that window.

![Diagram: the SSOkenizer runs the OAuth2 flow between the browser and Google and hands Rails a sealed token, which Rails sends through the proxy to Google](https://fly.io/blog/tokenized-tokens/assets/ssokenizer.png?2/3&card&center)

Enter the `SSOkenizer`.

The job of the `SSOkenizer` is to perform the OAuth2 dance on behalf of Rails, and then use the output of that process (the OAuth2 bearer token yielded from the OAuth2 code flow, which you can [see in its cursed majesty here](https://github.com/superfly/ssokenizer#ssokenizer)) to drive the `Tokenizer`.

In other words, where we’d otherwise explicitly encrypt secrets to be tokenized a-priori, the `SSOkenizer` does that on the fly, passing tokenized OAuth2 credentials back to Rails. Those… tokenized tokens can only be used through the `Tokenizer` proxy, which is the only component in our system with the private key that unseals them.

We think this is a pretty neat trick. The `SSOkenizer` itself is tiny, even smaller than the `Tokenizer` ([you can read it here](https://github.com/superfly/ssokenizer/)), and essentially stateless; in fact, pretty much everything in this system is minimally stateful, except Rails, which is great at being stateful. We even keep almost all of OAuth2 out of Rails and confined to Go code (where it’s practically the hello-world of Go OAuth2 libraries).

A nice side effect-slash-validation of this design: once we got it working for Google, it became a super easy project to get OAuth2 logins working for other providers.

### Feel Free To Poach This

We’re psyched for a bunch of reasons:

- We’ve got a clear path to rolling out SSO logins.
- We can do integrations with third-party services now without infecting Rails with more hazmat secrets.
- We’ve honored the rule of “only removing hazmat from Rails, not adding it”.
- We’ve also cleared a path to getting all the rest of the hazmat Rails has access to tokenized.

These are standalone tools with no real dependencies on Fly.io, so they’re easy for us to open source. Which is what we did: if they sound useful to you, check out the [tokenizer](https://github.com/superfly/tokenizer) and [ssokenizer](https://github.com/superfly/ssokenizer) repositories for instructions on deploying and using these services yourself.
