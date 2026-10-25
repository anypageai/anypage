//! Google Cloud: OAuth access tokens from the `gcp` secret, and Cloud
//! Storage through its JSON API.
//!
//! The secret is one of
//! * a service-account key file (`"type": "service_account"`): a JWT signed
//!   RS256 with its private key is exchanged at its `token_uri`
//!   (RFC 7523 JWT bearer grant);
//! * an `authorized_user` file (gcloud application-default credentials):
//!   its refresh token is exchanged;
//! * a raw access token.
//!
//! Tokens are cached until a minute before they expire, keyed by a hash of
//! the secret, the scope and the subject. The token endpoint is checked
//! against the page's `capabilities.net` like every other URL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anypage_runtime::{Data, Grants};
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::{json, Value as Json};

use crate::net::{self, Redirects, Request};
use crate::util::{b64url, build_query, encode_strict, pem_block, sha256, unix_now};

pub const SCOPE_READ: &str = "https://www.googleapis.com/auth/devstorage.read_only";
pub const SCOPE_WRITE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
pub const SCOPE_CLOUD: &str = "https://www.googleapis.com/auth/cloud-platform";
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
pub const STORAGE: &str = "https://storage.googleapis.com";

pub enum Credential {
    Token(String),
    ServiceAccount(Box<ServiceAccount>),
    AuthorizedUser { client_id: String, client_secret: String, refresh_token: String, token_uri: String },
}

/// Shows the kind and the account, never tokens or keys.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Token(_) => write!(f, "Credential::Token(hidden)"),
            Credential::ServiceAccount(sa) => write!(f, "Credential::{sa:?}"),
            Credential::AuthorizedUser { client_id, .. } => write!(f, "Credential::AuthorizedUser({client_id}, secrets hidden)"),
        }
    }
}

pub struct ServiceAccount {
    pub client_email: String,
    pub private_key_id: Option<String>,
    pub token_uri: String,
    key: RsaKeyPair,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServiceAccount({}, key redacted)", self.client_email)
    }
}

/// Parse the `gcp` secret. Messages never include key material.
pub fn parse_credential(secret: &str) -> Result<Credential, String> {
    let s = secret.trim();
    if !s.starts_with('{') {
        if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("the gcp secret is neither a credentials JSON file nor an access token".into());
        }
        return Ok(Credential::Token(s.to_string()));
    }
    let j: Json = serde_json::from_str(s).map_err(|_| "the gcp secret is not valid JSON".to_string())?;
    let get = |k: &str| j.get(k).and_then(Json::as_str).map(str::to_string).filter(|v| !v.is_empty());
    match get("type").as_deref() {
        Some("service_account") => {
            let client_email = get("client_email").ok_or("the service-account key has no client_email")?;
            let pem = get("private_key").ok_or("the service-account key has no private_key")?;
            let key = if let Some(der) = pem_block(&pem, "PRIVATE KEY") {
                RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("the service-account private_key is not a usable RSA key ({e})"))?
            } else if let Some(der) = pem_block(&pem, "RSA PRIVATE KEY") {
                RsaKeyPair::from_der(&der).map_err(|e| format!("the service-account private_key is not a usable RSA key ({e})"))?
            } else {
                return Err("the service-account private_key is not a PEM private key".into());
            };
            Ok(Credential::ServiceAccount(Box::new(ServiceAccount { client_email, private_key_id: get("private_key_id"), token_uri: get("token_uri").unwrap_or_else(|| DEFAULT_TOKEN_URI.into()), key })))
        }
        Some("authorized_user") => Ok(Credential::AuthorizedUser {
            client_id: get("client_id").ok_or("the authorized_user credentials have no client_id")?,
            client_secret: get("client_secret").ok_or("the authorized_user credentials have no client_secret")?,
            refresh_token: get("refresh_token").ok_or("the authorized_user credentials have no refresh_token")?,
            token_uri: get("token_uri").unwrap_or_else(|| DEFAULT_TOKEN_URI.into()),
        }),
        _ => match get("access_token").or_else(|| get("value")) {
            Some(t) => Ok(Credential::Token(t)),
            None => Err("the gcp secret must be a service_account or authorized_user JSON file, or an access token".into()),
        },
    }
}

/// Where a Google token from `cred` may be sent when its secret names no
/// `"hosts"`: Google APIs, and the host of the key file's own token
/// endpoint (an emulator or test setup the key's owner pointed it at).
pub fn token_hosts(cred: &Credential) -> Vec<String> {
    let mut v = vec!["googleapis.com".to_string(), "*.googleapis.com".to_string()];
    let uri = match cred {
        Credential::Token(_) => None,
        Credential::ServiceAccount(sa) => Some(sa.token_uri.as_str()),
        Credential::AuthorizedUser { token_uri, .. } => Some(token_uri.as_str()),
    };
    if let Some((host, _)) = uri.and_then(anypage_runtime::natives::url_host) {
        let host = host.to_ascii_lowercase();
        if !host.is_empty() && host != "googleapis.com" && !host.ends_with(".googleapis.com") {
            v.push(host);
        }
    }
    v
}

