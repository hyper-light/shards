use super::*;

#[test]
fn creds_08_a_held_post_continues_before_its_body_and_only_once() {
    for chunked in [false, true] {
        for refresh in [false, true] {
            let root = root("shards test root");
            let mut credentials = sign_in_credentials(&root, true);
            let mut broker = Broker::holding(ISSUED_ACCESS, ISSUED_REFRESH);
            let mut cli = Bench::new(
                guest(credentials.ca.der().to_vec().into(), TOKEN_HOST, None),
                token_host(&root),
            );
            let (target, body) = if refresh {
                ("/v1/oauth/token", refresh_body())
            } else {
                (
                    "/v1/oauth/token/revoke",
                    format!(r#"{{"token":"{SIGNED_REFRESH}"}}"#),
                )
            };
            let framing = if chunked {
                "Transfer-Encoding: chunked\r\n".to_string()
            } else {
                format!("Content-Length: {}\r\n", body.len())
            };
            let head = format!(
                "POST {target} HTTP/1.1\r\nHost: {TOKEN_HOST}\r\nContent-Type: application/json\r\nExpect: 100-CoNtInUe\r\n{framing}\r\n"
            );
            cli.guest.writer().write_all(head.as_bytes()).unwrap();
            cli.settle(&mut credentials, &mut unanswered);
            assert_eq!(
                cli.got, b"HTTP/1.1 100 Continue\r\n\r\n",
                "the body has not been sent"
            );
            assert!(cli.h1().heads.is_empty(), "a held head reached the provider");
            cli.got.clear();
            cli.settle(&mut credentials, &mut unanswered);
            assert!(cli.got.is_empty(), "a retry sent another Continue");
            let data = if chunked {
                format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len())
            } else {
                body
            };
            cli.guest.writer().write_all(data.as_bytes()).unwrap();
            cli.until(&mut credentials, &mut |c| broker.serve(c), |b| whole(&b.got));
            assert!(cli.got.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(
                cli.h1().heads.is_empty(),
                "a refresh or revocation reached the provider"
            );
            assert_eq!(broker.provider_calls, usize::from(refresh));
            assert_eq!(broker.revoked, usize::from(!refresh));
        }
    }
}
