//! Credentials as aws-sdk-go-v2's default chain finds them (D168; config/resolve_credentials.go
//! and the providers under credentials/), for CloudWatch Logs (`--log-driver awslogs`) and
//! S3's build cache:
//! 1. the environment's keys (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//!    `AWS_SESSION_TOKEN`);
//! 2. its web identity (`AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN`): a role assumed with
//!    it through STS;
//! 3. the profile's (`AWS_PROFILE`, else `AWS_DEFAULT_PROFILE`, else `default`): its source
//!    profile's, its keys, its credential source's, its web identity's, its SSO session's,
//!    its login session's or its credential process's; else the container's endpoint's
//!    (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, `AWS_CONTAINER_CREDENTIALS_FULL_URI`); else
//!    EC2's instance metadata's; then its `role_arn`, assumed with them through STS.
//!
//! Credentials that expire say when, for their user to find them again then.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use shards_registry::http::{Client, Patience};
use shards_registry::url::Url;
use zeroize::Zeroizing;

use super::profile::{self, Files, Keys, Profile};
use super::sigv4::{self, Credentials, Rule, Scope};
use super::{env_bool, exchange, rfc3339, rfc3339_of, text};

/// Credentials found, and when they expire, where they do.
pub struct Found {
    pub creds: Credentials,
    pub expires: Option<SystemTime>,
}

impl Found {
    fn of(keys: Keys, expires: Option<SystemTime>) -> Found {
        Found {
            creds: Credentials {
                access_key: keys.access_key,
                secret: keys.secret,
                token: keys.token,
            },
            expires,
        }
    }

    /// Whether they have expired, or will within `window`.
    pub fn expiring(&self, window: Duration) -> bool {
        self.expires
            .is_some_and(|t| SystemTime::now().checked_add(window).is_none_or(|soon| t <= soon))
    }
}

/// How long STS gives a role's credentials, unless the profile asks for more than 15
/// minutes (stscreds DefaultDuration; credsFromAssumeRole).
const ROLE_SECONDS: u64 = 15 * 60;
/// How long an IMDS session token is asked to live (imds defaultTokenTTL).
const IMDS_TOKEN_SECONDS: &str = "300";
/// What IMDS is given: to connect, for an answer's head, and for each operation (imds
/// defaultDialerTimeout, defaultResponseHeaderTimeout, defaultOperationTimeout).
const IMDS_PATIENCE: Patience = Patience {
    connect: Duration::from_millis(250),
    head: Some(Duration::from_millis(500)),
    stall: Some(Duration::from_secs(5)),
};
/// How long IMDS's credentials are kept at most (ec2rolecreds' an hour).
const IMDS_KEPT: Duration = Duration::from_secs(3600);
/// How long a credential process may run (processcreds DefaultTimeout).
const PROCESS_WAIT: Duration = Duration::from_secs(60);
/// The ECS agent, a relative URI's base (ecsContainerEndpoint).
pub const ECS: &str = "http://169.254.170.2";
/// The most of a credential process's output read: credentials are a few hundred bytes.
const MOST: u64 = 1 << 20;
const FORM: &str = "application/x-www-form-urlencoded";

/// Where credentials are looked for: the environment and the files it names, and the
/// region the chain's own services (STS, sign-in) are asked in.
pub struct Chain<'a> {
    env: &'a dyn Fn(&str) -> Option<String>,
    /// The whole environment, `NAME=VALUE`, where it is not this process's: a credential
    /// process runs in it.
    listed: Option<&'a [String]>,
    files: Files,
    region: String,
}

impl<'a> Chain<'a> {
    pub fn new(env: &'a dyn Fn(&str) -> Option<String>) -> Chain<'a> {
        Chain {
            env,
            listed: None,
            files: Files::of(env),
            region: String::new(),
        }
    }

    /// This chain, its environment `vars` alone (a client's, the daemon's own left out of
    /// what it runs).
    // The daemon's, which is Unix's alone as yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn listing(mut self, vars: &'a [String]) -> Chain<'a> {
        self.listed = Some(vars);
        self
    }

