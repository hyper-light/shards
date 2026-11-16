//! Codex's sign-in through the injector (D242 step 8), as its pinned build makes it (rust-v0.167.2,
//! commit ce21072c9; paths under codex-rs, read and never run): `codex login --device-auth` asks
//! `auth.openai.com` for a user code, polls it for the code the person's sign-in in a browser
//! gives, and exchanges that code at its token endpoint in form encoding; it refreshes in JSON; and
//! `codex logout`, as `codex login` does before it signs in again, revokes its refresh token. Each
//! over HTTP/1.1, to a local token host whose certificate the test root signs, which the injector's
//! host side trusts.

use super::*;

const AUTH_HOST: &str = "auth.openai.com";
const CHATGPT_HOST: &str = "chatgpt.com";
/// Codex's OAuth client (login/src/auth/manager.rs:1863).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// An ID token shaped as OpenAI's, none real: a header, a payload of claims Codex reads, and a
/// signature.
const ID_HEADER: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9";
const ID_PAYLOAD: &str = "eyJlbWFpbCI6ImFAZXhhbXBsZS50ZXN0IiwiZXhwIjoxOTAwMDAwMDAwfQ";
const ID_SIGNATURE: &str = "c2lnbmF0dXJlLW5vdC1yZWFs";

/// Codex's sign-in, named as kiln names it (`codex/<person>`), its access token granted toward
/// ChatGPT's backend alone, as kiln grants it (its `Provider::toward`); its provider's token
/// response as Codex reads it, which names no `expires_in`.
const CODEX_ACCOUNT: Account = Account {
    cli: Cli::Codex,
    sign_in: "codex/acct-1",
    api_host: CHATGPT_HOST,
    response: issued,
    expires_in: None,
};

