//! AWS Signature Version 4: header signing and query (presigned URL)
//! signing, checked against the published AWS SigV4 test suite
//! (`aws-c-auth/tests/aws-signing-test-suite/v4`).
//!
//! Paths are given raw (not yet percent-encoded). S3 encodes them once and
//! never normalises; the other services normalise (`.`/`..`/`//`) and, since
//! the path a client sends is itself encoded once, sign it encoded twice.

use std::fmt;

use crate::util::{amz_date, encode_path, encode_strict, hex, hmac_sha256, sha256_hex};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
/// Longest presigned URL lifetime AWS accepts (7 days).
pub const MAX_PRESIGN_SECS: u64 = 604_800;

#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credentials({}, secret redacted, token {})", self.access_key_id, if self.session_token.is_some() { "set" } else { "none" })
    }
}

impl Credentials {
    /// From the `aws` secret: JSON `{access_key_id, secret_access_key,
    /// session_token}` (the `AccessKeyId`/`SecretAccessKey`/`SessionToken`
    /// spellings of `aws sts` output are accepted too).
    pub fn from_secret(secret: &str) -> Result<Self, String> {
        let j: serde_json::Value = serde_json::from_str(secret).map_err(|_| "the aws secret must be JSON {access_key_id, secret_access_key, session_token}".to_string())?;
        let j = j.get("Credentials").cloned().unwrap_or(j);
        let get = |a: &str, b: &str| j.get(a).or_else(|| j.get(b)).and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());
        let access_key_id = get("access_key_id", "AccessKeyId").ok_or("the aws secret has no access_key_id")?;
        let secret_access_key = get("secret_access_key", "SecretAccessKey").ok_or("the aws secret has no secret_access_key")?;
        let session_token = get("session_token", "SessionToken");
        Ok(Self { access_key_id, secret_access_key, session_token })
    }

    /// Values that must never appear in messages.
    pub fn secrets(&self) -> Vec<String> {
        let mut v = vec![self.secret_access_key.clone()];
        if let Some(t) = &self.session_token {
            v.push(t.clone());
        }
        v
    }
}

/// Region, service and signing time.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
    /// Unix seconds.
    pub time: u64,
}

impl Scope<'_> {
    pub fn date(&self) -> String {
        amz_date(self.time)[..8].to_string()
    }
    pub fn amz_date(&self) -> String {
        amz_date(self.time)
    }
    /// `20150830/us-east-1/service/aws4_request`
    pub fn credential_scope(&self) -> String {
        format!("{}/{}/{}/aws4_request", self.date(), self.region, self.service)
    }
}

/// Everything a signature is made of, for tests and debugging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub signed_headers: String,
}

/// Canonical URI from a raw path.
pub fn canonical_uri(raw_path: &str, normalize: bool, double_encode: bool) -> String {
    let path = if raw_path.is_empty() { "/" } else { raw_path };
    let path = if normalize { normalize_path(path) } else { path.to_string() };
    let once = encode_path(&path);
    let out = if double_encode { encode_path(&once) } else { once };
    if out.starts_with('/') {
        out
    } else {
        format!("/{out}")
    }
}

/// Remove `.` and `..` segments and empty segments (`//`), keeping a
/// trailing slash.
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut s = format!("/{}", out.join("/"));
    if path.ends_with('/') && s.len() > 1 {
        s.push('/');
    }
    s
}

/// Raw (decoded) query pairs to the canonical query string.
pub fn canonical_query(params: &[(String, String)]) -> String {
    let mut enc: Vec<(String, String)> = params.iter().map(|(k, v)| (encode_strict(k), encode_strict(v))).collect();
    enc.sort();
    enc.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Canonical header block (each line ending in `\n`) and the signed header list.
pub fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in headers {
        let name = k.trim().to_ascii_lowercase();
        let value = v.split_whitespace().collect::<Vec<_>>().join(" ");
        match grouped.iter_mut().find(|(n, _)| *n == name) {
            Some((_, vals)) => vals.push(value),
            None => grouped.push((name, vec![value])),
        }
    }
    grouped.sort_by(|a, b| a.0.cmp(&b.0));
    let block: String = grouped.iter().map(|(n, v)| format!("{n}:{}\n", v.join(","))).collect();
    let signed = grouped.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(";");
    (block, signed)
}

pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Sign a canonical request made of already-canonical parts.
pub fn sign(method: &str, canonical_uri: &str, canonical_query: &str, headers: &[(String, String)], payload_hash: &str, creds: &Credentials, scope: &Scope) -> Signed {
    let (block, signed_headers) = canonical_headers(headers);
    let canonical_request = format!("{method}\n{canonical_uri}\n{canonical_query}\n{block}\n{signed_headers}\n{payload_hash}");
    let string_to_sign = format!("{ALGORITHM}\n{}\n{}\n{}", scope.amz_date(), scope.credential_scope(), sha256_hex(canonical_request.as_bytes()));
    let key = signing_key(&creds.secret_access_key, &scope.date(), scope.region, scope.service);
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    Signed { canonical_request, string_to_sign, signature, signed_headers }
}