    /// This chain, its services asked in `region`.
    pub fn in_region(mut self, region: &str) -> Chain<'a> {
        self.region = region.to_string();
        self
    }

    fn var(&self, k: &str) -> Option<String> {
        (self.env)(k).filter(|v| !v.is_empty())
    }

    /// The profile in use: `AWS_PROFILE`'s, `AWS_DEFAULT_PROFILE`'s, else `default`'s. A
    /// profile `AWS_PROFILE` names is to be in a file; one the others name is none where it
    /// is in neither (resolveConfigLoaders).
    pub fn profile(&self) -> Result<Option<Profile>, String> {
        let named = self.var("AWS_PROFILE");
        let name = named
            .clone()
            .or_else(|| self.var("AWS_DEFAULT_PROFILE"))
            .unwrap_or_else(|| "default".into());
        match profile::load(&self.files, &name)? {
            Some(p) => Ok(Some(p)),
            None if named.is_some() => {
                let paths: Vec<String> = self
                    .files
                    .config
                    .iter()
                    .chain(&self.files.credentials)
                    .map(|p| p.display().to_string())
                    .collect();
                Err(format!(
                    "AWS_PROFILE={name}: no such profile in {}",
                    paths.join(" or ")
                ))
            }
            None => Ok(None),
        }
    }

    /// The region the configuration names: `AWS_REGION`, `AWS_DEFAULT_REGION`, else the
    /// profile's (resolveRegion).
    pub fn configured_region(&self, top: Option<&Profile>) -> Option<String> {
        self.var("AWS_REGION")
            .or_else(|| self.var("AWS_DEFAULT_REGION"))
            .or_else(|| top.map(|p| p.region.clone()).filter(|r| !r.is_empty()))
    }

    /// The region EC2's instance metadata says this host is in (imds GetRegion).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn instance_region(&self, top: Option<&Profile>) -> Result<String, String> {
        let region = self.imds(top)?.get("/latest/meta-data/placement/region")?;
        if region.is_empty() {
            return Err("EC2's instance metadata names no region".into());
        }
        Ok(region)
    }

    /// The chain's credentials (resolveCredentialChain).
    pub fn resolve(&self) -> Result<Found, String> {
        let profile = self.profile()?;
        if let Some(keys) = self.env_keys() {
            return Ok(Found::of(keys, None));
        }
        if let Some(file) = self.var("AWS_WEB_IDENTITY_TOKEN_FILE") {
            let role = self.var("AWS_ROLE_ARN").unwrap_or_default();
            let session = self.var("AWS_ROLE_SESSION_NAME").unwrap_or_default();
            return self.web_identity(&file, &role, &session, profile.as_ref());
        }
        let top = profile.unwrap_or_default();
        self.of_profile(&top, &top)
    }

    /// The environment's keys, where it has both.
    fn env_keys(&self) -> Option<Keys> {
        let id = self
            .var("AWS_ACCESS_KEY_ID")
            .or_else(|| self.var("AWS_ACCESS_KEY"))?;
        let secret = self
            .var("AWS_SECRET_ACCESS_KEY")
            .or_else(|| self.var("AWS_SECRET_KEY"))?;
        Some(Keys {
            access_key: id,
            secret: Zeroizing::new(secret),
            token: self.var("AWS_SESSION_TOKEN").map(Zeroizing::new),
        })
    }

    /// Profile `p`'s credentials, `top` the profile in use (resolveCredsFromProfile).
    fn of_profile(&self, p: &Profile, top: &Profile) -> Result<Found, String> {
        let base = if let Some(source) = &p.source {
            self.of_profile(source, top)?
        } else if let Some(keys) = &p.keys {
            Found::of(keys.clone(), None)
        } else if !p.credential_source.is_empty() {
            self.of_source(&p.credential_source, top)?
        } else if !p.web_identity_token_file.is_empty() {
            // Its role is the one assumed with it; no other is (assumeWebIdentity).
            return self.web_identity(
                &p.web_identity_token_file,
                &p.role_arn,
                &p.role_session_name,
                Some(top),
            );
        } else if p.has_sso() {
            self.sso(p, top)?
        } else if !p.login_session.is_empty() {
            self.login(p, top)?
        } else if !p.credential_process.is_empty() {
            self.process(&p.credential_process)?
        } else if let Some(found) = self.container()? {
            found
        } else {
            // The last place looked: its refusal says where else none was found.
            self.instance(top).map_err(|e| {
                format!(
                    "none in the environment (AWS_ACCESS_KEY_ID), the config and credentials files, a container's endpoint (AWS_CONTAINER_CREDENTIALS_RELATIVE_URI, AWS_CONTAINER_CREDENTIALS_FULL_URI), nor EC2's instance metadata ({e})"
                )
            })?
        };
        if p.role_arn.is_empty() {
            Ok(base)
        } else {
            self.assume_role(&base, p, top)
        }
    }

    /// A profile's `credential_source`'s credentials (resolveCredsFromSource).
    fn of_source(&self, source: &str, top: &Profile) -> Result<Found, String> {
        match source {
            "Ec2InstanceMetadata" => self
                .instance(top)
                .map_err(|e| format!("credential_source Ec2InstanceMetadata: {e}")),
            "Environment" => self.env_keys().map(|k| Found::of(k, None)).ok_or_else(|| {
                "credential_source Environment: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are not both set"
                    .into()
            }),
            "EcsContainer" => self.container()?.ok_or_else(|| {
                "credential_source EcsContainer: neither AWS_CONTAINER_CREDENTIALS_RELATIVE_URI nor AWS_CONTAINER_CREDENTIALS_FULL_URI is set"
                    .into()
            }),
            other => Err(format!(
                "credential_source {other}: EcsContainer, Ec2InstanceMetadata or Environment"
            )),
        }
    }

    /// The client the chain's services are asked with, as the configuration says.
    fn client(&self, top: Option<&Profile>) -> Result<Client, String> {
        super::client(self.env, top, Patience::default())
    }

    /// Whether FIPS's endpoints, and dual-stack's, are asked for: `AWS_USE_FIPS_ENDPOINT`,
    /// `AWS_USE_DUALSTACK_ENDPOINT`, else the profile's.
    pub fn variants(&self, top: Option<&Profile>) -> Result<(bool, bool), String> {
        let fips = env_bool(self.env, "AWS_USE_FIPS_ENDPOINT")?
            .or_else(|| top.and_then(|p| p.use_fips_endpoint))
            .unwrap_or(false);
        let dual = env_bool(self.env, "AWS_USE_DUALSTACK_ENDPOINT")?
            .or_else(|| top.and_then(|p| p.use_dualstack_endpoint))
            .unwrap_or(false);
        Ok((fips, dual))
    }

    /// Service `id`'s endpoint: the configured one, else its own (`prefix`'s) in `region`.
    pub fn service(
        &self,
        top: Option<&Profile>,
        id: &str,
        prefix: &str,
        region: &str,
    ) -> Result<String, String> {
        if let Some(e) = super::configured_endpoint(self.env, top, id)? {
            return Ok(e.trim_end_matches('/').to_string());
        }
        if region.is_empty() {
            return Err(format!(
                "no region to ask {id} in: set AWS_REGION, or the profile's region"
            ));
        }
        let (fips, dual) = self.variants(top)?;
        Ok(match prefix {
            "signin" => super::signin_endpoint(region, fips, dual),
            _ => super::endpoint(prefix, region, fips, dual),
        })
    }

    /// `p`'s role assumed with `base` (stscreds AssumeRoleProvider), its session name,
    /// external ID and duration as it says. One whose profile asks for an MFA code
    /// (`mfa_serial`) is refused: a daemon has no one to ask for it.
    fn assume_role(&self, base: &Found, p: &Profile, top: &Profile) -> Result<Found, String> {
        if !p.mfa_serial.is_empty() {
            return Err(format!(
                "the AWS profile {} assumes {} with an MFA code (mfa_serial), which shards has no one to ask for",
                p.name, p.role_arn
            ));
        }
        let session = if p.role_session_name.is_empty() {
            format!("aws-go-sdk-{}", now_nanos())
        } else {
            p.role_session_name.clone()
        };
        let seconds = p
            .duration_seconds
            .filter(|s| s / 60 > 15)
            .unwrap_or(ROLE_SECONDS)
            .to_string();
        let mut pairs = vec![
            ("Action", "AssumeRole"),
            ("DurationSeconds", seconds.as_str()),
            ("RoleArn", p.role_arn.as_str()),
            ("RoleSessionName", session.as_str()),
            ("Version", "2011-06-15"),
        ];
        if !p.external_id.is_empty() {
            pairs.push(("ExternalId", p.external_id.as_str()));
        }
        let body = super::form(&pairs);
        let url =
            Url::parse(&self.service(Some(top), "STS", "sts", &self.region)?).map_err(|e| e.to_string())?;
        let (status, answer) = super::post_signed(
            &self.client(Some(top))?,
            &url,
            &[("Content-Type", FORM)],
            body.as_bytes(),
            &base.creds,
            &Scope {
                region: &self.region,
                service: "sts",
                rule: Rule::Ignored,
            },
        )?;
        sts_credentials(status, &answer, &format!("assuming {}", p.role_arn))
    }

    /// A role assumed with the web identity token in `file` (stscreds
    /// WebIdentityRoleProvider), as STS takes it, unsigned.
    fn web_identity(
        &self,
        file: &str,
        role: &str,
        session: &str,
        top: Option<&Profile>,
    ) -> Result<Found, String> {
        if role.is_empty() {
            return Err(format!(
                "the web identity token {file} names no role to assume: set AWS_ROLE_ARN, or the profile's role_arn"
            ));
        }
        let token =
            std::fs::read_to_string(file).map_err(|e| format!("the web identity token {file}: {e}"))?;
        let session = if session.is_empty() {
            now_nanos().to_string()
        } else {
            session.to_string()
        };
        let body = super::form(&[
            ("Action", "AssumeRoleWithWebIdentity"),
            ("RoleArn", role),
            ("RoleSessionName", &session),
            ("Version", "2011-06-15"),
            ("WebIdentityToken", token.trim()),
        ]);
        let url = self.service(top, "STS", "sts", &self.region)?;
        let (status, answer) = exchange(
            &self.client(top)?,
            "POST",
            &url,
            &[("Content-Type", FORM)],
            body.as_bytes(),
        )?;
        sts_credentials(status, &answer, &format!("assuming {role} with a web identity"))
    }

    /// The container's credentials, where its endpoint is given: a relative URI's at the
    /// ECS agent's address; a full URI's over HTTPS, or over HTTP where its host is
    /// loopback or the ECS or EKS agent's; with the token in the file
    /// `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE` names, read each time, else
    /// `AWS_CONTAINER_AUTHORIZATION_TOKEN`'s (resolveHTTPCredProvider).
    fn container(&self) -> Result<Option<Found>, String> {
        let url = if let Some(relative) = self.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            format!("{ECS}{relative}")
        } else if let Some(full) = self.var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
            let parsed =
                Url::parse(&full).map_err(|e| format!("AWS_CONTAINER_CREDENTIALS_FULL_URI={full}: {e}"))?;
            if full.starts_with("http://") && !allowed_host(parsed.host())? {
                return Err(format!(
                    "AWS_CONTAINER_CREDENTIALS_FULL_URI={full}: over HTTP, only a loopback, ECS or EKS address"
                ));
            }
            full
        } else {
            return Ok(None);
        };
        let token = match self.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
            Some(file) => Some(
                std::fs::read_to_string(&file)
                    .map_err(|e| format!("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE={file}: {e}"))?,
            ),
            None => self.var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
        };
        self.endpoint(&url, token.as_deref()).map(Some)
    }

    /// The credentials an endpoint gives (endpointcreds): the container's, and awslogs'
    /// `awslogs-credentials-endpoint`'s. Its token goes as `Authorization`, a line ending
    /// after it left out, as a file holding it may end with one; a token with any other
    /// is refused.
    pub fn endpoint(&self, url: &str, token: Option<&str>) -> Result<Found, String> {
        let token = token.map(|t| {
            t.strip_suffix('\n')
                .map_or(t, |t| t.strip_suffix('\r').unwrap_or(t))
        });
        if token.is_some_and(|t| t.contains(['\r', '\n'])) {
            return Err(format!("{url}: its authorization token has a line break in it"));
        }
        // The SDK's endpoint client trusts the platform's roots, whatever bundle the rest
        // is given.
        let env = |k: &str| if k == "AWS_CA_BUNDLE" { None } else { (self.env)(k) };
        let client = super::client(&env, None, Patience::default())?;
        let mut fields = vec![("Accept", "application/json")];
        if let Some(t) = token.filter(|t| !t.is_empty()) {
            fields.push(("Authorization", t));
        }
        let (status, answer) = exchange(&client, "GET", url, &fields, &[])?;
        let v: Option<Value> = serde_json::from_slice(&answer).ok();
        if !(200..300).contains(&status) {
            let said = v
                .as_ref()
                .and_then(|v| Some(format!("{}: {}", text(v, "code")?, text(v, "message")?)));
            return Err(format!(
                "{url}: {status} {}",
                said.unwrap_or_else(|| String::from_utf8_lossy(&answer).trim().to_string())
            ));
        }
        v.as_ref()
            .and_then(|v| keys_of(v, "AccessKeyId", "SecretAccessKey", "Token", "Expiration"))
            .ok_or_else(|| format!("{url}: no credentials in its answer"))
    }

    /// EC2's instance metadata's credentials (ec2rolecreds): its first role's, kept an
    /// hour at most.
    fn instance(&self, top: &Profile) -> Result<Found, String> {
        let imds = self.imds(Some(top))?;
        let roles = imds.get("/latest/meta-data/iam/security-credentials/")?;
        let role = roles.lines().next().unwrap_or_default();
        if role.is_empty() {
            return Err("EC2's instance metadata names no role".into());
        }
        let answer = imds.get(&format!("/latest/meta-data/iam/security-credentials/{role}"))?;
        let v: Value = serde_json::from_str(&answer).map_err(|e| format!("EC2's role {role}: {e}"))?;
        if !text(&v, "Code").is_some_and(|c| c.eq_ignore_ascii_case("Success")) {
            return Err(format!(
                "EC2's role {role}: {}: {}",
                text(&v, "Code").unwrap_or_default(),
                text(&v, "Message").unwrap_or_default()
            ));
        }
        let mut found = keys_of(&v, "AccessKeyId", "SecretAccessKey", "Token", "Expiration")
            .ok_or_else(|| format!("EC2's role {role}: no credentials"))?;
        let most = SystemTime::now().checked_add(IMDS_KEPT);
        found.expires = match (found.expires, most) {
            (Some(t), Some(most)) => Some(t.min(most)),
            (t, most) => t.or(most),
        };
        Ok(found)
    }

    /// EC2's instance metadata service, as the configuration says to reach it: refused
    /// where `AWS_EC2_METADATA_DISABLED` says; at `AWS_EC2_METADATA_SERVICE_ENDPOINT`, the
    /// profile's, else IPv4's or, where its mode says, IPv6's address; IMDSv1 where IMDSv2's
    /// token cannot be had, unless `AWS_EC2_METADATA_V1_DISABLED` or the profile says not.
    fn imds(&self, top: Option<&Profile>) -> Result<Imds, String> {
        if self
            .var("AWS_EC2_METADATA_DISABLED")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            return Err("EC2's instance metadata is not asked (AWS_EC2_METADATA_DISABLED)".into());
        }
        let configured = self.var("AWS_EC2_METADATA_SERVICE_ENDPOINT").or_else(|| {
            top.map(|p| p.ec2_metadata_service_endpoint.clone())
                .filter(|e| !e.is_empty())
        });
        let base = match configured {
            Some(e) => e.trim_end_matches('/').to_string(),
            None => {
                let mode = self.var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE").or_else(|| {
                    top.map(|p| p.ec2_metadata_service_endpoint_mode.clone())
                        .filter(|m| !m.is_empty())
                });
                match mode {
                    None => "http://169.254.169.254".to_string(),
                    Some(m) if m.eq_ignore_ascii_case("ipv4") => "http://169.254.169.254".to_string(),
                    Some(m) if m.eq_ignore_ascii_case("ipv6") => "http://[fd00:ec2::254]".to_string(),
                    Some(m) => {
                        return Err(format!(
                            "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE={m}: IPv4 or IPv6"
                        ));
                    }
                }
            }
        };
        let v1 = !env_bool(self.env, "AWS_EC2_METADATA_V1_DISABLED")?
            .or_else(|| top.and_then(|p| p.ec2_metadata_v1_disabled))
            .unwrap_or(false);
        Ok(Imds {
            client: super::client(self.env, top, IMDS_PATIENCE)?,
            base,
            v1,
            session: std::cell::RefCell::new(None),
        })
    }

    /// An SSO profile's role's credentials (ssocreds): asked for with the token `aws sso
    /// login` keeps (`~/.aws/sso/cache/`, by the SHA-1 of the session's name, or of a legacy
    /// profile's start URL), a session's refreshed through SSO OIDC once it has expired.
    fn sso(&self, p: &Profile, top: &Profile) -> Result<Found, String> {
        let missing = |keys: Vec<&str>| {
            format!(
                "the AWS profile {} signs in with SSO and does not say its {}",
                p.name,
                keys.join(", ")
            )
        };
        let (key, region) = if !p.sso_session_name.is_empty() {
            let Some(s) = &p.sso_session else {
                return Err(missing(vec!["sso-session section"]));
            };
            let lacks: Vec<&str> = [("sso_region", &s.region), ("sso_start_url", &s.start_url)]
                .into_iter()
                .filter(|(_, v)| v.is_empty())
                .map(|(k, _)| k)
                .collect();
            if !lacks.is_empty() {
                return Err(missing(lacks));
            }
            for (k, own, session) in [
                ("sso_region", &p.sso_region, &s.region),
                ("sso_start_url", &p.sso_start_url, &s.start_url),
            ] {
                if !own.is_empty() && own != session {
                    return Err(format!(
                        "the AWS profile {}: its {k} is not its sso-session's",
                        p.name
                    ));
                }
            }
            (s.name.clone(), s.region.clone())
        } else {
            (p.sso_start_url.clone(), p.sso_region.clone())
        };
        // Account and role, which the SDK leaves to SSO to refuse.
        let lacks: Vec<&str> = [
            ("sso_region", region.as_str()),
            ("sso_start_url", key.as_str()),
            ("sso_account_id", p.sso_account_id.as_str()),
            ("sso_role_name", p.sso_role_name.as_str()),
        ]
        .into_iter()
        .filter(|(_, v)| v.is_empty())
        .map(|(k, _)| k)
        .collect();
        if !lacks.is_empty() {
            return Err(missing(lacks));
        }
        let cache = self.home()?.join(".aws/sso/cache").join(format!(
            "{}.json",
            digest_hex(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, &key)
        ));
        let said = |e: &dyn std::fmt::Display| {
            format!(
                "the SSO token {}: {e}: sign in again with `aws sso login`",
                cache.display()
            )
        };
        let mut token: Value = std::fs::read(&cache)
            .map_err(|e| said(&e))
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| said(&e)))?;
        let expires = text(&token, "expiresAt").as_deref().and_then(rfc3339);
        if text(&token, "accessToken").is_none() || expires.is_none() {
            return Err(said(&"it has no accessToken and expiresAt"));
        }
        if expires.is_some_and(|t| t <= SystemTime::now()) {
            if p.sso_session_name.is_empty() {
                return Err(said(&"it has expired"));
            }
            token = self.sso_refresh(token, &region, top)?;
            keep(&cache, &token).map_err(|e| said(&e))?;
        }
        let access = text(&token, "accessToken").unwrap_or_default();
        let base = self.service(Some(top), "SSO", "portal.sso", &region)?;
        let url = format!(
            "{base}/federation/credentials?account_id={}&role_name={}",
            super::query_escape(&p.sso_account_id),
            super::query_escape(&p.sso_role_name)
        );
        let (status, answer) = exchange(
            &self.client(Some(top))?,
            "GET",
            &url,
            &[("x-amz-sso_bearer_token", &access)],
            &[],
        )?;
        if status != 200 {
            return Err(format!(
                "SSO's role {} in account {}: {status} {}",
                p.sso_role_name,
                p.sso_account_id,
                String::from_utf8_lossy(&answer).trim()
            ));
        }
        let v: Value =
            serde_json::from_slice(&answer).map_err(|e| format!("SSO's role {}: {e}", p.sso_role_name))?;
        let role = v.get("roleCredentials").unwrap_or(&Value::Null);
        let expires = role
            .get("expiration")
            .and_then(Value::as_u64)
            .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
        let given = |k: &str| {
            role.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match (given("accessKeyId"), given("secretAccessKey")) {
            (Some(id), Some(secret)) => Ok(Found::of(
                Keys {
                    access_key: id,
                    secret: Zeroizing::new(secret),
                    token: given("sessionToken").map(Zeroizing::new),
                },
                expires,
            )),
            _ => Err(format!(
                "SSO gave no credentials for the role {}",
                p.sso_role_name
            )),
        }
    }

    /// An SSO session's token refreshed through SSO OIDC's CreateToken (SSOTokenProvider),
    /// its client's and refresh token's: the new refresh token, where one is given.
    fn sso_refresh(&self, mut token: Value, region: &str, top: &Profile) -> Result<Value, String> {
        let (Some(id), Some(secret), Some(refresh)) = (
            text(&token, "clientId"),
            text(&token, "clientSecret"),
            text(&token, "refreshToken"),
        ) else {
            return Err(
                "the SSO token has expired and cannot be refreshed: sign in again with `aws sso login`"
                    .into(),
            );
        };
        let body = serde_json::json!({
            "clientId": id,
            "clientSecret": secret,
            "grantType": "refresh_token",
            "refreshToken": refresh,
        })
        .to_string();
        let url = format!("{}/token", self.service(Some(top), "SSO OIDC", "oidc", region)?);
        let (status, answer) = exchange(
            &self.client(Some(top))?,
            "POST",
            &url,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        )?;
        if status != 200 {
            return Err(format!(
                "refreshing the SSO token: {status} {}: sign in again with `aws sso login`",
                String::from_utf8_lossy(&answer).trim()
            ));
        }
        let fresh: Value =
            serde_json::from_slice(&answer).map_err(|e| format!("refreshing the SSO token: {e}"))?;
        let lasts = fresh.get("expiresIn").and_then(Value::as_u64).unwrap_or(0);
        let at = SystemTime::now()
            .checked_add(Duration::from_secs(lasts))
            .ok_or("refreshing the SSO token: its lifetime is past the clock's")?;
        if let Some(o) = token.as_object_mut() {
            o.insert(
                "accessToken".into(),
                fresh.get("accessToken").cloned().unwrap_or(Value::Null),
            );
            o.insert("expiresAt".into(), Value::String(rfc3339_of(at)));
            if let Some(r) = fresh.get("refreshToken").filter(|r| r.is_string()) {
                o.insert("refreshToken".into(), r.clone());
            }
        }
        Ok(token)
    }

    /// A login session's credentials (logincreds): those `aws login` keeps
    /// (`AWS_LOGIN_CACHE_DIRECTORY`, else `~/.aws/login/cache/`, by the SHA-256 of the
    /// session's name), refreshed through sign-in's CreateOAuth2Token once they expire,
    /// with a DPoP proof signed by the key kept beside them.
    fn login(&self, p: &Profile, top: &Profile) -> Result<Found, String> {
        let dir = match self.var("AWS_LOGIN_CACHE_DIRECTORY") {
            Some(d) => PathBuf::from(d),
            None => self.home()?.join(".aws/login/cache"),
        };
        let name = digest_hex(&aws_lc_rs::digest::SHA256, p.login_session.trim());
        let cache = dir.join(format!("{name}.json"));
        let said = |e: &dyn std::fmt::Display| {
            format!(
                "the login session {}: {e}: sign in again with `aws login`",
                cache.display()
            )
        };
        let mut token: Value = std::fs::read(&cache)
            .map_err(|e| said(&e))
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| said(&e)))?;
        let access = token.get("accessToken").unwrap_or(&Value::Null);
        for (v, k) in [
            (access, "accessKeyId"),
            (access, "secretAccessKey"),
            (access, "sessionToken"),
            (access, "accountId"),
            (access, "expiresAt"),
            (&token, "clientId"),
            (&token, "refreshToken"),
            (&token, "dpopKey"),
        ] {
            if text(v, k).is_none() {
                return Err(said(&format!("it has no {k}")));
            }
        }
        let expires = text(access, "expiresAt").as_deref().and_then(rfc3339);
        if expires.is_none_or(|t| t <= SystemTime::now()) {
            let url = format!(
                "{}/v1/token",
                self.service(Some(top), "Signin", "signin", &self.region)?
            );
            let proof = dpop(&text(&token, "dpopKey").unwrap_or_default(), &url)?;
            let body = serde_json::json!({
                "clientId": text(&token, "clientId"),
                "grantType": "refresh_token",
                "refreshToken": text(&token, "refreshToken"),
            })
            .to_string();
            let (status, answer) = exchange(
                &self.client(Some(top))?,
                "POST",
                &url,
                &[("Content-Type", "application/json"), ("DPoP", &proof)],
                body.as_bytes(),
            )?;
            if status != 200 {
                return Err(said(&format!(
                    "refreshing it: {status} {}",
                    String::from_utf8_lossy(&answer).trim()
                )));
            }
            let fresh: Value = serde_json::from_slice(&answer).map_err(|e| said(&e))?;
            let new = fresh.get("accessToken").unwrap_or(&Value::Null);
            let lasts = fresh.get("expiresIn").and_then(Value::as_u64).unwrap_or(0);
            let at = SystemTime::now()
                .checked_add(Duration::from_secs(lasts))
                .ok_or_else(|| said(&"its lifetime is past the clock's"))?;
            for k in ["accessKeyId", "secretAccessKey", "sessionToken"] {
                if new.get(k).and_then(Value::as_str).is_none() {
                    return Err(said(&format!("sign-in gave no {k}")));
                }
            }
            let new = new.clone();
            if let Some(o) = token.get_mut("accessToken").and_then(Value::as_object_mut) {
                for k in ["accessKeyId", "secretAccessKey", "sessionToken"] {
                    o.insert(k.into(), new.get(k).cloned().unwrap_or(Value::Null));
                }
                o.insert("expiresAt".into(), Value::String(rfc3339_of(at)));
            }
            if let (Some(o), Some(r)) = (
                token.as_object_mut(),
                fresh.get("refreshToken").filter(|r| r.is_string()),
            ) {
                o.insert("refreshToken".into(), r.clone());
            }
            keep(&cache, &token).map_err(|e| said(&e))?;
        }
        let access = token.get("accessToken").unwrap_or(&Value::Null);
        keys_of(
            access,
            "accessKeyId",
            "secretAccessKey",
            "sessionToken",
            "expiresAt",
        )
        .ok_or_else(|| said(&"it has no credentials"))
    }

    /// The home the SDK's caches are under: `HOME`, else `USERPROFILE`.
    fn home(&self) -> Result<PathBuf, String> {
        self.var("HOME")
            .or_else(|| self.var("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| "no home directory (HOME) for AWS's caches".into())
    }

    /// A credential process's credentials (processcreds): run by `sh -c` (`cmd.exe /C` on
    /// Windows) in the chain's environment, its standard error this process's, for a
    /// minute at most; its output `{"Version": 1, "AccessKeyId", "SecretAccessKey",
    /// "SessionToken", "Expiration"}`.
    fn process(&self, command: &str) -> Result<Found, String> {
        let out = run_process(command, &self.process_env())?;
        let v: Value =
            serde_json::from_slice(&out).map_err(|e| format!("credential_process: its output: {e}"))?;
        if super::field(&v, "Version").as_i64() != Some(1) {
            return Err("credential_process: its output's Version is not 1".into());
        }
        if text(&v, "AccessKeyId").is_none() {
            return Err("credential_process: its output has no AccessKeyId".into());
        }
        keys_of(&v, "AccessKeyId", "SecretAccessKey", "SessionToken", "Expiration")
            .ok_or_else(|| "credential_process: its output has no SecretAccessKey".into())
    }

    /// The environment a credential process runs in: this process's, unless the chain's
    /// is another's, which then takes the place of all of this one's the SDK reads.
    fn process_env(&self) -> Vec<(String, std::ffi::OsString)> {
        let own = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v)));
        match self.listed {
            None => own.collect(),
            Some(vars) => own
                .filter(|(k, _)| !super::reads(k))
                .chain(vars.iter().filter_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    Some((k.to_string(), std::ffi::OsString::from(v)))
                }))
                .collect(),
        }
    }
}