/// A token response as Codex reads one (server.rs:701-707's `ExchangedTokens`,
/// auth/manager.rs:1855-1860's `RefreshResponse`): its ID token `id`, its access token and its
/// refresh token.
fn tokens(id: &str, access: &str, refresh: &str) -> String {
    format!(r#"{{"id_token":"{id}","access_token":"{access}","refresh_token":"{refresh}"}}"#)
}

/// OpenAI's token response for `access` and `refresh`, its ID token signed.
fn issued(access: &str, refresh: &str) -> String {
    tokens(
        &format!("{ID_HEADER}.{ID_PAYLOAD}.{ID_SIGNATURE}"),
        access,
        refresh,
    )
}

/// A token response as the guest is given it: the broker's placeholders, and the ID token's header
/// and payload as they came, its signature gone.
fn placeheld() -> String {
    tokens(
        &format!("{ID_HEADER}.{ID_PAYLOAD}.shards-placeholder"),
        SIGNED_ACCESS,
        SIGNED_REFRESH,
    )
}

/// Codex's refresh of its refresh placeholder (oauth/client.rs:97-113, 124-135: JSON, a sorted
/// map's keys), its reason a 401's (auth/manager.rs:2168-2171): in the microVM its access token
/// is a placeholder and no JWT, so it refreshes on a 401 or on its schedule, never before an expiry
/// (3528-3546).
fn refresh_json() -> String {
    format!(
        r#"{{"client_id":"{CLIENT_ID}","grant_type":"refresh_token","refresh_reason":"unauthorized_recovery","refresh_token":"{SIGNED_REFRESH}"}}"#
    )
}

/// Codex's revocation of its refresh placeholder, which it holds (auth/revoke.rs:47-53, 68-77,
/// 97-117).
fn revoke_json() -> String {
    format!(r#"{{"token":"{SIGNED_REFRESH}","token_type_hint":"refresh_token","client_id":"{CLIENT_ID}"}}"#)
}

/// Codex's token host, speaking HTTP/1.1 and answering what a test writes (`Bench::answer`).
fn auth_host(root: &CertifiedIssuer<'static, KeyPair>) -> H1Host {
    H1Host::new(leaf_config(root, &[AUTH_HOST]), H1Answers::Quiet, false)
}

/// ChatGPT's backend, where Codex's access token goes: an HTTP/1.1 echo.
fn chatgpt_host(root: &CertifiedIssuer<'static, KeyPair>) -> H1Host {
    H1Host::new(leaf_config(root, &[CHATGPT_HOST]), H1Answers::Echo, false)
}

/// The token host's answer, `status` and a JSON `body`, its Content-Length its last field, where
/// the injector writes a whole response's.
fn reply(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `request` written on `bench`, until its host has had it whole. Expect: had as the injector sends
/// a token host a request, asking `identity` alone (`filled`'s edit, its `gzip, br` cut), every
/// other byte as it came.
fn sent(bench: &mut Bench, credentials: &mut Credentials, broker: &mut Broker, request: &[u8]) {
    let n = bench.h1().had.len();
    bench.guest.writer().write_all(request).unwrap();
    bench.until(credentials, &mut |c| broker.serve(c), |b| b.h1().had.len() > n);
    assert_eq!(
        String::from_utf8_lossy(&bench.h1().had[n].request),
        String::from_utf8_lossy(&filled(request, b"Accept-Encoding: gzip, br\r\n", b""))
    );
}

/// `request` on `cli` (`sent`), its host answering `status` with `body`, which names no token.
/// Expect: the answer read whole for tokens and reaching the guest as it came.
fn passes(
    cli: &mut Bench,
    credentials: &mut Credentials,
    broker: &mut Broker,
    request: &[u8],
    status: &str,
    body: &str,
) {
    sent(cli, credentials, broker, request);
    let answer = reply(status, body);
    cli.answer(&answer);
    cli.until(credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
    assert_eq!(
        String::from_utf8_lossy(&cli.got),
        String::from_utf8_lossy(&answer)
    );
    cli.got.clear();
}

/// Signs Codex in on `cli` as `codex login --device-auth` does (device_code_auth.rs): its user
/// code asked (62-97, 165-180); its token polled (99-147), answered 403 while the person has yet to
/// sign in (131-140, RFC 8628 §3.5's word; Codex reads the status alone), then with the code their
/// sign-in gave; that code exchanged at its token endpoint in form encoding (182-217;
/// oauth/client.rs:79-95, 124-131; server.rs:714-722, 805-829), the token host issuing `access` and
/// `refresh`. Expect: each request reaching the host (`sent`); each answer that names no token
/// reaching the guest as it came (`passes`); the exchange's answer held until the broker keeps its
/// tokens, then reaching the guest with the broker's placeholders and its ID token inert, no real
/// token in it.
fn device_sign_in(
    cli: &mut Bench,
    credentials: &mut Credentials,
    broker: &mut Broker,
    access: &str,
    refresh: &str,
) {
    let usercode = post(
        "/api/accounts/deviceauth/usercode",
        AUTH_HOST,
        &format!(r#"{{"client_id":"{CLIENT_ID}"}}"#),
    );
    let device = r#"{"device_auth_id":"deviceauth-not-real","user_code":"ABCD-1234","interval":"5"}"#;
    passes(cli, credentials, broker, &usercode, "200 OK", device);
    let poll = post(
        "/api/accounts/deviceauth/token",
        AUTH_HOST,
        r#"{"device_auth_id":"deviceauth-not-real","user_code":"ABCD-1234"}"#,
    );
    let pending = r#"{"error":"authorization_pending"}"#;
    passes(cli, credentials, broker, &poll, "403 Forbidden", pending);
    let code = r#"{"authorization_code":"code-not-real","code_challenge":"challenge-not-real","code_verifier":"verifier-not-real"}"#;
    passes(cli, credentials, broker, &poll, "200 OK", code);
    let form = format!(
        "grant_type=authorization_code&client_id={CLIENT_ID}&code=code-not-real&redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback&code_verifier=verifier-not-real"
    );
    let exchange = format!(
        "POST /oauth/token HTTP/1.1\r\nHost: {AUTH_HOST}\r\nContent-Type: application/x-www-form-urlencoded\r\nAccept-Encoding: gzip, br\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    );
    sent(cli, credentials, broker, exchange.as_bytes());
    cli.answer(&reply("200 OK", &issued(access, refresh)));
    cli.until(credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
    assert_eq!(broker.access, access.as_bytes(), "the issued token was not kept");
    assert_eq!(broker.refresh, refresh.as_bytes());
    let answer = String::from_utf8(cli.got.clone()).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.ends_with(&placeheld()), "{answer}");
    for real in [access, refresh, ID_SIGNATURE] {
        assert!(
            !holds(&cli.got, real.as_bytes()),
            "a real token reached the guest"
        );
    }
    cli.got.clear();
}

/// `revoke`, the VM's revocation of its sign-in's refresh token, on `bench`. Expect: answered 200
/// here (`signin::REVOKED`, RFC 7009 §2.2), nothing reaching the host; the run host told; the kept
/// tokens as they were.
fn revoked_here(bench: &mut Bench, credentials: &mut Credentials, broker: &mut Broker, revoke: &[u8]) {
    let kept = (broker.access.clone(), broker.refresh.clone());
    let (heads, revoked) = (bench.h1().heads.len(), broker.revoked);
    bench.guest.writer().write_all(revoke).unwrap();
    bench.until(credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
    bench.settle(credentials, &mut |c| broker.serve(c));
    assert_eq!(
        String::from_utf8_lossy(&bench.got),
        String::from_utf8_lossy(signin::REVOKED)
    );
    assert_eq!(bench.h1().heads.len(), heads, "the revocation reached the host");
    assert_eq!(broker.revoked, revoked + 1, "its run host was not told");
    assert_eq!(
        (broker.access.clone(), broker.refresh.clone()),
        kept,
        "the kept tokens were revoked"
    );
    bench.got.clear();
}

/// A request of Codex's to ChatGPT's backend, for its Responses (`https://chatgpt.com/backend-api/codex`,
/// kiln's docs/research/s20-codex-agentfile), its access placeholder its bearer token, on a
/// connection of its own, its lease answered by `broker`. Expect: it reaching the host with `kept`
/// where its placeholder stood, asking `identity`; the token's echo never reaching the guest. The
/// bench, for its audit.
fn filled_toward_chatgpt(
    root: &CertifiedIssuer<'static, KeyPair>,
    credentials: &mut Credentials,
    broker: &mut Broker,
    kept: &str,
) -> Bench {
    let ca = credentials.ca.der().to_vec();
    let mut api = Bench::new(guest(ca.into(), CHATGPT_HOST, None), chatgpt_host(root));
    let body = r#"{"input":[]}"#;
    let request = format!(
        "POST /backend-api/codex/responses HTTP/1.1\r\nHost: {CHATGPT_HOST}\r\nAuthorization: Bearer {SIGNED_ACCESS}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    api.guest.writer().write_all(request.as_bytes()).unwrap();
    api.until(credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
    assert_eq!(
        String::from_utf8_lossy(&api.h1().had[0].request),
        String::from_utf8_lossy(&filled(
            request.as_bytes(),
            SIGNED_ACCESS.as_bytes(),
            kept.as_bytes()
        ))
    );
    assert!(
        !holds(&api.got, kept.as_bytes()),
        "the token's echo reached the guest"
    );
    api
}

/// What `bench`'s flow did, in order, each audited by the sign-in's name and never with a token or
/// a placeholder.
fn audited(bench: &Bench, did: &[Did]) {
    assert_eq!(bench.audit.iter().map(|a| a.what).collect::<Vec<_>>(), did);
    for record in &bench.audit {
        let said = record.to_string();
        assert!(said.contains(CODEX_ACCOUNT.sign_in), "{said}");
        for secret in [
            ISSUED_ACCESS,
            ISSUED_REFRESH,
            ROTATED_ACCESS,
            ROTATED_REFRESH,
            SIGNED_ACCESS,
            SIGNED_REFRESH,
            ID_SIGNATURE,
        ] {
            assert!(!said.contains(secret), "{said}");
        }
    }
}

/// Do: sign Codex in through the injector by its device flow (`device_sign_in`); send ChatGPT's
/// backend a request with the access placeholder the guest was given; then ask its JSON refresh
/// from two connections at once, before either is answered. Expect: the device flow as
/// `device_sign_in` expects, its tokens kept outside; the request filled with the kept token; one
/// ask of the run host and one call to the provider, neither refresh reaching the host, each
/// answered with the sign-in's placeholders and its ID token inert, kept from caches, no rotated
/// token in it; the held lease of the access token let go, its value rotated; each step audited by
/// the sign-in's name, no token or placeholder in it.
#[test]
fn a_device_sign_in_is_taken_outside_and_its_json_refreshes_answered_there_once() {
    let root = root("shards test root");
    let mut credentials = credentials_of(&root, &CODEX_ACCOUNT, false);
    let ca = credentials.ca.der().to_vec();
    let mut broker = Broker {
        account: CODEX_ACCOUNT,
        ..Broker::default()
    };
    let mut cli = Bench::new(guest(ca.clone().into(), AUTH_HOST, None), auth_host(&root));
    device_sign_in(
        &mut cli,
        &mut credentials,
        &mut broker,
        ISSUED_ACCESS,
        ISSUED_REFRESH,
    );
    let api = filled_toward_chatgpt(&root, &mut credentials, &mut broker, ISSUED_ACCESS);
    assert!(
        credentials
            .leases
            .held(&CODEX_ACCOUNT.access(), Instant::now())
            .is_some()
    );
    let refresh = post("/oauth/token", AUTH_HOST, &refresh_json());
    let heads = cli.h1().heads.len();
    let mut two = Bench::new(guest(ca.into(), AUTH_HOST, None), auth_host(&root));
    two.waiter = 8;
    // Each flow's refresh asked and waited on, unanswered: nothing moves after it.
    for (bench, flows) in [(&mut cli, 1), (&mut two, 2)] {
        bench.guest.writer().write_all(&refresh).unwrap();
        bench.settle(&mut credentials, &mut unanswered);
        assert_eq!(credentials.sign_ins.waiting(), flows, "the refresh was not asked");
    }
    assert_eq!(broker.serve(&mut credentials), 1, "two asks of one refresh");
    // The one answer resumes both flows.
    let _ = credentials.sign_ins.resumed();
    for bench in [&mut cli, &mut two] {
        bench.resume(&mut credentials);
        bench.until(&mut credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
        let answer = String::from_utf8(bench.got.clone()).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("Cache-Control: no-store\r\n"), "{answer}");
        assert!(answer.ends_with(&placeheld()), "{answer}");
        for real in [
            ISSUED_ACCESS,
            ISSUED_REFRESH,
            ROTATED_ACCESS,
            ROTATED_REFRESH,
            ID_SIGNATURE,
        ] {
            assert!(
                !holds(&bench.got, real.as_bytes()),
                "a real token reached the guest"
            );
        }
    }
    assert_eq!(broker.provider_calls, 1);
    assert_eq!(broker.refresh, ROTATED_REFRESH.as_bytes());
    assert_eq!(cli.h1().heads.len(), heads, "a refresh reached the host");
    assert!(two.h1().heads.is_empty(), "a refresh reached the host");
    assert!(
        credentials
            .leases
            .held(&CODEX_ACCOUNT.access(), Instant::now())
            .is_none(),
        "the rotated token's old value is held still"
    );
    audited(&cli, &[Did::Captured, Did::Refreshed]);
    audited(&two, &[Did::Refreshed]);
    audited(&api, &[Did::Filled]);
}

/// Do: a run whose Codex signed in before (its placeholders staged, its kept tokens rotated since);
/// `codex login --device-auth` again, which revokes its refresh token before it signs in
/// (cli/src/login.rs:127-141, 372-393), then signs in by its device flow (`device_sign_in`); then,
/// on a connection of its own, `codex logout`, which revokes it again (cli/src/login.rs:566-610;
/// auth/manager.rs:1062-1087); then a request to ChatGPT's backend with the access placeholder.
/// Expect: each revocation answered here, never reaching the host, the run host told and the kept
/// tokens as they were (`revoked_here`); the new sign-in's tokens kept outside, its CLI given
/// placeholders; nothing asked of the provider outside; the request filled with the token the new
/// sign-in left kept, so the host's sign-in outlives the VM's logout; each step audited by the
/// sign-in's name, no token or placeholder in it.
#[test]
fn a_relogin_and_a_logout_revoke_outside_and_the_hosts_sign_in_survives() {
    let root = root("shards test root");
    let mut credentials = credentials_of(&root, &CODEX_ACCOUNT, true);
    let ca = credentials.ca.der().to_vec();
    let mut broker = Broker {
        account: CODEX_ACCOUNT,
        ..Broker::holding(ROTATED_ACCESS, ROTATED_REFRESH)
    };
    let revoke = post("/oauth/revoke", AUTH_HOST, &revoke_json());
    let mut login = Bench::new(guest(ca.clone().into(), AUTH_HOST, None), auth_host(&root));
    revoked_here(&mut login, &mut credentials, &mut broker, &revoke);
    device_sign_in(
        &mut login,
        &mut credentials,
        &mut broker,
        ISSUED_ACCESS,
        ISSUED_REFRESH,
    );
    let mut logout = Bench::new(guest(ca.into(), AUTH_HOST, None), auth_host(&root));
    revoked_here(&mut logout, &mut credentials, &mut broker, &revoke);
    assert_eq!((broker.revoked, broker.provider_calls), (2, 0));
    let api = filled_toward_chatgpt(&root, &mut credentials, &mut broker, ISSUED_ACCESS);
    audited(&login, &[Did::Revoked, Did::Captured]);
    audited(&logout, &[Did::Revoked]);
    audited(&api, &[Did::Filled]);
}
