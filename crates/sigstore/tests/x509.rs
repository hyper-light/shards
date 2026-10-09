//! What this crate's x509 makes of testdata/x509.json, against what Go 1.26's crypto/x509
//! made of it (scripts/sigstore/generate-x509): each certificate parsed to the same
//! fields or the same error, and each chain verified to the same chains or the same
//! error, word for word.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use base64::Engine as _;
use serde_json::{Value, json};
use shards_sigstore::der::oid_text;
use shards_sigstore::time::Time;
use shards_sigstore::x509::{Certificate, Eku, Options, Pool, PublicKey};

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap(), 16).unwrap())
        .collect()
}

fn eku_number(e: Eku) -> i64 {
    match e {
        Eku::Any => 0,
        Eku::ServerAuth => 1,
        Eku::ClientAuth => 2,
        Eku::CodeSigning => 3,
        Eku::EmailProtection => 4,
        Eku::IpsecEndSystem => 5,
        Eku::IpsecTunnel => 6,
        Eku::IpsecUser => 7,
        Eku::TimeStamping => 8,
        Eku::OcspSigning => 9,
        Eku::MicrosoftServerGatedCrypto => 10,
        Eku::NetscapeServerGatedCrypto => 11,
        Eku::MicrosoftCommercialCodeSigning => 12,
        Eku::MicrosoftKernelCodeSigning => 13,
    }
}

fn eku_of(n: i64) -> Eku {
    [
        Eku::Any,
        Eku::ServerAuth,
        Eku::ClientAuth,
        Eku::CodeSigning,
        Eku::EmailProtection,
        Eku::IpsecEndSystem,
        Eku::IpsecTunnel,
        Eku::IpsecUser,
        Eku::TimeStamping,
        Eku::OcspSigning,
        Eku::MicrosoftServerGatedCrypto,
        Eku::NetscapeServerGatedCrypto,
        Eku::MicrosoftCommercialCodeSigning,
        Eku::MicrosoftKernelCodeSigning,
    ][n as usize]
}

fn key(k: &PublicKey) -> String {
    match k {
        PublicKey::Rsa { n, e } => {
            let n = num_bigint::BigUint::from_bytes_be(n).to_str_radix(16);
            let e = e.iter().fold(0u64, |a, b| a << 8 | u64::from(*b));
            format!("rsa:{n}:{e}")
        }
        PublicKey::Ecdsa { curve, point } => format!("ecdsa:{}:{}", curve.go_name(), hex(point)),
        PublicKey::Ed25519(k) => format!("ed25519:{}", hex(k)),
        PublicKey::Dsa => "dsa".into(),
        PublicKey::Unknown => "none".into(),
    }
}

fn opt(v: Option<i64>) -> Value {
    v.map_or(Value::Null, |v| json!(v))
}