/// An RS256 JWT for the OAuth JWT-bearer grant.
pub fn make_jwt(sa: &ServiceAccount, scope: &str, subject: Option<&str>, now: u64) -> Result<String, String> {
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(kid) = &sa.private_key_id {
        header["kid"] = json!(kid);
    }
    let mut claims = json!({ "iss": sa.client_email, "scope": scope, "aud": sa.token_uri, "iat": now, "exp": now + 3600 });
    if let Some(sub) = subject {
        claims["sub"] = json!(sub);
    }
    let input = format!("{}.{}", b64url(header.to_string().as_bytes()), b64url(claims.to_string().as_bytes()));
    let mut sig = vec![0u8; sa.key.public().modulus_len()];
    sa.key.sign(&RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig).map_err(|_| "signing the service-account JWT failed".to_string())?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

struct Cached {
    token: String,
    until: Instant,
}

fn cache() -> &'static Mutex<HashMap<[u8; 32], Cached>> {
    static C: OnceLock<Mutex<HashMap<[u8; 32], Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An access token for `scope`, from the cache or the token endpoint.
pub fn access_token(grants: &Grants, secret: &str, scope: &str, subject: Option<&str>, cancel: &dyn Fn() -> bool) -> Result<String, String> {
    let cred = parse_credential(secret)?;
    if let Credential::Token(t) = &cred {
        return Ok(t.clone());
    }
    let key = sha256(format!("{secret}\n{scope}\n{}", subject.unwrap_or("")).as_bytes());
    {
        let c = cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(t) = c.get(&key).filter(|t| t.until > Instant::now()) {
            return Ok(t.token.clone());
        }
    }
    let (token_uri, form) = match &cred {
        Credential::Token(t) => return Ok(t.clone()),
        Credential::ServiceAccount(sa) => {
            let jwt = make_jwt(sa, scope, subject, unix_now())?;
            (sa.token_uri.clone(), vec![("grant_type".to_string(), "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string()), ("assertion".to_string(), jwt)])
        }
        Credential::AuthorizedUser { client_id, client_secret, refresh_token, token_uri } => (
            token_uri.clone(),
            vec![
                ("grant_type".to_string(), "refresh_token".to_string()),
                ("client_id".to_string(), client_id.clone()),
                ("client_secret".to_string(), client_secret.clone()),
                ("refresh_token".to_string(), refresh_token.clone()),
            ],
        ),
    };
    let mut req = Request::new("POST", &token_uri).header("Content-Type", "application/x-www-form-urlencoded").header("Accept", "application/json");
    let body = build_query(&form);
    req.redact = form.iter().filter(|(k, _)| k != "grant_type").map(|(_, v)| v.clone()).collect();
    req.redact.push(secret.to_string());
    req.body = Some(Arc::new(body.into_bytes()));
    req.redirects = Redirects::Return;
    let resp = net::send(grants, &req, cancel).map_err(|e| format!("getting a Google access token: {e}"))?;
    let status = resp.status;
    let bytes = resp.read_all(1 << 20, cancel, &|_| {})?;
    if !(200..300).contains(&status) {
        return Err(format!("getting a Google access token: HTTP {status}: {}", net::error_text(&bytes)));
    }
    let j: Json = serde_json::from_slice(&bytes).map_err(|_| "the token endpoint did not answer with JSON".to_string())?;
    let token = j.get("access_token").and_then(Json::as_str).filter(|t| !t.is_empty()).ok_or("the token endpoint sent no access_token")?.to_string();
    let ttl = j.get("expires_in").and_then(Json::as_u64).unwrap_or(3600);
    let until = Instant::now() + Duration::from_secs(ttl.saturating_sub(60));
    let mut c = cache().lock().unwrap_or_else(|p| p.into_inner());
    c.retain(|_, v| v.until > Instant::now());
    if c.len() >= 64 {
        c.clear();
    }
    c.insert(key, Cached { token: token.clone(), until });
    Ok(token)
}

// ---------------------------------------------------------------- Cloud Storage

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Get,
    Put,
    List,
    Delete,
    Head,
}

impl Op {
    pub fn parse(s: &str) -> Result<Op, String> {
        Ok(match s {
            "get" => Op::Get,
            "put" => Op::Put,
            "list" => Op::List,
            "delete" => Op::Delete,
            "head" => Op::Head,
            _ => return Err(format!("unknown op \"{s}\" (get, put, list, delete, head)")),
        })
    }
}

fn check_bucket(b: &str) -> Result<(), String> {
    if b.is_empty() || b.len() > 222 || !b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')) {
        return Err(format!("\"{b}\" is not a valid bucket name"));
    }
    Ok(())
}

/// The HTTP request for a GCS operation (without the token).
pub fn build(op: Op, req: &Data) -> Result<Request, String> {
    let bucket = req.get("bucket").as_str().ok_or("gcs needs a bucket")?;
    check_bucket(bucket)?;
    let endpoint = req.get("endpoint").as_str().unwrap_or(STORAGE).trim_end_matches('/');
    let object = req.get("object").as_str().or_else(|| req.get("key").as_str()).unwrap_or("");
    if op != Op::List && object.is_empty() {
        return Err("gcs needs an object name".into());
    }
    if object.len() > 1024 {
        return Err("object names are at most 1024 bytes".into());
    }
    let b = encode_strict(bucket);
    let o = encode_strict(object);
    let r = match op {
        Op::Get => Request::new("GET", &format!("{endpoint}/storage/v1/b/{b}/o/{o}?alt=media")),
        Op::Head => Request::new("GET", &format!("{endpoint}/storage/v1/b/{b}/o/{o}")),
        Op::Delete => Request::new("DELETE", &format!("{endpoint}/storage/v1/b/{b}/o/{o}")),
        Op::List => {
            let mut q: Vec<(String, String)> = Vec::new();
            for (k, name) in [("prefix", "prefix"), ("delimiter", "delimiter"), ("pageToken", "page_token"), ("startOffset", "start_offset")] {
                if let Some(v) = req.get(name).as_str().filter(|v| !v.is_empty()) {
                    q.push((k.to_string(), v.to_string()));
                }
            }
            if let Some(n) = req.get("max").num().or_else(|| req.get("max_keys").num()) {
                q.push(("maxResults".into(), (n.clamp(1.0, 5000.0) as u64).to_string()));
            }
            let qs = build_query(&q);
            Request::new("GET", &format!("{endpoint}/storage/v1/b/{b}/o{}{qs}", if qs.is_empty() { "" } else { "?" }))
        }
        Op::Put => {
            let body = req.get("body").as_bytes().ok_or("gcs put needs a body (bytes or a string)")?.to_vec();
            let ctype = req.get("content_type").as_str().unwrap_or("application/octet-stream");
            Request::new("POST", &format!("{endpoint}/upload/storage/v1/b/{b}/o?uploadType=media&name={o}")).header("Content-Type", ctype).body(body)
        }
    };
    let mut r = r;
    r.redirects = Redirects::Return;
    Ok(r)
}

pub fn scope_for(op: Op, req: &Data) -> String {
    match req.get("scope").as_str() {
        Some(s) => s.to_string(),
        None if matches!(op, Op::Put | Op::Delete) => SCOPE_WRITE.into(),
        None => SCOPE_READ.into(),
    }
}

fn object_meta(j: &Json) -> Data {
    let s = |k: &str| j.get(k).and_then(Json::as_str).map(Data::str).unwrap_or_default();
    let size = j.get("size").and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64())).map(Data::Num).unwrap_or_default();
    Data::map([
        ("name", s("name")),
        ("bucket", s("bucket")),
        ("size", size),
        ("content_type", s("contentType")),
        ("updated", s("updated")),
        ("md5", s("md5Hash")),
        ("crc32c", s("crc32c")),
        ("etag", s("etag")),
        ("generation", s("generation")),
        ("metadata", j.get("metadata").map(Data::from_json).unwrap_or_default()),
    ])
}

