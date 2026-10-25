//! Credentials for generic requests: a secret named by the page, applied by
//! the host on the page's behalf. The page never sees the value.
//!
//! `auth` modes: `"bearer"` (default; `api-key` for Azure OpenAI hosts),
//! `"api-key"`, `"x-api-key"`, `"basic"` (secret `user:password`),
//! `"header:<Name>"`, `"aws"` (SigV4; `region`, `service`), `"gcp"`
//! (OAuth token from the `gcp` secret; `scope`), `"none"`.
//!
//! # Binding a secret to hosts
//!
//! Until the runtime binds secrets to hosts (CORE_REQUESTS.md #2), the
//! secret's owner can do it in the value: a JSON secret with
//! `"hosts": ["api.openai.com", "*.example.com", "localhost:8080"]` is only
//! ever sent (or used to sign or mint tokens for requests) to those hosts. An
//! opaque key then goes in `"value"`: `{"value": "sk-...", "hosts": [...]}`.
//! Google tokens are bound by default to `*.googleapis.com` and the key
//! file's own token endpoint host. The URL is always checked against the
//! page's capabilities before any credential is read or derived.

use anypage_runtime::{Caps, Data, Grants};

use crate::net::{self, CheckedUrl, Redirects, Request};
use crate::sigv4::{self, Credentials, Options, Scope};
use crate::util::{b64, parse_query, percent_decode, sha256_hex, unix_now, Scrubber};

/// What a secret's value holds.
#[derive(Clone)]
pub struct Material {
    /// The credential: the `"value"` field of a JSON secret that has one,
    /// else the whole (trimmed) value.
    pub value: String,
    /// The hosts the owner bound the secret to (`"hosts"`), if any.
    pub hosts: Option<Vec<String>>,
    /// The parts that must never reach a page (keys, tokens, passwords).
    pub secret_parts: Vec<String>,
}

/// Fields of JSON secrets that hold secret material.
const SECRET_FIELDS: [&str; 13] = ["value", "secret_access_key", "SecretAccessKey", "session_token", "SessionToken", "private_key", "client_secret", "refresh_token", "access_token", "api_key", "key", "token", "password"];

/// Read a secret's value. Never puts the value in an error message.
pub fn material(raw: &str) -> Result<Material, String> {
    let trimmed = raw.trim();
    let mut m = Material { value: trimmed.to_string(), hosts: None, secret_parts: vec![trimmed.to_string()] };
    if !trimmed.starts_with('{') {
        return Ok(m);
    }
    let Ok(serde_json::Value::Object(j)) = serde_json::from_str::<serde_json::Value>(trimmed) else { return Ok(m) };
    if let Some(h) = j.get("hosts") {
        let hosts: Option<Vec<String>> = h.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
        match hosts {
            Some(v) if !v.is_empty() && v.len() == h.as_array().map_or(0, Vec::len) => m.hosts = Some(v),
            _ => return Err("the secret's \"hosts\" must be a non-empty list of host patterns".into()),
        }
    }
    if let Some(v) = j.get("value").and_then(|v| v.as_str()) {
        m.value = v.trim().to_string();
    }
    let nested = j.get("Credentials").and_then(|c| c.as_object());
    for obj in std::iter::once(&j).chain(nested) {
        for f in SECRET_FIELDS {
            if let Some(s) = obj.get(f).and_then(|v| v.as_str()) {
                m.secret_parts.push(s.trim().to_string());
                // PEM keys: each base64 line on its own too
                m.secret_parts.extend(s.lines().map(str::trim).filter(|l| l.len() >= 16 && !l.starts_with("-----")).map(str::to_string));
            }
        }
    }
    Ok(m)
}

/// Is `url` one of `hosts` (patterns as in `capabilities.net`)?
pub fn check_hosts(name: &str, hosts: &[String], url: &CheckedUrl) -> Result<(), String> {
    let bound = Grants::new("secret-binding", Caps { net: hosts.to_vec(), ..Caps::default() });
    bound.check_url(&url.as_string()).map_err(|_| format!("secret \"{name}\" is bound to {} and may not be sent to {}", hosts.join(", "), url.host))
}

/// Check the owner's binding of secret `name`, if it has one.
pub fn check_binding(name: &str, m: &Material, url: &CheckedUrl) -> Result<(), String> {
    match &m.hosts {
        Some(h) => check_hosts(name, h, url),
        None => Ok(()),
    }
}