/// EC2's instance metadata service, as the SDK's imds client asks it.
struct Imds {
    client: Client,
    base: String,
    /// IMDSv1 is asked where IMDSv2's token cannot be had.
    v1: bool,
    /// The session's token, once asked for: none where IMDSv1 is asked instead. The
    /// chain's few reads are over long before its five minutes are.
    session: std::cell::RefCell<Option<Option<String>>>,
}

impl Imds {
    /// A session token; none where IMDSv1 is to be asked instead: where the service
    /// refuses tokens (403, 404, 405) or gives none, unless IMDSv1 is disabled. Refused
    /// as malformed (400), it is an error (tokenProvider.updateToken).
    fn token(&self) -> Result<Option<String>, String> {
        let url = format!("{}/latest/api/token", self.base);
        let answer = exchange(
            &self.client,
            "PUT",
            &url,
            &[("X-Aws-Ec2-Metadata-Token-Ttl-Seconds", IMDS_TOKEN_SECONDS)],
            &[],
        );
        let why = match answer {
            Ok((200, t)) => return Ok(Some(String::from_utf8_lossy(&t).into_owned())),
            Ok((400, t)) => {
                return Err(format!(
                    "EC2's instance metadata: a session token: 400 {}",
                    String::from_utf8_lossy(&t).trim()
                ));
            }
            Ok((status, _)) => format!("EC2's instance metadata: a session token: {status}"),
            Err(e) => format!("EC2's instance metadata: {e}"),
        };
        if self.v1 { Ok(None) } else { Err(why) }
    }