/// Run one GCS operation.
pub fn run(grants: &Grants, req: &Data, max: usize, cancel: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Data, String> {
    let op = Op::parse(req.get("op").as_str().unwrap_or("get"))?;
    let secret_name = req.get("secret").as_str().unwrap_or("gcp");
    let mut r = build(op, req)?;
    let body_len = r.body.as_ref().map_or(0, |b| b.len());
    r.timeouts = net::Timeouts::for_job(req, max.saturating_add(body_len))?;
    // the target first: no JWT is signed and no token fetched for a request
    // this page may not make, or for a host the token is not bound to
    let url = net::check(grants, &r.url)?;
    grants.check_secret_url(secret_name, &url.as_string())?;
    let secret = grants.secret(secret_name)?;
    let m = crate::auth::material(&secret)?;
    let cred = parse_credential(&secret)?;
    let hosts = m.hosts.clone().unwrap_or_else(|| token_hosts(&cred));
    crate::auth::check_hosts(secret_name, &hosts, &url)?;
    let token = access_token(grants, &secret, &scope_for(op, req), req.get("subject").as_str(), cancel)?;
    r.secret_headers.push(("Authorization".into(), format!("Bearer {token}")));
    r.redact.push(secret.clone());
    r.redact.extend(m.secret_parts);
    let resp = net::send(grants, &r, cancel).map_err(|e| format!("gcs {}: {e}", op_name(op)))?;
    // what the page gets back is scrubbed of the token and the key: the
    // body here, the one header used below
    let scrub = resp.scrubber();
    let status = resp.status;
    let ctype = resp.content_type();
    let ctype = scrub.text(&ctype).unwrap_or(ctype);
    if op == Op::Head && status == 404 {
        return Ok(Data::map([("exists", Data::Bool(false)), ("status", Data::Num(404.0))]));
    }
    let bytes = scrub.bytes(resp.read_all(max, cancel, progress)?);
    if !(200..300).contains(&status) {
        return Err(format!("gcs {}: HTTP {status}: {}", op_name(op), net::error_text(&bytes)));
    }
    let json = || serde_json::from_slice::<Json>(&bytes).map_err(|_| format!("gcs {}: the answer is not JSON", op_name(op)));
    Ok(match op {
        Op::Get => {
            let mut m = vec![("status", Data::Num(status as f64)), ("content_type", Data::str(&ctype)), ("size", Data::Num(bytes.len() as f64))];
            if let Ok(t) = std::str::from_utf8(&bytes) {
                m.push(("text", Data::str(t)));
            }
            m.push(("body", Data::bytes(bytes)));
            Data::map(m)
        }
        Op::Head | Op::Put => {
            let j = json()?;
            let mut d = object_meta(&j);
            if let Data::Map(m) = &mut d {
                let m = Arc::make_mut(m);
                m.insert(Arc::from("status"), Data::Num(status as f64));
                m.insert(Arc::from("exists"), Data::Bool(true));
            }
            d
        }
        Op::Delete => Data::map([("status", Data::Num(status as f64)), ("deleted", Data::Bool(true))]),
        Op::List => {
            let j = json()?;
            let items = j.get("items").and_then(Json::as_array).map(|a| a.iter().map(object_meta).collect()).unwrap_or_default();
            let prefixes = j.get("prefixes").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_str).map(Data::str).collect()).unwrap_or_default();
            Data::map([
                ("objects", Data::list(items)),
                ("prefixes", Data::list(prefixes)),
                ("next_page_token", j.get("nextPageToken").and_then(Json::as_str).map(Data::str).unwrap_or_default()),
                ("status", Data::Num(status as f64)),
            ])
        }
    })
}