/// Page-supplied headers (`{ ["X-Name"] = "value" }`), validated.
pub fn page_headers(d: &Data) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    match d {
        Data::Nil => {}
        Data::Map(_) => {
            for (k, v) in d.entries() {
                net::check_header_name(k)?;
                let v = match v {
                    Data::Str(s) => s.to_string(),
                    Data::Num(n) => Data::Num(*n).to_json().to_string(),
                    Data::Bool(b) => b.to_string(),
                    _ => return Err(format!("header \"{k}\" must be a string")),
                };
                if v.contains(['\r', '\n', '\0']) {
                    return Err(format!("header \"{k}\" contains a line break"));
                }
                out.push((k.to_string(), v));
            }
        }
        _ => return Err("headers must be a table { name = value }".into()),
    }
    if out.len() > net::MAX_HEADERS {
        return Err(format!("more than {} headers", net::MAX_HEADERS));
    }
    Ok(out)
}

/// The values to scrub from messages for the secret `name`: the secret, and
/// when it is JSON (aws, gcp) every string field in it that looks like a key.
pub fn secret_values(grants: &Grants, name: Option<&str>) -> Vec<String> {
    let Some(v) = name.and_then(|n| grants.secret(n).ok()) else { return Vec::new() };
    let mut out = Vec::new();
    if let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&v) {
        for x in m.values() {
            if let Some(s) = x.as_str().filter(|s| s.len() >= 8) {
                out.push(s.to_string());
                // PEM keys: each base64 line on its own too
                out.extend(s.lines().filter(|l| l.len() >= 16 && !l.starts_with("-----")).map(str::to_string));
            }
        }
    }
    out.push(v.trim().to_string());
    out.push(v);
    out
}

/// Scrub `secret`'s values, and their base64 forms, from an error message.
pub fn scrub(grants: &Grants, name: Option<&str>, msg: String) -> String {
    let vals = secret_values(grants, name);
    if vals.is_empty() {
        return msg;
    }
    let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
    let msg = crate::util::redact(&msg, &refs);
    Scrubber::new(&vals).text(&msg).unwrap_or(msg)
}

fn is_azure(url: &str) -> bool {
    anypage_runtime::natives::url_host(url).is_some_and(|(h, _)| {
        let h = h.to_ascii_lowercase();
        h.ends_with(".openai.azure.com") || h.ends_with(".cognitiveservices.azure.com") || h.ends_with(".services.ai.azure.com")
    })
}