/// How a request is signed.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Normalise the path (all services but S3).
    pub normalize: bool,
    /// Encode the path twice (all services but S3).
    pub double_encode: bool,
    /// Add and sign `x-amz-content-sha256` (S3 requires it).
    pub content_sha256_header: bool,
}

impl Options {
    pub fn s3() -> Self {
        Self { normalize: false, double_encode: false, content_sha256_header: true }
    }
    pub fn service() -> Self {
        Self { normalize: true, double_encode: false, content_sha256_header: false }
    }
}

/// Header-signing result: headers to add to the request, and the parts.
pub struct HeaderSigned {
    pub add_headers: Vec<(String, String)>,
    pub authorization: String,
    pub signed: Signed,
}

/// Sign a request in the `Authorization` header.
/// `headers` must contain `host`; `x-amz-date`, `x-amz-security-token` and
/// (with `content_sha256_header`) `x-amz-content-sha256` are added.
#[allow(clippy::too_many_arguments)]
pub fn sign_request(method: &str, raw_path: &str, query: &[(String, String)], headers: &[(String, String)], payload_hash: &str, creds: &Credentials, scope: &Scope, opts: Options) -> HeaderSigned {
    let mut add = vec![("X-Amz-Date".to_string(), scope.amz_date())];
    if opts.content_sha256_header {
        add.push(("X-Amz-Content-Sha256".to_string(), payload_hash.to_string()));
    }
    if let Some(t) = &creds.session_token {
        add.push(("X-Amz-Security-Token".to_string(), t.clone()));
    }
    let all: Vec<(String, String)> = headers.iter().cloned().chain(add.iter().cloned()).collect();
    let signed = sign(method, &canonical_uri(raw_path, opts.normalize, opts.double_encode), &canonical_query(query), &all, payload_hash, creds, scope);
    let authorization = format!("{ALGORITHM} Credential={}/{}, SignedHeaders={}, Signature={}", creds.access_key_id, scope.credential_scope(), signed.signed_headers, signed.signature);
    HeaderSigned { add_headers: add, authorization, signed }
}

/// Presign: the full query string (with `X-Amz-Signature`) for `host` + path.
/// Only `host` is signed. S3 uses `UNSIGNED-PAYLOAD` as the payload hash.
#[allow(clippy::too_many_arguments)]
pub fn presign(method: &str, host: &str, raw_path: &str, query: &[(String, String)], expires: u64, payload_hash: &str, creds: &Credentials, scope: &Scope, opts: Options) -> (String, Signed) {
    let mut params: Vec<(String, String)> = query.to_vec();
    params.push(("X-Amz-Algorithm".into(), ALGORITHM.into()));
    params.push(("X-Amz-Credential".into(), format!("{}/{}", creds.access_key_id, scope.credential_scope())));
    params.push(("X-Amz-Date".into(), scope.amz_date()));
    params.push(("X-Amz-Expires".into(), expires.clamp(1, MAX_PRESIGN_SECS).to_string()));
    if let Some(t) = &creds.session_token {
        params.push(("X-Amz-Security-Token".into(), t.clone()));
    }
    params.push(("X-Amz-SignedHeaders".into(), "host".into()));
    let cq = canonical_query(&params);
    let headers = [("host".to_string(), host.to_string())];
    let signed = sign(method, &canonical_uri(raw_path, opts.normalize, opts.double_encode), &cq, &headers, payload_hash, creds, scope);
    let q = format!("{cq}&X-Amz-Signature={}", signed.signature);
    (q, signed)
}

#[cfg(test)]
mod tests {
    //! Vectors from the AWS SigV4 test suite as published in
    //! awslabs/aws-c-auth (tests/aws-signing-test-suite/v4): credentials
    //! AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, region
    //! us-east-1, service "service", 2015-08-30T12:36:00Z, presign
    //! expiration 3600 s.

    use super::*;
    use crate::util::EMPTY_SHA256;

    const T: u64 = 1_440_938_160; // 2015-08-30T12:36:00Z