fn op_name(op: Op) -> &'static str {
    match op {
        Op::Get => "get",
        Op::Put => "put",
        Op::List => "list",
        Op::Delete => "delete",
        Op::Head => "head",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials() {
        assert!(matches!(parse_credential("ya29.token"), Ok(Credential::Token(t)) if t == "ya29.token"));
        assert!(matches!(parse_credential(r#"{"access_token":"t"}"#), Ok(Credential::Token(_))));
        let e = parse_credential(r#"{"type":"service_account","client_email":"a@b","private_key":"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n"}"#).unwrap_err();
        assert!(e.contains("not a usable RSA key"), "{e}");
        assert!(!e.contains("AAAA"));
        assert!(parse_credential("two words").is_err());
        assert!(matches!(parse_credential(r#"{"type":"authorized_user","client_id":"i","client_secret":"s","refresh_token":"r"}"#), Ok(Credential::AuthorizedUser { .. })));
    }

    #[test]
    fn urls() {
        let req = Data::map([("bucket", Data::str("my-bucket")), ("object", Data::str("dir/a b.txt"))]);
        assert_eq!(build(Op::Get, &req).unwrap().url, "https://storage.googleapis.com/storage/v1/b/my-bucket/o/dir%2Fa%20b.txt?alt=media");
        let req = Data::map([("bucket", Data::str("b")), ("object", Data::str("x")), ("body", Data::str("hi")), ("content_type", Data::str("text/plain"))]);
        let r = build(Op::Put, &req).unwrap();
        assert_eq!(r.url, "https://storage.googleapis.com/upload/storage/v1/b/b/o?uploadType=media&name=x");
        assert_eq!(r.body.as_deref().map(Vec::as_slice), Some(&b"hi"[..]));
        let req = Data::map([("bucket", Data::str("b")), ("prefix", Data::str("p/")), ("max", Data::Num(10.0))]);
        assert_eq!(build(Op::List, &req).unwrap().url, "https://storage.googleapis.com/storage/v1/b/b/o?prefix=p%2F&maxResults=10");
        assert!(build(Op::Get, &Data::map([("bucket", Data::str("Bad/Name")), ("object", Data::str("x"))])).is_err());
        assert!(build(Op::Get, &Data::map([("bucket", Data::str("b"))])).is_err());
    }
}