    /// What `path` holds: asked with the session's token, else with none; once more with
    /// a new token where the answer is 401, as a token expired or IMDSv1 refused says.
    fn get(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{path}", self.base);
        let ask = |token: Option<&str>| {
            let fields: Vec<(&str, &str)> = token
                .map(|t| ("X-Aws-Ec2-Metadata-Token", t))
                .into_iter()
                .collect();
            exchange(&self.client, "GET", &url, &fields, &[])
                .map_err(|e| format!("EC2's instance metadata: {e}"))
        };
        let held = self.session.borrow().clone();
        let token = match held {
            Some(token) => token,
            None => {
                let token = self.token()?;
                *self.session.borrow_mut() = Some(token.clone());
                token
            }
        };
        let (mut status, mut body) = ask(token.as_deref())?;
        if status == 401 {
            let token = self
                .token()?
                .ok_or_else(|| format!("EC2's instance metadata: {path}: 401"))?;
            *self.session.borrow_mut() = Some(Some(token.clone()));
            (status, body) = ask(Some(&token))?;
        }
        if status != 200 {
            return Err(format!("EC2's instance metadata: {path}: {status}"));
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// Whether a full URI's host may be reached over plain HTTP: loopback, or the ECS or EKS
/// agent's address; a name, where every address it has is (isAllowedHost).
fn allowed_host(host: &str) -> Result<bool, String> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs as _};
    let allowed = |ip: IpAddr| {
        let ip = ip.to_canonical();
        ip.is_loopback()
            || ip == IpAddr::V4(Ipv4Addr::new(169, 254, 170, 2))
            || ip == IpAddr::V4(Ipv4Addr::new(169, 254, 170, 23))
            || ip == IpAddr::V6(Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x23))
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(allowed(ip));
    }
    let addrs: Vec<IpAddr> = (bare, 0)
        .to_socket_addrs()
        .map_err(|e| format!("{bare}: {e}"))?
        .map(|a| a.ip())
        .collect();
    Ok(!addrs.is_empty() && addrs.into_iter().all(allowed))
}