fn fields(c: &Certificate) -> Value {
    let nets = |l: &[(Vec<u8>, Vec<u8>)]| -> Vec<String> {
        l.iter().map(|(i, m)| format!("{}/{}", hex(i), hex(m))).collect()
    };
    let strs = |l: &[Vec<u8>]| -> Vec<String> {
        l.iter()
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    let mut f = json!({
        "version": c.version,
        "serial": c.serial_string(),
        "sigAlg": c.sig_alg.name(),
        "issuer": c.issuer_string(),
        "subject": shards_sigstore::x509::name_string(&c.subject),
        "notBefore": c.not_before.secs,
        "notAfter": c.not_after.secs,
        "key": key(&c.public_key),
        "keyUsage": c.key_usage,
        "bc": c.basic_constraints_valid,
        "isCA": c.is_ca,
        "maxPathLen": if c.basic_constraints_valid { c.max_path_len } else { 0 },
        "dns": c.dns_names,
        "emails": c.emails,
        "ski": hex(&c.subject_key_id),
        "aki": hex(&c.authority_key_id),
        "ekus": c.ext_key_usage.iter().map(|e| eku_number(*e)).collect::<Vec<_>>(),
        "unknownEkus": c.unknown_ext_key_usage.iter().map(|o| oid_text(o)).collect::<Vec<_>>(),
        "ips": c.ips.iter().map(|ip| shards_sigstore::x509_constraints::ip_string(ip)).collect::<Vec<_>>(),
        "uris": c.uris,
        "unhandled": c.unhandled_critical.iter().map(|o| oid_text(o)).collect::<Vec<_>>(),
        "policies": c.policies.iter().map(|p| hex(p)).collect::<Vec<_>>(),
        "mappings": c.policy_mappings.iter().map(|(a, b)| vec![hex(a), hex(b)]).collect::<Vec<_>>(),
        "requireExplicit": opt(c.require_explicit_policy),
        "inhibitMapping": opt(c.inhibit_policy_mapping),
        "inhibitAny": opt(c.inhibit_any_policy),
    });
    if let Some(nc) = &c.name_constraints {
        f["nc"] = json!({
            "pDNS": strs(&nc.permitted_dns), "xDNS": strs(&nc.excluded_dns),
            "pIP": nets(&nc.permitted_ips), "xIP": nets(&nc.excluded_ips),
            "pEmail": strs(&nc.permitted_emails), "xEmail": strs(&nc.excluded_emails),
            "pURI": strs(&nc.permitted_uris), "xURI": strs(&nc.excluded_uris),
        });
    }
    f
}

#[test]
fn certificates_parse_and_chains_verify_as_go_s_do() {
    let o: Value = serde_json::from_str(include_str!("../testdata/x509.json")).unwrap();
    let table: Vec<Vec<u8>> = o["der"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| b64(d.as_str().unwrap()))
        .collect();
    let mut failed = Vec::new();
    let parse = o["parse"].as_array().unwrap();
    for c in parse {
        let name = c["name"].as_str().unwrap();
        let base = &table[c["base"].as_u64().unwrap() as usize];
        let (pre, post) = (
            c["pre"].as_u64().unwrap() as usize,
            c["post"].as_u64().unwrap() as usize,
        );
        let der = [
            &base[..pre],
            &unhex(c["mid"].as_str().unwrap()),
            &base[base.len() - post..],
        ]
        .concat();
        let got = match Certificate::parse(&der) {
            Ok(cert) => (String::new(), fields(&cert)),
            Err(e) => (e.0, Value::Null),
        };
        let want = (
            c["error"].as_str().unwrap().to_string(),
            c.get("fields").cloned().unwrap_or(Value::Null),
        );
        if got != want {
            failed.push(format!("--- parse {name}\n  got  {got:?}\n  Go   {want:?}"));
        }
    }
    let verify = o["verify"].as_array().unwrap();
    for c in verify {
        let name = c["name"].as_str().unwrap();
        let certs: Vec<Certificate> = c["certs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| Certificate::parse(&table[i.as_u64().unwrap() as usize]).unwrap())
            .collect();
        let mut roots = Pool::default();
        for i in c["roots"].as_array().unwrap() {
            roots.add(certs[i.as_u64().unwrap() as usize].clone());
        }
        let mut inters = Pool::default();
        for i in c["inters"].as_array().unwrap() {
            inters.add(certs[i.as_u64().unwrap() as usize].clone());
        }
        let opts = Options {
            roots: &roots,
            intermediates: &inters,
            now: Time::utc(c["now"].as_i64().unwrap(), 0),
            key_usages: c["usages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| eku_of(u.as_i64().unwrap()))
                .collect(),
        };
        let leaf = &certs[c["leaf"].as_u64().unwrap() as usize];
        let got = match leaf.verify(&opts) {
            Ok(chains) => (
                String::new(),
                chains
                    .iter()
                    .map(|ch| {
                        ch.iter()
                            .map(|x| certs.iter().position(|y| y.raw == x.raw).map_or(-1, |p| p as i64))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
            ),
            Err(e) => (e.0, Vec::new()),
        };
        let want: (String, Vec<Vec<i64>>) = (
            c["error"].as_str().unwrap().to_string(),
            serde_json::from_value(c["chains"].clone()).unwrap(),
        );
        if got != want {
            failed.push(format!("--- verify {name}\n  got  {got:?}\n  Go   {want:?}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} differ:\n{}",
        failed.len(),
        parse.len() + verify.len(),
        failed.join("\n")
    );
}