    fn creds(token: Option<&str>) -> Credentials {
        Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: token.map(str::to_string) }
    }

    fn scope() -> Scope<'static> {
        Scope { region: "us-east-1", service: "service", time: T }
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    struct Case {
        method: &'static str,
        path: &'static str,
        query: &'static [(&'static str, &'static str)],
        headers: &'static [(&'static str, &'static str)],
        body: &'static [u8],
        sign_body: bool,
        token: Option<&'static str>,
        creq: &'static str,
        sts_hash: Option<&'static str>,
        signature: &'static str,
    }

    fn run(c: &Case) {
        let payload = sha256_hex(c.body);
        let opts = Options { normalize: true, double_encode: false, content_sha256_header: c.sign_body };
        let s = sign_request(c.method, c.path, &h(c.query), &h(c.headers), &payload, &creds(c.token), &scope(), opts);
        assert_eq!(s.signed.canonical_request, c.creq, "canonical request for {}", c.path);
        if let Some(hash) = c.sts_hash {
            assert_eq!(s.signed.string_to_sign, format!("AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n{hash}"));
        }
        assert_eq!(s.signed.signature, c.signature);
        assert!(s.authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders="));
        assert!(s.authorization.ends_with(&format!("Signature={}", c.signature)));
    }

    #[test]
    fn get_vanilla() {
        run(&Case {
            method: "GET",
            path: "/",
            query: &[],
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            sign_body: false,
            token: None,
            creq: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: Some("bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"),
            signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
        });
    }

    #[test]
    fn get_vanilla_query_order_key_case() {
        run(&Case {
            method: "GET",
            path: "/",
            query: &[("Param2", "value2"), ("Param1", "value1")],
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            sign_body: false,
            token: None,
            creq: "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: Some("816cd5b414d056048ba4f7c5386d6e0533120fb1fcfa93762cf0fc39e2cf19e0"),
            signature: "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
        });
    }

    #[test]
    fn post_x_www_form_urlencoded() {
        run(&Case {
            method: "POST",
            path: "/",
            query: &[],
            headers: &[("Content-Type", "application/x-www-form-urlencoded"), ("Host", "example.amazonaws.com"), ("Content-Length", "13")],
            body: b"Param1=value1",
            sign_body: true,
            token: None,
            creq: "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
            sts_hash: Some("b1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb"),
            signature: "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b",
        });
    }

    #[test]
    fn get_vanilla_with_session_token() {
        run(&Case {
            method: "GET",
            path: "/",
            query: &[],
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            sign_body: false,
            token: Some("6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267"),
            creq: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267\n\nhost;x-amz-date;x-amz-security-token\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8",
        });
    }

    #[test]
    fn get_utf8_and_unreserved_and_space() {
        run(&Case {
            method: "GET",
            path: "/\u{1234}",
            query: &[],
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            sign_body: false,
            token: None,
            creq: "GET\n/%E1%88%B4\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
        });
        const U: &str = "-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        run(&Case {
            method: "GET",
            path: "/",
            query: &[(U, U)],
            headers: &[("Host", "example.amazonaws.com")],
            body: b"",
            sign_body: false,
            token: None,
            creq: "GET\n/\n-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz=-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "9c3e54bfcdf0b19771a7f523ee5669cdf59bc7cc0884027167c21bb143a40197",
        });
        // get-space-unnormalized (normalize = false in its context)
        let s = sign_request("GET", "/example space/", &[], &h(&[("Host", "example.amazonaws.com")]), EMPTY_SHA256, &creds(None), &scope(), Options { normalize: false, double_encode: false, content_sha256_header: false });
        assert!(s.signed.canonical_request.starts_with("GET\n/example%20space/\n\n"), "{}", s.signed.canonical_request);
        assert_eq!(s.signed.signature, "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741");
    }

    #[test]
    fn presigned_get_vanilla() {
        let (q, s) = presign("GET", "example.amazonaws.com", "/", &[], 3600, EMPTY_SHA256, &creds(None), &scope(), Options::service());
        assert_eq!(
            s.canonical_request,
            "GET\n/\nX-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fus-east-1%2Fservice%2Faws4_request&X-Amz-Date=20150830T123600Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host\nhost:example.amazonaws.com\n\nhost\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(s.string_to_sign, "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb7705b4aa3cb8e8f5e1e0b3d4c0b64030797a313c8ceee43e33117cc43eadc5");
        assert_eq!(s.signature, "e93c787ed7f371d5c6b165c1b38ede9550f4dce4144713e844b25b7192d3865d");
        assert!(q.ends_with("&X-Amz-Signature=e93c787ed7f371d5c6b165c1b38ede9550f4dce4144713e844b25b7192d3865d"));
    }

    #[test]
    fn presigned_with_session_token() {
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        let (_, s) = presign("GET", "example.amazonaws.com", "/", &[], 3600, EMPTY_SHA256, &creds(Some(token)), &scope(), Options::service());
        assert!(s.canonical_request.contains(&format!("&X-Amz-Security-Token={token}&X-Amz-SignedHeaders=host\n")));
        assert_eq!(s.signature, "7ff2b50b376cb4d151970630573d6291dc128cc5c2a12ffb237f73cc53f67b6c");
    }

    #[test]
    fn helpers() {
        assert_eq!(canonical_uri("/a/./b/../c//d/", true, false), "/a/c/d/");
        assert_eq!(canonical_uri("/a/./b", false, false), "/a/./b");
        assert_eq!(canonical_uri("/a b", false, true), "/a%2520b");
        assert_eq!(canonical_uri("", false, false), "/");
        let (block, signed) = canonical_headers(&h(&[("My-Header1", "  a   b  "), ("my-header1", "c"), ("Host", "x")]));
        assert_eq!(block, "host:x\nmy-header1:a b,c\n");
        assert_eq!(signed, "host;my-header1");
        let c = Credentials::from_secret(r#"{"AccessKeyId":"A","SecretAccessKey":"S","SessionToken":"T"}"#).unwrap();
        assert_eq!(c.session_token.as_deref(), Some("T"));
        assert!(!format!("{c:?}").contains('S'));
        let e = Credentials::from_secret("not json wJalrXUtnFEMI").unwrap_err();
        assert!(!e.contains("wJalr"));
    }
}