/// STS's answer's credentials, or why there are none.
fn sts_credentials(status: u16, answer: &[u8], doing: &str) -> Result<Found, String> {
    let text = String::from_utf8_lossy(answer);
    if status != 200 {
        return Err(format!(
            "{doing}: STS answered {status}: {}: {}",
            super::xml_text(&text, "Code").unwrap_or_default(),
            super::xml_text(&text, "Message").unwrap_or_default()
        ));
    }
    let field = |tag: &str| super::xml_text(&text, tag).filter(|v| !v.is_empty());
    let (Some(id), Some(secret)) = (field("AccessKeyId"), field("SecretAccessKey")) else {
        return Err(format!("{doing}: STS gave no credentials"));
    };
    Ok(Found::of(
        Keys {
            access_key: id,
            secret: Zeroizing::new(secret),
            token: field("SessionToken").map(Zeroizing::new),
        },
        field("Expiration").as_deref().and_then(rfc3339),
    ))
}

/// Keys from an answer's fields, as Go's encoding/json reads them, and when they expire.
fn keys_of(v: &Value, id: &str, secret: &str, token: &str, expires: &str) -> Option<Found> {
    Some(Found::of(
        Keys {
            access_key: text(v, id)?,
            secret: Zeroizing::new(text(v, secret)?),
            token: text(v, token).map(Zeroizing::new),
        },
        text(v, expires).as_deref().and_then(rfc3339),
    ))
}

/// A cache the SDK keeps, written again whole or not at all: beside it, then in its place,
/// its mode kept, as ssocreds' storeCachedToken writes it; readable by no one else where
/// it had none.
fn keep(path: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    let bytes = serde_json::to_vec(value)?;
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp-{}", now_nanos()));
    let temp = PathBuf::from(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mode = std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777);
        options.mode(mode);
    }
    let written = options.open(&temp).and_then(|mut f| {
        f.write_all(&bytes)?;
        f.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&temp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

#[cfg(unix)]
fn run_process(command: &str, env: &[(String, std::ffi::OsString)]) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let said = |e: &dyn std::fmt::Display| format!("credential_process: {e}");
    let (mut read, write) = std::io::pipe().map_err(|e| said(&e))?;
    let null = std::fs::File::open("/dev/null").map_err(|e| said(&e))?;
    let stderr = std::io::stderr();
    let args = [std::ffi::OsStr::new("-c"), std::ffi::OsStr::new(command)];
    let vars: Vec<(&str, &std::ffi::OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
    let child = shards_ipc::spawn_in(
        Path::new("/bin/sh"),
        &args,
        &[(null.as_fd(), 0), (write.as_fd(), 1), (stderr.as_fd(), 2)],
        false,
        &vars,
    )
    .map_err(|e| said(&e))?;
    drop(write);
    let ended = |child: &shards_ipc::Child, why: String| {
        let _ = child.kill(libc::SIGKILL);
        let _ = child.wait();
        Err(said(&why))
    };
    let deadline = std::time::Instant::now() + PROCESS_WAIT;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd: read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(left.as_millis().saturating_add(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: poll(2) of one pollfd, of a pipe this function holds.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return ended(&child, e.to_string());
        }
        if ready == 0 {
            if left.is_zero() {
                return ended(&child, format!("it ran past {} s", PROCESS_WAIT.as_secs()));
            }
            continue;
        }
        match read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(buf.get(..n).unwrap_or_default());
                if out.len() as u64 > MOST {
                    return ended(&child, "its output is past a mebibyte".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return ended(&child, e.to_string()),
        }
    }
    match child.wait() {
        Ok(0) => Ok(out),
        Ok(code) => Err(said(&format!("it exited {code}"))),
        Err(e) => Err(said(&e)),
    }
}

#[cfg(not(unix))]
fn run_process(command: &str, env: &[(String, std::ffi::OsString)]) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let said = |e: &dyn std::fmt::Display| format!("credential_process: {e}");
    let mut child = std::process::Command::new("cmd.exe")
        .args(["/C", command])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| said(&e))?;
    let stdout = child.stdout.take();
    let mut ended = |why: String| {
        let _ = child.kill();
        let _ = child.wait();
        Err(said(&why))
    };
    let Some(stdout) = stdout else {
        return ended("no output to read".into());
    };
    // Read on a thread of its own, so that waiting for it can end in time.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("credential-process".into())
        .spawn(move || {
            let mut out = Vec::new();
            let read = stdout
                .take(MOST.saturating_add(1))
                .read_to_end(&mut out)
                .map(|_| out);
            let _ = tx.send(read);
        });
    if let Err(e) = reader {
        return ended(e.to_string());
    }
    let out = match rx.recv_timeout(PROCESS_WAIT) {
        Ok(Ok(out)) if out.len() as u64 > MOST => return ended("its output is past a mebibyte".into()),
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return ended(e.to_string()),
        Err(_) => return ended(format!("it ran past {} s", PROCESS_WAIT.as_secs())),
    };
    match child.wait() {
        Ok(status) if status.success() => Ok(out),
        Ok(status) => Err(said(&format!("it exited {status}"))),
        Err(e) => Err(said(&e)),
    }
}

/// A DPoP proof for a POST to `url` (logincreds mkdpop): a JWT, ES256, of its method, its
/// URL, now and a random ID, its header naming the key's public half, signed with the
/// login session's key (`dpopKey`, PEM of an EC key). Its coordinates are each 32 bytes,
/// as RFC 7518 §6.2.1.2 has them, where the SDK drops their leading zero bytes.
fn dpop(pem: &str, url: &str) -> Result<String, String> {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
    use base64::Engine as _;
    #[derive(serde::Serialize)]
    struct Header<'a> {
        typ: &'a str,
        alg: &'a str,
        jwk: Jwk,
    }
    #[derive(serde::Serialize)]
    struct Jwk {
        kty: &'static str,
        x: String,
        y: String,
        crv: &'static str,
    }
    #[derive(serde::Serialize)]
    struct Payload<'a> {
        jti: &'a str,
        htm: &'a str,
        htu: &'a str,
        iat: u64,
    }
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let said = |e: &dyn std::fmt::Display| format!("the login session's dpopKey: {e}");
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| said(&e))?;
    let key =
        EcdsaKeyPair::from_private_key_der(&ECDSA_P256_SHA256_FIXED_SIGNING, &der).map_err(|e| said(&e))?;
    // The public key, uncompressed: 4, then x and y.
    let public = key.public_key().as_ref();
    let (Some(x), Some(y)) = (public.get(1..33), public.get(33..65)) else {
        return Err(said(&"not a P-256 key"));
    };
    let header = Header {
        typ: "dpop+jwt",
        alg: "ES256",
        jwk: Jwk {
            kty: "EC",
            x: b64(x),
            y: b64(y),
            crv: "P-256",
        },
    };
    let jti = crate::random_uuid().map_err(|e| said(&e))?;
    let iat = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let payload = Payload {
        jti: &jti,
        htm: "POST",
        htu: url,
        iat,
    };
    // Indented as the SDK's json.MarshalIndent writes them.
    let header = serde_json::to_vec_pretty(&header).map_err(|e| said(&e))?;
    let payload = serde_json::to_vec_pretty(&payload).map_err(|e| said(&e))?;
    let message = format!("{}.{}", b64(&header), b64(&payload));
    let signature = key
        .sign(&aws_lc_rs::rand::SystemRandom::new(), message.as_bytes())
        .map_err(|e| said(&e))?;
    Ok(format!("{message}.{}", b64(signature.as_ref())))
}