/// Add credentials to `req`. `secret` is the secret's name (declared in
/// `capabilities.secrets`); `opts` holds `region`/`service` (aws),
/// `scope`/`subject` (gcp).
pub fn apply(grants: &Grants, req: &mut Request, secret: Option<&str>, mode: Option<&str>, opts: &Data, cancel: &dyn Fn() -> bool) -> Result<(), String> {
    let mode = mode.unwrap_or(if secret.is_some() && is_azure(&req.url) { "api-key" } else { "bearer" });
    if mode == "none" {
        return Ok(());
    }
    let Some(name) = secret else {
        if mode == "bearer" {
            return Ok(());
        }
        return Err(format!("auth \"{mode}\" needs a secret name"));
    };
    if !matches!(mode, "bearer" | "api-key" | "x-api-key" | "basic" | "aws" | "gcp") && !mode.starts_with("header:") {
        return Err(format!("unknown auth \"{mode}\" (bearer, api-key, x-api-key, basic, header:<Name>, aws, gcp, none)"));
    }
    // the target first: no secret is read, signed with or exchanged for a
    // request this page may not make
    let url = net::check(grants, &req.url)?;
    // page.json may bind the secret to hosts: {"name": .., "hosts": [..]}
    grants.check_secret_url(name, &url.as_string())?;
    let value = grants.secret(name)?;
    let m = material(&value)?;
    req.redact.push(value.clone());
    req.redact.extend(m.secret_parts.iter().cloned());
    if mode == "gcp" {
        let cred = crate::gcp::parse_credential(&value)?;
        let hosts = m.hosts.clone().unwrap_or_else(|| crate::gcp::token_hosts(&cred));
        check_hosts(name, &hosts, &url)?;
    } else {
        check_binding(name, &m, &url)?;
    }
    let key = m.value.as_str();
    match mode {
        "bearer" => req.secret_headers.push(("Authorization".into(), format!("Bearer {key}"))),
        "api-key" => req.secret_headers.push(("api-key".into(), key.to_string())),
        "x-api-key" => req.secret_headers.push(("x-api-key".into(), key.to_string())),
        "basic" => req.secret_headers.push(("Authorization".into(), format!("Basic {}", b64(key.as_bytes())))),
        "aws" => sign_aws(req, &url, &value, opts)?,
        "gcp" => {
            let scope = opts.get("scope").as_str().unwrap_or(crate::gcp::SCOPE_CLOUD);
            let token = crate::gcp::access_token(grants, &value, scope, opts.get("subject").as_str(), cancel)?;
            req.secret_headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        m => {
            let h = m.strip_prefix("header:").unwrap_or(m);
            net::check_header_name(h)?;
            req.secret_headers.push((h.to_string(), key.to_string()));
        }
    }
    Ok(())
}

/// SigV4-sign a generic AWS request (Bedrock, STS, DynamoDB, S3, ...).
fn sign_aws(req: &mut Request, url: &CheckedUrl, secret: &str, opts: &Data) -> Result<(), String> {
    let creds = Credentials::from_secret(secret)?;
    req.redact.extend(creds.secrets());
    let service = opts.get("service").as_str().ok_or("auth = \"aws\" needs service (e.g. \"bedrock\", \"sts\", \"s3\")")?;
    let region = opts.get("region").as_str().unwrap_or("us-east-1");
    if !service.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') || !region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err("bad aws service or region".into());
    }
    let s3 = service == "s3";
    // S3 signs the decoded key encoded once; the other services sign the
    // path as sent, encoded once more (so twice relative to the raw path)
    let raw_path = if s3 { String::from_utf8_lossy(&percent_decode(url.path())).into_owned() } else { url.path().to_string() };
    let query = url.query().map(parse_query).unwrap_or_default();
    let payload = sha256_hex(req.body.as_deref().map(Vec::as_slice).unwrap_or(&[]));
    let mut headers: Vec<(String, String)> = vec![("Host".into(), url.authority())];
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("content-type") || k.to_ascii_lowercase().starts_with("x-amz-") {
            headers.push((k.clone(), v.clone()));
        }
    }
    let scope = Scope { region, service, time: unix_now() };
    let opts = if s3 { Options::s3() } else { Options::service() };
    let signed = sigv4::sign_request(&req.method, &raw_path, &query, &headers, &payload, &creds, &scope, opts);
    req.headers.push(("Host".into(), url.authority()));
    for (k, v) in signed.add_headers {
        if k == "X-Amz-Security-Token" {
            req.secret_headers.push((k, v));
        } else {
            req.headers.push((k, v));
        }
    }
    req.secret_headers.push(("Authorization".into(), signed.authorization));
    req.redirects = Redirects::Return;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anypage_runtime::{Caps, SecretStore};
    use std::sync::Arc;

    struct S;
    impl SecretStore for S {
        fn get(&self, _p: &str, name: &str) -> Option<String> {
            match name {
                "k" => Some("sk-secret-value".into()),
                "aws" => Some(r#"{"access_key_id":"AKID","secret_access_key":"SECRETKEY","session_token":"TOK"}"#.into()),
                _ => None,
            }
        }
    }

    fn grants() -> Grants {
        let mut g = Grants::new("t", Caps { net: vec!["api.x.com".into(), "r.openai.azure.com".into(), "*.amazonaws.com".into()], secrets: vec!["k".into(), "aws".into()], ..Caps::default() });
        g.secrets = Some(Arc::new(S));
        g
    }

    #[test]
    fn modes() {
        let g = grants();
        let mut r = Request::new("GET", "https://api.x.com/v1");
        apply(&g, &mut r, Some("k"), None, &Data::Nil, &|| false).unwrap();
        assert_eq!(r.secret_headers, vec![("Authorization".to_string(), "Bearer sk-secret-value".to_string())]);
        let mut r = Request::new("GET", "https://r.openai.azure.com/openai");
        apply(&g, &mut r, Some("k"), None, &Data::Nil, &|| false).unwrap();
        assert_eq!(r.secret_headers[0].0, "api-key");
        let mut r = Request::new("GET", "https://api.x.com/v1");
        apply(&g, &mut r, Some("k"), Some("header:X-Goog-Api-Key"), &Data::Nil, &|| false).unwrap();
        assert_eq!(r.secret_headers[0].0, "X-Goog-Api-Key");
        let mut r = Request::new("GET", "https://api.x.com/v1");
        assert!(apply(&g, &mut r, Some("undeclared"), None, &Data::Nil, &|| false).unwrap_err().contains("not declared"));
        assert!(apply(&g, &mut r, Some("k"), Some("weird"), &Data::Nil, &|| false).is_err());
        let mut r = Request::new("GET", "https://api.x.com/v1");
        apply(&g, &mut r, None, None, &Data::Nil, &|| false).unwrap();
        assert!(r.secret_headers.is_empty());
    }

    #[test]
    fn aws_generic() {
        let g = grants();
        let mut r = Request::new("POST", "https://bedrock-runtime.us-west-2.amazonaws.com/model/m%3A1/invoke").header("Content-Type", "application/json").body(b"{}".to_vec());
        apply(&g, &mut r, Some("aws"), Some("aws"), &Data::map([("service", Data::str("bedrock")), ("region", Data::str("us-west-2"))]), &|| false).unwrap();
        let auth = &r.secret_headers.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert!(auth.contains("/us-west-2/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token,"), "{auth}");
        assert!(r.secrets().contains(&"SECRETKEY"));
        assert!(r.secrets().contains(&"TOK"));
    }

    #[test]
    fn material_and_binding() {
        let m = material("  sk-plain-123  ").unwrap();
        assert_eq!((m.value.as_str(), m.hosts.is_none()), ("sk-plain-123", true));
        let m = material(r#"{"value": "sk-bound-1", "hosts": ["api.x.com", "*.y.com"]}"#).unwrap();
        assert_eq!(m.value, "sk-bound-1");
        assert!(m.secret_parts.contains(&"sk-bound-1".to_string()));
        let g = Grants::new("t", Caps { net: vec!["api.x.com".into(), "a.y.com".into(), "z.com".into()], ..Caps::default() });
        assert!(check_binding("k", &m, &net::check(&g, "https://api.x.com/v1").unwrap()).is_ok());
        assert!(check_binding("k", &m, &net::check(&g, "https://a.y.com/").unwrap()).is_ok());
        let e = check_binding("k", &m, &net::check(&g, "https://z.com/").unwrap()).unwrap_err();
        assert!(e.contains("bound to api.x.com, *.y.com") && e.contains("z.com") && !e.contains("sk-bound"), "{e}");
        assert!(material(r#"{"value": "v", "hosts": []}"#).is_err());
        assert!(material(r#"{"value": "v", "hosts": [1]}"#).is_err());
        // aws JSON: the keys are secret parts, the key id is not
        let m = material(r#"{"access_key_id":"AKIDPUBLIC","secret_access_key":"SECRETKEY","session_token":"TOKEN-1"}"#).unwrap();
        assert!(m.secret_parts.contains(&"SECRETKEY".to_string()) && m.secret_parts.contains(&"TOKEN-1".to_string()));
        assert!(!m.secret_parts.contains(&"AKIDPUBLIC".to_string()));
        // apply refuses before using a bound secret elsewhere
        struct B;
        impl SecretStore for B {
            fn get(&self, _p: &str, _n: &str) -> Option<String> {
                Some(r#"{"value": "sk-bound-1", "hosts": ["api.x.com"]}"#.into())
            }
        }
        let mut g = Grants::new("t", Caps { net: vec!["api.x.com".into(), "z.com".into()], secrets: vec!["k".into()], ..Caps::default() });
        g.secrets = Some(Arc::new(B));
        let mut r = Request::new("GET", "https://api.x.com/");
        apply(&g, &mut r, Some("k"), None, &Data::Nil, &|| false).unwrap();
        assert_eq!(r.secret_headers[0].1, "Bearer sk-bound-1");
        let mut r = Request::new("GET", "https://z.com/");
        assert!(apply(&g, &mut r, Some("k"), None, &Data::Nil, &|| false).unwrap_err().contains("bound to"));
        assert!(r.secret_headers.is_empty());
        // an undeclared target is refused before the secret is even read
        let mut r = Request::new("GET", "https://undeclared.example/");
        assert!(apply(&g, &mut r, Some("k"), None, &Data::Nil, &|| false).unwrap_err().contains("not declared in page.json capabilities.net"));
    }

    #[test]
    fn scrub_covers_base64() {
        let g = grants();
        let b = crate::util::b64(b"sk-secret-value");
        let msg = format!("server said: sk-secret-value / {b}");
        assert_eq!(scrub(&g, Some("k"), msg), "server said: [redacted] / [redacted]");
    }

    #[test]
    fn headers() {
        let h = page_headers(&Data::map([("X-A", Data::str("1")), ("X-N", Data::Num(2.0))])).unwrap();
        assert_eq!(h, vec![("X-A".to_string(), "1".to_string()), ("X-N".to_string(), "2".to_string())]);
        assert!(page_headers(&Data::map([("Host", Data::str("x"))])).is_err());
        assert!(page_headers(&Data::map([("X", Data::str("a\r\nB: c"))])).is_err());
    }
}