fn digest_hex(alg: &'static aws_lc_rs::digest::Algorithm, data: &str) -> String {
    sigv4::hex(aws_lc_rs::digest::digest(alg, data.as_bytes()).as_ref())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    /// A request the fake AWS was sent.
    #[derive(Debug)]
    struct Seen {
        method: String,
        target: String,
        headers: BTreeMap<String, String>,
        body: String,
    }

    /// Answers each connection's one request as `answer` says, sent on `seen`, closing
    /// it after, until a request whose method is QUIT.
    fn serve(listener: &TcpListener, answer: &dyn Fn(&Seen) -> (u16, String), seen: &mpsc::Sender<Seen>) {
        for conn in listener.incoming() {
            let mut reader = BufReader::new(conn.unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, target) = (
                parts.next().unwrap().to_string(),
                parts.next().unwrap().to_string(),
            );
            if method == "QUIT" {
                return;
            }
            let mut headers = BTreeMap::new();
            loop {
                let mut field = String::new();
                reader.read_line(&mut field).unwrap();
                let field = field.trim_end();
                if field.is_empty() {
                    break;
                }
                let (k, v) = field.split_once(':').unwrap();
                headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
            }
            let len: usize = headers.get("content-length").map_or(0, |l| l.parse().unwrap());
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let request = Seen {
                method,
                target,
                headers,
                body: String::from_utf8(body).unwrap(),
            };
            let (status, text) = answer(&request);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            seen.send(request).unwrap();
        }
    }

    /// Ends `serve` as it drops, a failing assertion's unwinding too.
    struct Quit(u16);

    impl Drop for Quit {
        fn drop(&mut self) {
            if let Ok(mut conn) = TcpStream::connect(("127.0.0.1", self.0)) {
                let _ = conn.write_all(b"QUIT / HTTP/1.1\r\n\r\n");
            }
        }
    }

    /// Runs `test` with a fake AWS answering as `answer` says on a port of its own.
    fn with_aws(
        answer: &(dyn Fn(&Seen) -> (u16, String) + Sync),
        test: impl FnOnce(u16, &mpsc::Receiver<Seen>),
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, got) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| serve(&listener, answer, &seen));
            let _quit = Quit(port);
            test(port, &got);
        });
    }

    /// An environment of `pairs`, with nothing else: EC2's instance metadata disabled
    /// unless a pair says otherwise.
    fn env_of(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        let mut map: BTreeMap<&str, String> = [("AWS_EC2_METADATA_DISABLED", "true".to_string())].into();
        map.extend(pairs);
        move |k: &str| map.get(k).cloned()
    }

    const ASSUMED: &str = "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ASSUMED</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";

    /// The chain's order: the environment's keys first; a profile AWS_PROFILE
    /// names and no file has refused, one AWS_DEFAULT_PROFILE names left out; a profile's
    /// keys; its role assumed with its source profile's, as STS's form and SigV4 have it.
    #[test]
    fn the_chain_finds_credentials_in_order() {
        let dir = shards_testdir::TempDir::new("aws-chain").unwrap();
        let config = dir.join("config");
        std::fs::write(
            &config,
            "[profile static]\naws_access_key_id = STATIC\naws_secret_access_key = st\n\
             [profile chained]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = static\nrole_session_name = s\n\
             external_id = x/y\nduration_seconds = 3600\n\
             [profile short]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = static\nduration_seconds = 960\n\
             [profile shorter]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = static\nduration_seconds = 959\n\
             [profile mfa]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = static\nmfa_serial = arn:m\n\
             [profile environment]\nrole_arn = arn:aws:iam::1:role/r\ncredential_source = Environment\n",
        )
        .unwrap();
        let files = |more: Vec<(&'static str, String)>| {
            let mut all = vec![
                ("AWS_CONFIG_FILE", config.display().to_string()),
                (
                    "AWS_SHARED_CREDENTIALS_FILE",
                    dir.join("none").display().to_string(),
                ),
            ];
            all.extend(more);
            env_of(all)
        };
        // The environment's keys win over a profile's.
        let env = files(vec![
            ("AWS_ACCESS_KEY_ID", "ENV".into()),
            ("AWS_SECRET_ACCESS_KEY", "e".into()),
            ("AWS_PROFILE", "static".into()),
        ]);
        let found = Chain::new(&env).resolve().unwrap();
        assert_eq!((found.creds.access_key.as_str(), found.expires), ("ENV", None));
        // AWS_PROFILE's must be there; AWS_DEFAULT_PROFILE's need not.
        let env = files(vec![("AWS_PROFILE", "absent".into())]);
        let err = Chain::new(&env).resolve().err().unwrap();
        assert!(err.contains("AWS_PROFILE=absent: no such profile"), "{err}");
        let env = files(vec![("AWS_DEFAULT_PROFILE", "absent".into())]);
        let err = Chain::new(&env).resolve().err().unwrap();
        assert!(err.contains("AWS_EC2_METADATA_DISABLED"), "{err}");
        let env = files(vec![("AWS_PROFILE", "static".into())]);
        assert_eq!(Chain::new(&env).resolve().unwrap().creds.access_key, "STATIC");
        // An MFA code no one can give; the environment's keys, which are not there.
        let env = files(vec![("AWS_PROFILE", "mfa".into())]);
        assert!(Chain::new(&env).resolve().err().unwrap().contains("MFA"));
        let env = files(vec![("AWS_PROFILE", "environment".into())]);
        assert!(
            Chain::new(&env)
                .resolve()
                .err()
                .unwrap()
                .contains("credential_source Environment")
        );
        // A role, assumed with the source profile's keys.
        with_aws(&|_| (200, ASSUMED.into()), |port, seen| {
            let sts = format!("http://127.0.0.1:{port}");
            for (profile, seconds) in [("chained", "3600"), ("short", "960"), ("shorter", "900")] {
                let env = files(vec![
                    ("AWS_PROFILE", profile.into()),
                    ("AWS_ENDPOINT_URL_STS", sts.clone()),
                ]);
                let found = Chain::new(&env).in_region("eu-west-1").resolve().unwrap();
                assert_eq!(found.creds.access_key, "ASSUMED");
                assert_eq!(found.creds.token.as_deref().map(String::as_str), Some("t"));
                assert_eq!(found.expires, rfc3339("2099-01-01T00:00:00Z"));
                let r = seen.recv().unwrap();
                assert_eq!((r.method.as_str(), r.target.as_str()), ("POST", "/"));
                assert_eq!(r.headers["content-type"], FORM);
                let auth = &r.headers["authorization"];
                assert!(
                    auth.starts_with("AWS4-HMAC-SHA256 Credential=STATIC/")
                        && auth.contains("/eu-west-1/sts/aws4_request"),
                    "{auth}"
                );
                let expected = if profile == "chained" {
                    "Action=AssumeRole&DurationSeconds=3600&ExternalId=x%2Fy&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr&RoleSessionName=s&Version=2011-06-15".to_string()
                } else {
                    let session = r
                        .body
                        .split("RoleSessionName=")
                        .nth(1)
                        .unwrap()
                        .split('&')
                        .next()
                        .unwrap();
                    assert!(session.starts_with("aws-go-sdk-"), "{session}");
                    format!(
                        "Action=AssumeRole&DurationSeconds={seconds}&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr&RoleSessionName={session}&Version=2011-06-15"
                    )
                };
                assert_eq!(r.body, expected);
            }
        });
    }

    /// A web identity's role, asked of STS unsigned; the container's endpoint, with its
    /// token's file; EC2's instance metadata, IMDSv2's token first, IMDSv1 where the token
    /// is refused and IMDSv1 is not disabled.
    #[test]
    fn web_identity_container_and_instance_credentials_are_found() {
        let dir = shards_testdir::TempDir::new("aws-chain").unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "tok\n").unwrap();
        let creds = r#"{"Code":"Success","AccessKeyId":"K","SecretAccessKey":"S","Token":"T","Expiration":"2099-01-01T00:00:00Z"}"#;
        let refuse_tokens = std::sync::atomic::AtomicBool::new(false);
        let answer = |r: &Seen| match (r.method.as_str(), r.target.as_str()) {
            ("POST", "/") => (200, ASSUMED.replace("ASSUMED", "WEB")),
            ("GET", "/creds") => (200, creds.to_string()),
            ("PUT", "/latest/api/token") if refuse_tokens.load(std::sync::atomic::Ordering::SeqCst) => {
                (403, String::new())
            }
            ("PUT", "/latest/api/token") => (200, "SESSION".into()),
            ("GET", "/latest/meta-data/iam/security-credentials/") => (200, "role-name\nother\n".into()),
            ("GET", "/latest/meta-data/iam/security-credentials/role-name") => {
                (200, creds.replace("\"K\"", "\"IMDS\""))
            }
            ("GET", "/latest/meta-data/placement/region") => (200, "us-west-2".into()),
            _ => (404, String::new()),
        };
        with_aws(&answer, |port, seen| {
            let at = format!("http://127.0.0.1:{port}");
            let none = dir.join("none").display().to_string();
            let base = |more: Vec<(&'static str, String)>| {
                let mut all = vec![
                    ("AWS_CONFIG_FILE", none.clone()),
                    ("AWS_SHARED_CREDENTIALS_FILE", none.clone()),
                ];
                all.extend(more);
                env_of(all)
            };
            let env = base(vec![
                ("AWS_WEB_IDENTITY_TOKEN_FILE", token_file.display().to_string()),
                ("AWS_ROLE_ARN", "arn:aws:iam::1:role/web".into()),
                ("AWS_ROLE_SESSION_NAME", "me".into()),
                ("AWS_ENDPOINT_URL_STS", at.clone()),
            ]);
            assert_eq!(
                Chain::new(&env)
                    .in_region("us-east-1")
                    .resolve()
                    .unwrap()
                    .creds
                    .access_key,
                "WEB"
            );
            let r = seen.recv().unwrap();
            assert!(!r.headers.contains_key("authorization"));
            assert_eq!(
                r.body,
                "Action=AssumeRoleWithWebIdentity&RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fweb&RoleSessionName=me&Version=2011-06-15&WebIdentityToken=tok"
            );
            // The container's: its token's file read, its line ending left out.
            let env = base(vec![
                ("AWS_CONTAINER_CREDENTIALS_FULL_URI", format!("{at}/creds")),
                (
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
                    token_file.display().to_string(),
                ),
                ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "not this".into()),
            ]);
            let found = Chain::new(&env).resolve().unwrap();
            assert_eq!(
                (found.creds.access_key.as_str(), found.expires),
                ("K", rfc3339("2099-01-01T00:00:00Z"))
            );
            let r = seen.recv().unwrap();
            assert_eq!(
                (r.headers["authorization"].as_str(), r.headers["accept"].as_str()),
                ("tok", "application/json")
            );
            let env = base(vec![(
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "http://192.0.2.1/creds".into(),
            )]);
            assert!(
                Chain::new(&env)
                    .resolve()
                    .err()
                    .unwrap()
                    .contains("only a loopback, ECS or EKS address")
            );
            // EC2's: a session token, the first role's credentials, kept an hour at most.
            let imds = |more: Vec<(&'static str, String)>| {
                let mut all = vec![
                    ("AWS_EC2_METADATA_DISABLED", "false".to_string()),
                    ("AWS_EC2_METADATA_SERVICE_ENDPOINT", format!("{at}/")),
                ];
                all.extend(more);
                base(all)
            };
            let env = imds(vec![]);
            let found = Chain::new(&env).resolve().unwrap();
            assert_eq!(found.creds.access_key, "IMDS");
            assert!(found.expiring(Duration::from_secs(3601)) && !found.expiring(Duration::from_secs(3500)));
            let asked: Vec<Seen> = (0..3).map(|_| seen.recv().unwrap()).collect();
            assert_eq!(asked[0].headers["x-aws-ec2-metadata-token-ttl-seconds"], "300");
            assert!(
                asked[1..]
                    .iter()
                    .all(|r| r.headers["x-aws-ec2-metadata-token"] == "SESSION")
            );
            assert_eq!(Chain::new(&env).instance_region(None).unwrap(), "us-west-2");
            let _ = (seen.recv().unwrap(), seen.recv().unwrap());
            // Tokens refused: IMDSv1, unless it is disabled.
            refuse_tokens.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(Chain::new(&env).resolve().unwrap().creds.access_key, "IMDS");
            let asked: Vec<Seen> = (0..3).map(|_| seen.recv().unwrap()).collect();
            assert!(
                asked[1..]
                    .iter()
                    .all(|r| !r.headers.contains_key("x-aws-ec2-metadata-token"))
            );
            let env = imds(vec![("AWS_EC2_METADATA_V1_DISABLED", "true".into())]);
            assert!(Chain::new(&env).resolve().err().unwrap().contains("403"));
            let env = imds(vec![("AWS_EC2_METADATA_V1_DISABLED", "yes".into())]);
            assert!(
                Chain::new(&env)
                    .resolve()
                    .err()
                    .unwrap()
                    .contains("true or false")
            );
        });
    }

    /// SSO: a legacy profile's token, by its start URL's SHA-1; a session's, by its name's,
    /// refreshed through SSO OIDC once expired and kept so; the role's credentials asked
    /// for with it.
    #[test]
    fn sso_tokens_are_read_from_the_cache_and_refreshed() {
        let home = shards_testdir::TempDir::new("aws-sso").unwrap();
        let cache = home.join(".aws/sso/cache");
        std::fs::create_dir_all(&cache).unwrap();
        let config = home.join(".aws/config");
        std::fs::write(
            &config,
            "[profile legacy]\nsso_start_url = https://start\nsso_region = us-east-1\nsso_account_id = 123\nsso_role_name = dev\n\
             [profile session]\nsso_session = mine\nsso_account_id = 123\nsso_role_name = dev\n\
             [profile partial]\nsso_session = mine\n\
             [sso-session mine]\nsso_region = eu-west-1\nsso_start_url = https://start\n",
        )
        .unwrap();
        let legacy = cache.join(format!(
            "{}.json",
            digest_hex(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, "https://start")
        ));
        std::fs::write(
            &legacy,
            r#"{"accessToken":"LEGACY","expiresAt":"2099-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let session = cache.join(format!(
            "{}.json",
            digest_hex(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, "mine")
        ));
        std::fs::write(
            &session,
            r#"{"accessToken":"OLD","expiresAt":"2001-01-01T00:00:00Z","clientId":"c","clientSecret":"cs","refreshToken":"R1","startUrl":"https://start"}"#,
        )
        .unwrap();
        let answer = |r: &Seen| match r.method.as_str() {
            "POST" => (
                200,
                r#"{"accessToken":"NEW","expiresIn":3600,"refreshToken":"R2","tokenType":"Bearer"}"#.into(),
            ),
            _ => (
                200,
                format!(
                    r#"{{"roleCredentials":{{"accessKeyId":"SSO-{}","secretAccessKey":"s","sessionToken":"t","expiration":4070908800000}}}}"#,
                    r.headers["x-amz-sso_bearer_token"]
                ),
            ),
        };
        with_aws(&answer, |port, seen| {
            let at = format!("http://127.0.0.1:{port}");
            let env = |profile: &'static str| {
                env_of(vec![
                    ("HOME", home.display().to_string()),
                    ("AWS_PROFILE", profile.into()),
                    ("AWS_ENDPOINT_URL_SSO", at.clone()),
                    ("AWS_ENDPOINT_URL_SSO_OIDC", at.clone()),
                ])
            };
            let legacy_env = env("legacy");
            let found = Chain::new(&legacy_env).resolve().unwrap();
            assert_eq!(found.creds.access_key, "SSO-LEGACY");
            assert_eq!(found.expires, rfc3339("2099-01-01T00:00:00Z"));
            let r = seen.recv().unwrap();
            assert_eq!(r.target, "/federation/credentials?account_id=123&role_name=dev");
            // A session's expired token: refreshed, and kept.
            let session_env = env("session");
            assert_eq!(
                Chain::new(&session_env).resolve().unwrap().creds.access_key,
                "SSO-NEW"
            );
            let refresh = seen.recv().unwrap();
            assert_eq!(refresh.target, "/token");
            let asked: serde_json::Value = serde_json::from_str(&refresh.body).unwrap();
            assert_eq!(
                asked,
                serde_json::json!({"clientId": "c", "clientSecret": "cs", "grantType": "refresh_token", "refreshToken": "R1"})
            );
            let _ = seen.recv().unwrap();
            let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&session).unwrap()).unwrap();
            assert_eq!(
                (kept["accessToken"].as_str(), kept["refreshToken"].as_str()),
                (Some("NEW"), Some("R2"))
            );
            assert_eq!(kept["startUrl"], "https://start", "what the CLI keeps, kept");
            assert!(rfc3339(kept["expiresAt"].as_str().unwrap()).unwrap() > SystemTime::now());
            // A session's profile without its account and role.
            let partial = env("partial");
            assert!(
                Chain::new(&partial)
                    .resolve()
                    .err()
                    .unwrap()
                    .contains("sso_account_id, sso_role_name")
            );
            // A legacy token expired: sign in again.
            std::fs::write(
                &legacy,
                r#"{"accessToken":"LEGACY","expiresAt":"2001-01-01T00:00:00Z"}"#,
            )
            .unwrap();
            assert!(
                Chain::new(&legacy_env)
                    .resolve()
                    .err()
                    .unwrap()
                    .contains("aws sso login")
            );
        });
    }

    /// A login session's credentials: those kept, while they last; then refreshed through
    /// sign-in with a DPoP proof its key signs, and kept so.
    #[test]
    fn login_sessions_are_refreshed_with_a_dpop_proof() {
        use aws_lc_rs::encoding::AsDer as _;
        use aws_lc_rs::signature::{
            ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _,
            UnparsedPublicKey,
        };
        use base64::Engine as _;
        let dir = shards_testdir::TempDir::new("aws-login").unwrap();
        let config = dir.join("config");
        std::fs::write(&config, "[default]\nlogin_session = arn:aws:iam::0:user/me\n").unwrap();
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).unwrap();
        let der = key.private_key().as_der().unwrap();
        let pem = format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der.as_ref())
        );
        let cached = |expires: &str| {
            serde_json::json!({
                "accessToken": {"accessKeyId": "KEPT", "secretAccessKey": "s", "sessionToken": "t", "accountId": "0", "expiresAt": expires},
                "tokenType": "aws_sigv4", "refreshToken": "R1", "identityToken": "i", "clientId": "arn:client", "dpopKey": pem,
            })
        };
        let cache = dir.join(format!(
            "{}.json",
            digest_hex(&aws_lc_rs::digest::SHA256, "arn:aws:iam::0:user/me")
        ));
        std::fs::write(&cache, cached("2099-01-01T00:00:00Z").to_string()).unwrap();
        let answer = |_: &Seen| {
            (
                200,
                r#"{"accessToken":{"accessKeyId":"FRESH","secretAccessKey":"s2","sessionToken":"t2"},"expiresIn":900,"refreshToken":"R2","tokenType":"aws_sigv4"}"#.to_string(),
            )
        };
        with_aws(&answer, |port, seen| {
            let env = env_of(vec![
                ("AWS_CONFIG_FILE", config.display().to_string()),
                (
                    "AWS_SHARED_CREDENTIALS_FILE",
                    dir.join("none").display().to_string(),
                ),
                ("AWS_LOGIN_CACHE_DIRECTORY", dir.display().to_string()),
                ("AWS_ENDPOINT_URL_SIGNIN", format!("http://127.0.0.1:{port}")),
            ]);
            assert_eq!(
                Chain::new(&env)
                    .in_region("us-east-1")
                    .resolve()
                    .unwrap()
                    .creds
                    .access_key,
                "KEPT"
            );
            std::fs::write(&cache, cached("2001-01-01T00:00:00Z").to_string()).unwrap();
            let found = Chain::new(&env).in_region("us-east-1").resolve().unwrap();
            assert_eq!(found.creds.access_key, "FRESH");
            assert!(found.expiring(Duration::from_secs(901)) && !found.expiring(Duration::from_secs(800)));
            let r = seen.recv().unwrap();
            assert_eq!((r.method.as_str(), r.target.as_str()), ("POST", "/v1/token"));
            let asked: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(
                asked,
                serde_json::json!({"clientId": "arn:client", "grantType": "refresh_token", "refreshToken": "R1"})
            );
            // The proof: its header names the key, which signed it, for this request.
            let proof = &r.headers["dpop"];
            let parts: Vec<&str> = proof.split('.').collect();
            let decode = |p: &str| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(p)
                    .unwrap()
            };
            let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
            assert_eq!(
                (
                    header["typ"].as_str(),
                    header["alg"].as_str(),
                    header["jwk"]["crv"].as_str()
                ),
                (Some("dpop+jwt"), Some("ES256"), Some("P-256"))
            );
            assert_eq!(payload["htu"], format!("http://127.0.0.1:{port}/v1/token"));
            assert_eq!(
                (payload["htm"].as_str(), payload["jti"].as_str().unwrap().len()),
                (Some("POST"), 36)
            );
            let point = [
                vec![4u8],
                decode(header["jwk"]["x"].as_str().unwrap()),
                decode(header["jwk"]["y"].as_str().unwrap()),
            ]
            .concat();
            assert_eq!(point, key.public_key().as_ref());
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
                .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &decode(parts[2]))
                .unwrap();
            let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
            assert_eq!(
                (
                    kept["accessToken"]["accessKeyId"].as_str(),
                    kept["refreshToken"].as_str()
                ),
                (Some("FRESH"), Some("R2"))
            );
            assert_eq!(kept["identityToken"], "i", "what the CLI keeps, kept");
        });
    }

    /// A credential process's output, run in the chain's environment; refused for a
    /// version not 1, or as it fails.
    #[cfg(unix)]
    #[test]
    fn credential_processes_give_their_credentials() {
        let dir = shards_testdir::TempDir::new("aws-process").unwrap();
        let config = dir.join("config");
        std::fs::write(
            &config,
            "[profile ok]\ncredential_process = printf '{\"Version\": 1, \"AccessKeyId\": \"%s\", \"SecretAccessKey\": \"s\", \"Expiration\": \"2099-01-01T00:00:00Z\"}' \"$AWS_MARK\"\n\
             [profile two]\ncredential_process = printf '{\"Version\": 2, \"AccessKeyId\": \"a\", \"SecretAccessKey\": \"s\"}'\n\
             [profile fails]\ncredential_process = echo no >&2; exit 3\n",
        )
        .unwrap();
        let vars = vec![
            format!("AWS_CONFIG_FILE={}", config.display()),
            format!("AWS_SHARED_CREDENTIALS_FILE={}", dir.join("none").display()),
            "AWS_MARK=listed".to_string(),
            "AWS_EC2_METADATA_DISABLED=true".to_string(),
        ];
        let run = |profile: &str| {
            let mut vars = vars.clone();
            vars.push(format!("AWS_PROFILE={profile}"));
            let env = |k: &str| shards_ipc::env_value(&vars, k);
            Chain::new(&env).listing(&vars).resolve()
        };
        let found = run("ok").unwrap();
        assert_eq!(
            (found.creds.access_key.as_str(), found.expires),
            ("listed", rfc3339("2099-01-01T00:00:00Z"))
        );
        assert!(run("two").err().unwrap().contains("Version is not 1"));
        assert!(run("fails").err().unwrap().contains("exited 3"));
    }

    /// Over plain HTTP, a full URI's host is to be loopback or the ECS or EKS agent's.
    #[test]
    fn full_uris_reach_their_agents_alone() {
        for (host, allowed) in [
            ("127.0.0.1", true),
            ("[::1]", true),
            ("[::ffff:127.0.0.1]", true),
            ("169.254.170.2", true),
            ("169.254.170.23", true),
            ("[fd00:ec2::23]", true),
            ("localhost", true),
            ("169.254.169.254", false),
            ("10.0.0.1", false),
            ("[fd00:ec2::254]", false),
        ] {
            assert_eq!(allowed_host(host).unwrap(), allowed, "{host}");
        }
    }
}
