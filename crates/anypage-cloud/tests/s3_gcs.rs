//! `cloud.s3` and `cloud.gcs` against in-memory 127.0.0.1 servers that check
//! every signature themselves: SigV4 recomputed from what arrived on the
//! wire, and RS256 JWTs verified with the service account's public key by an
//! independent RSA implementation (the `rsa` crate).

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anypage_cloud::sigv4::{self, Credentials, Options, Scope};
use anypage_cloud::util::{amz_date, encode_path, parse_query, percent_decode, sha256_hex, unix_now};
use anypage_engine::Page;
use common::*;
use serde_json::json;

const AWS_SECRET: &str = r#"{"access_key_id":"AKIDS3TEST","secret_access_key":"s3-secret-KEY-1234","session_token":"sess-TOKEN-5678"}"#;
/// The right key id with a wrong secret key: every signature mismatches.
const AWS_WRONG: &str = r#"{"access_key_id":"AKIDS3TEST","secret_access_key":"WRONG-secret-KEY-0000","session_token":"sess-TOKEN-5678"}"#;

fn aws_creds() -> Credentials {
    Credentials::from_secret(AWS_SECRET).unwrap()
}

/// `20231114T221320Z` to seconds since 1970.
fn amz_time(s: &str) -> Option<u64> {
    if s.len() != 16 || s.as_bytes()[8] != b'T' || !s.ends_with('Z') {
        return None;
    }
    let n = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(4, 6)?, n(6, 8)?, n(9, 11)?, n(11, 13)?, n(13, 15)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

#[test]
fn amz_time_round_trips() {
    for t in [0, 951_782_400, 1_440_938_160, 1_700_000_000, 4_102_444_799] {
        assert_eq!(amz_time(&amz_date(t)), Some(t));
    }
}

// ---------------------------------------------------------------- S3 server

/// Check a header-signed or presigned request the way S3 does: the
/// signature for the time the request carries, which must be within 15
/// minutes of now (presigned: not before its date, not after it expires).
fn verify_sigv4(r: &MockRequest) -> Result<(), String> {
    let creds = aws_creds();
    let raw_path = String::from_utf8_lossy(&percent_decode(r.path())).into_owned();
    let mut query = parse_query(r.query());
    let date = r.header("x-amz-date").map(str::to_string).or_else(|| query.iter().find(|(k, _)| k == "X-Amz-Date").map(|(_, v)| v.clone())).ok_or("no X-Amz-Date")?;
    let time = amz_time(&date).ok_or("bad X-Amz-Date")?;
    let now = unix_now();
    let scope = Scope { region: "us-east-1", service: "s3", time };
    if let Some(pos) = query.iter().position(|(k, _)| k == "X-Amz-Signature") {
        // presigned
        let sig = query.remove(pos).1;
        let expires: u64 = query.iter().find(|(k, _)| k == "X-Amz-Expires").and_then(|(_, v)| v.parse().ok()).ok_or("no X-Amz-Expires")?;
        if time > now + 900 || now > time + expires {
            return Err("RequestTimeTooSkewed or expired".into());
        }
        let extra: Vec<(String, String)> = query.iter().filter(|(k, _)| !k.starts_with("X-Amz-")).cloned().collect();
        let host = r.header("host").ok_or("no host")?;
        let (_, s) = sigv4::presign(&r.method, host, &raw_path, &extra, expires, sigv4::UNSIGNED_PAYLOAD, &creds, &scope, Options::s3());
        return if s.signature == sig { Ok(()) } else { Err("presigned signature mismatch".into()) };
    }
    if now.abs_diff(time) > 900 {
        return Err("RequestTimeTooSkewed".into());
    }
    let auth = r.header("authorization").ok_or("no Authorization")?;
    let signed_headers = auth.split("SignedHeaders=").nth(1).and_then(|s| s.split(',').next()).ok_or("no SignedHeaders")?;
    let sig = auth.split("Signature=").nth(1).ok_or("no Signature")?;
    if !auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential=AKIDS3TEST/{}/us-east-1/s3/aws4_request,", &date[..8])) {
        return Err(format!("bad credential scope: {auth}"));
    }
    let payload = r.header("x-amz-content-sha256").ok_or("no x-amz-content-sha256")?;
    if payload != sigv4::UNSIGNED_PAYLOAD && payload != sha256_hex(&r.body) {
        return Err("payload hash mismatch".into());
    }
    if r.header("x-amz-security-token") != Some("sess-TOKEN-5678") {
        return Err("no session token".into());
    }
    let headers: Vec<(String, String)> = signed_headers.split(';').map(|h| (h.to_string(), r.header(h).unwrap_or("").to_string())).collect();
    // sign() adds nothing: give it exactly the headers that were signed
    let s = sigv4::sign(&r.method, &encode_path(&raw_path), &sigv4::canonical_query(&query), &headers, payload, &creds, &scope);
    if s.signature == sig {
        Ok(())
    } else {
        Err(format!("signature mismatch; server canonical request:\n{}", s.canonical_request))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

struct Obj {
    body: Vec<u8>,
    ctype: String,
    meta: Vec<(String, String)>,
}

fn s3_server() -> Mock {
    let store: Arc<Mutex<BTreeMap<String, Obj>>> = Arc::new(Mutex::new(BTreeMap::new()));
    Mock::start(move |r| {
        if let Err(e) = verify_sigv4(r) {
            return Reply::text(403, "application/xml", format!("<Error><Code>SignatureDoesNotMatch</Code><Message>{}</Message></Error>", xml_escape(&e)));
        }
        let path = String::from_utf8_lossy(&percent_decode(r.path())).into_owned();
        let (bucket, key) = path.trim_start_matches('/').split_once('/').map(|(b, k)| (b.to_string(), k.to_string())).unwrap_or((path.trim_start_matches('/').to_string(), String::new()));
        if bucket == "moved" {
            return Reply::Full { status: 301, headers: vec![("x-amz-bucket-region".into(), "eu-west-1".into()), ("Content-Type".into(), "application/xml".into())], body: b"<Error><Code>PermanentRedirect</Code><Message>The bucket you are attempting to access must be addressed using the specified endpoint.</Message></Error>".to_vec() };
        }
        let mut s = store.lock().unwrap();
        match (r.method.as_str(), key.is_empty()) {
            ("PUT", false) => {
                let meta = r.headers.iter().filter(|(k, _)| k.to_ascii_lowercase().starts_with("x-amz-meta-")).map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
                let etag = format!("\"{}\"", &sha256_hex(&r.body)[..32]);
                s.insert(format!("{bucket}/{key}"), Obj { body: r.body.clone(), ctype: r.header("content-type").unwrap_or("").into(), meta });
                Reply::Full { status: 200, headers: vec![("ETag".into(), etag)], body: Vec::new() }
            }
            ("GET" | "HEAD", false) => match s.get(&format!("{bucket}/{key}")) {
                Some(o) => {
                    let mut body = o.body.clone();
                    if let Some(range) = r.header("range").and_then(|v| v.strip_prefix("bytes=")) {
                        let (a, b) = range.split_once('-').unwrap();
                        let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                        body = body[a..=b.min(body.len() - 1)].to_vec();
                    }
                    let mut headers = vec![("Content-Type".into(), o.ctype.clone()), ("ETag".into(), format!("\"{}\"", &sha256_hex(&o.body)[..32])), ("Last-Modified".into(), "Tue, 14 Nov 2023 22:13:20 GMT".into())];
                    headers.extend(o.meta.iter().cloned());
                    Reply::Full { status: if r.header("range").is_some() { 206 } else { 200 }, headers, body }
                }
                None => Reply::text(404, "application/xml", "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"),
            },
            ("DELETE", false) => {
                s.remove(&format!("{bucket}/{key}"));
                Reply::Full { status: 204, headers: vec![], body: Vec::new() }
            }
            ("GET", true) => {
                let q: BTreeMap<String, String> = parse_query(r.query()).into_iter().collect();
                assert_eq!(q.get("list-type").map(String::as_str), Some("2"));
                let prefix = q.get("prefix").cloned().unwrap_or_default();
                let delim = q.get("delimiter").cloned();
                let mut contents = String::new();
                let mut prefixes = std::collections::BTreeSet::new();
                for (k, o) in s.iter() {
                    let Some(name) = k.strip_prefix(&format!("{bucket}/")) else { continue };
                    let Some(rest) = name.strip_prefix(&prefix) else { continue };
                    if let Some(d) = &delim {
                        if let Some(i) = rest.find(d.as_str()) {
                            prefixes.insert(format!("{prefix}{}", &rest[..i + d.len()]));
                            continue;
                        }
                    }
                    contents.push_str(&format!("<Contents><Key>{}</Key><LastModified>2023-11-14T22:13:20.000Z</LastModified><ETag>&quot;e&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>", xml_escape(name), o.body.len()));
                }
                let cps: String = prefixes.iter().map(|p| format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", xml_escape(p))).collect();
                Reply::text(200, "application/xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{bucket}</Name><Prefix>{}</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>{contents}{cps}</ListBucketResult>", xml_escape(&prefix)))
            }
            _ => Reply::text(400, "application/xml", "<Error><Code>BadRequest</Code><Message>?</Message></Error>"),
        }
    })
}

const S3_PAGE: &str = r#"
Req = page.input("req", nil, { agent = true })
Res = page.job("res", "cloud.s3", { Req })
"#;

#[test]
fn s3_round_trip() {
    let mock = s3_server();
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{}"], "secrets": ["aws", "aws_wrong"] }}"#, mock.port);
    let mut p = load(&caps, S3_PAGE, secrets(&[("aws", AWS_SECRET), ("aws_wrong", AWS_WRONG)])).unwrap();
    let ep = mock.url("");
    let base = |extra: serde_json::Value| {
        let mut v = json!({ "bucket": "photos", "endpoint": ep });
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    // put (payload hashed), with metadata and an awkward key
    set(&mut p, "req", base(json!({ "op": "put", "key": "2024/夏 天 #1.txt", "body": "hello s3", "content_type": "text/plain", "metadata": { "Owner": "anypage" } })));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("status").num(), Some(200.0), "{r:?}");
    assert!(r.get("etag").as_str().is_some_and(|e| e.len() > 10));
    // put with UNSIGNED-PAYLOAD
    set(&mut p, "req", base(json!({ "op": "put", "key": "2024/b.bin", "body": "0123456789", "payload": "unsigned" })));
    wait_for(&mut p, "res", 10, |d| d.get("status").num() == Some(200.0) && d.get("etag").as_str().is_some_and(|e| e.contains('"')) );
    set(&mut p, "req", base(json!({ "op": "put", "key": "other/c.txt", "body": "c" })));
    job_result(&mut p, "res").unwrap();
    // get
    set(&mut p, "req", base(json!({ "op": "get", "key": "2024/夏 天 #1.txt" })));
    let r = wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("hello s3"));
    assert_eq!(r.get("content_type").as_str(), Some("text/plain"));
    assert_eq!(r.get("metadata").get("owner").as_str(), Some("anypage"));
    assert_eq!(r.get("body").as_bytes(), Some(&b"hello s3"[..]));
    // ranged get
    set(&mut p, "req", base(json!({ "op": "get", "key": "2024/b.bin", "range": "bytes=2-4" })));
    wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("234"));
    // list with prefix and delimiter
    set(&mut p, "req", base(json!({ "op": "list", "prefix": "", "delimiter": "/" })));
    let r = wait_for(&mut p, "res", 10, |d| !d.get("prefixes").items().is_empty());
    let prefixes: Vec<&str> = r.get("prefixes").items().iter().filter_map(|d| d.as_str()).collect();
    assert_eq!(prefixes, vec!["2024/", "other/"]);
    set(&mut p, "req", base(json!({ "op": "list", "prefix": "2024/" })));
    let r = wait_for(&mut p, "res", 10, |d| d.get("count").num() == Some(2.0));
    let keys: Vec<&str> = r.get("objects").items().iter().filter_map(|o| o.get("key").as_str()).collect();
    assert_eq!(keys, vec!["2024/b.bin", "2024/夏 天 #1.txt"]);
    assert_eq!(r.get("objects").at(2).get("size").num(), Some(8.0));
    // head, delete, head
    set(&mut p, "req", base(json!({ "op": "head", "key": "2024/b.bin" })));
    let r = wait_for(&mut p, "res", 10, |d| d.get("exists").as_bool() == Some(true));
    assert_eq!(r.get("size").num(), Some(10.0));
    set(&mut p, "req", base(json!({ "op": "delete", "key": "2024/b.bin" })));
    wait_for(&mut p, "res", 10, |d| d.get("deleted").as_bool() == Some(true));
    set(&mut p, "req", base(json!({ "op": "head", "key": "2024/b.bin" })));
    wait_for(&mut p, "res", 10, |d| d.get("exists").as_bool() == Some(false));
    // errors: S3 error XML, region hint, missing key, bad signature
    set(&mut p, "req", base(json!({ "op": "get", "key": "nope" })));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("HTTP 404 NoSuchKey: The specified key does not exist."), "{e}");
    set(&mut p, "req", json!({ "op": "get", "bucket": "moved", "key": "k", "endpoint": ep }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("PermanentRedirect") && e.contains("region eu-west-1"), "{e}");
    set(&mut p, "req", base(json!({ "op": "get", "key": "x", "secret": "aws_wrong" })));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("SignatureDoesNotMatch"), "{e}");
    assert!(!e.contains("WRONG-secret-KEY") && !e.contains("sess-TOKEN"), "{e}");
    // a page cannot pick the signing time: `now` is ignored, the request is
    // signed with the device clock and the server accepts it
    set(&mut p, "req", base(json!({ "op": "get", "key": "other/c.txt", "now": 4_102_444_800u64 })));
    wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("c"));
    let last = mock.requests().pop().unwrap();
    assert_eq!(last.header("x-amz-date").map(|d| &d[..4]), Some(&amz_date(unix_now())[..4]));
}

#[test]
fn s3_and_gcs_honour_timeouts() {
    let mock = Mock::start(|_| Reply::Hang(Duration::from_secs(6)));
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{}"], "secrets": ["aws", "gcp"] }}"#, mock.port);
    let gcp = r#"{"access_token": "raw-token-1", "hosts": ["127.0.0.1"]}"#;
    let src = r#"
S = page.input("s", nil, { agent = true })
SRes = page.job("s_res", "cloud.s3", { S })
G = page.input("g", nil, { agent = true })
GRes = page.job("g_res", "cloud.gcs", { G })
"#;
    let mut p = load(&caps, src, secrets(&[("aws", AWS_SECRET), ("gcp", gcp)])).unwrap();
    let t0 = Instant::now();
    set(&mut p, "s", json!({ "op": "get", "bucket": "b", "key": "k", "endpoint": mock.url(""), "timeout": 0.5 }));
    let e = job_result(&mut p, "s_res").unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(t0.elapsed() < Duration::from_millis(2500), "s3 took {:?}", t0.elapsed());
    let t0 = Instant::now();
    set(&mut p, "g", json!({ "op": "get", "bucket": "b", "object": "o", "endpoint": mock.url(""), "idle_timeout": 0.4 }));
    let e = job_result(&mut p, "g_res").unwrap_err();
    assert!(e.contains("timed out"), "{e}");
    assert!(t0.elapsed() < Duration::from_millis(2500), "gcs took {:?}", t0.elapsed());
}

#[test]
fn s3_presigned_url_fetched_by_http_job() {
    let mock = s3_server();
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{}"], "secrets": ["aws"] }}"#, mock.port);
    let src = format!(
        r#"
Put = page.input("put", nil, {{ agent = true }})
PutRes = page.job("put_res", "cloud.s3", {{ Put }})
Url = page.input("url", nil, {{ agent = true }})
Get = page.job("get", "cloud.http", {{ Url }})
Presigned = cloud.s3_presign({{ bucket = "docs", key = "a/報告.pdf", endpoint = "{ep}", expires = 600 }})
page.set(Url, {{ url = Presigned }})
-- a page cannot date a URL into the future
Future = cloud.s3_presign({{ bucket = "docs", key = "x", endpoint = "{ep}", now = 4102444800 }})
assert(not string.find(Future, "X-Amz-Date=2100", 1, true), "presign used the page's clock")
"#,
        ep = mock.url("")
    );
    let mut p = load(&caps, &src, secrets(&[("aws", AWS_SECRET)])).unwrap();
    set(&mut p, "put", json!({ "op": "put", "bucket": "docs", "key": "a/報告.pdf", "body": "%PDF-1.7", "endpoint": mock.url("") }));
    job_result(&mut p, "put_res").unwrap();
    // the URL set at load time is fetched only after the object exists: re-set it
    let url = mock.url("") + "/docs/a/%E5%A0%B1%E5%91%8A.pdf?";
    let r = p.act(r#"{"get": "url"}"#);
    assert!(r.contains(&url), "{r}");
    assert!(r.contains("X-Amz-Security-Token=sess-TOKEN-5678"), "{r}");
    assert!(!r.contains("s3-secret-KEY"), "{r}");
    let v: serde_json::Value = serde_json::from_str(&r).unwrap();
    let presigned = v["value"]["url"].as_str().unwrap().to_string();
    set(&mut p, "url", json!({ "url": presigned, "n": 2 }));
    let r = wait_for(&mut p, "get", 10, |d| d.get("status").num() == Some(200.0));
    assert_eq!(r.get("text").as_str(), Some("%PDF-1.7"));
    // tampering breaks it
    set(&mut p, "url", json!({ "url": presigned.replace("X-Amz-Expires=600", "X-Amz-Expires=6000") }));
    wait_for(&mut p, "get", 10, |d| d.get("status").num() == Some(403.0));
}

// ---------------------------------------------------------------- GCS server

struct TestKey {
    pem: String,
    public: rsa::RsaPublicKey,
}

fn test_key() -> &'static TestKey {
    static K: OnceLock<TestKey> = OnceLock::new();
    K.get_or_init(|| {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("RSA key");
        let pem = key.to_pkcs8_pem(LineEnding::LF).expect("PEM").to_string();
        TestKey { pem, public: key.to_public_key() }
    })
}

fn b64url_decode(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).expect("base64url")
}

/// Verify an RS256 JWT with the rsa crate; returns (header, claims).
fn verify_jwt(jwt: &str) -> Result<(serde_json::Value, serde_json::Value), String> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Err("not a JWT".into());
    }
    let vk = VerifyingKey::<rsa::sha2::Sha256>::new(test_key().public.clone());
    let sig = Signature::try_from(b64url_decode(parts[2]).as_slice()).map_err(|e| e.to_string())?;
    vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).map_err(|e| format!("bad signature: {e}"))?;
    let h = serde_json::from_slice(&b64url_decode(parts[0])).map_err(|e| e.to_string())?;
    let c = serde_json::from_slice(&b64url_decode(parts[1])).map_err(|e| e.to_string())?;
    Ok((h, c))
}

fn service_account(token_uri: &str, email: &str) -> String {
    json!({
        "type": "service_account",
        "project_id": "p",
        "private_key_id": "kid-1",
        "private_key": test_key().pem,
        "client_email": email,
        "client_id": "1",
        "token_uri": token_uri,
    })
    .to_string()
}

/// Object name -> (bytes, content type).
type GcsStore = Arc<Mutex<BTreeMap<String, (Vec<u8>, String)>>>;

fn gcs_server(email: &'static str) -> Mock {
    let store: GcsStore = Arc::new(Mutex::new(BTreeMap::new()));
    Mock::start(move |r| {
        if r.path() == "/token" {
            let form: BTreeMap<String, String> = parse_query(&String::from_utf8_lossy(&r.body)).into_iter().collect();
            assert_eq!(form.get("grant_type").map(String::as_str), Some("urn:ietf:params:oauth:grant-type:jwt-bearer"));
            let (h, c) = match verify_jwt(form.get("assertion").map(String::as_str).unwrap_or("")) {
                Ok(x) => x,
                Err(e) => return Reply::json(400, json!({ "error": "invalid_grant", "error_description": e })),
            };
            assert_eq!(h["alg"], "RS256");
            assert_eq!(h["kid"], "kid-1");
            assert_eq!(c["iss"], email);
            assert!(c["aud"].as_str().unwrap().ends_with("/token"));
            assert_eq!(c["exp"].as_u64().unwrap() - c["iat"].as_u64().unwrap(), 3600);
            let scope = c["scope"].as_str().unwrap();
            let tag = if scope.ends_with("read_write") { "rw" } else { "ro" };
            return Reply::json(200, json!({ "access_token": format!("ya29.mock-{tag}"), "expires_in": 3599, "token_type": "Bearer" }));
        }
        let auth = r.header("authorization").unwrap_or("");
        let writes = r.method == "POST" || r.method == "DELETE";
        let want = if writes { "Bearer ya29.mock-rw" } else { "Bearer ya29.mock-ro" };
        if auth != want && auth != "Bearer raw-token-1" {
            return Reply::json(401, json!({ "error": { "code": 401, "message": format!("bad token {auth}") } }));
        }
        let mut s = store.lock().unwrap();
        let meta = |name: &str, body: &[u8], ctype: &str| json!({ "kind": "storage#object", "name": name, "bucket": "bkt", "size": body.len().to_string(), "contentType": ctype, "md5Hash": "x", "generation": "1", "updated": "2023-11-14T22:13:20Z" });
        let q: BTreeMap<String, String> = parse_query(r.query()).into_iter().collect();
        if r.method == "POST" && r.path() == "/upload/storage/v1/b/bkt/o" {
            assert_eq!(q.get("uploadType").map(String::as_str), Some("media"));
            let name = q.get("name").cloned().unwrap_or_default();
            let ctype = r.header("content-type").unwrap_or("").to_string();
            let m = meta(&name, &r.body, &ctype);
            s.insert(name, (r.body.clone(), ctype));
            return Reply::json(200, m);
        }
        if r.method == "GET" && r.path() == "/storage/v1/b/bkt/o" {
            let prefix = q.get("prefix").cloned().unwrap_or_default();
            let items: Vec<serde_json::Value> = s.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, (b, c))| meta(k, b, c)).collect();
            return Reply::json(200, json!({ "kind": "storage#objects", "items": items }));
        }
        if let Some(enc) = r.path().strip_prefix("/storage/v1/b/bkt/o/") {
            assert!(!enc.contains('/'), "object names must be fully encoded: {enc}");
            let name = String::from_utf8_lossy(&percent_decode(enc)).into_owned();
            return match (r.method.as_str(), s.get(&name).cloned()) {
                ("GET", Some((b, c))) if q.get("alt").map(String::as_str) == Some("media") => Reply::text(200, &c, b),
                ("GET", Some((b, c))) => Reply::json(200, meta(&name, &b, &c)),
                ("DELETE", Some(_)) => {
                    s.remove(&name);
                    Reply::Full { status: 204, headers: vec![], body: vec![] }
                }
                _ => Reply::json(404, json!({ "error": { "code": 404, "message": format!("No such object: bkt/{name}") } })),
            };
        }
        Reply::json(400, json!({ "error": { "message": "unexpected" } }))
    })
}

const GCS_PAGE: &str = r#"
Req = page.input("req", nil, { agent = true })
Res = page.job("res", "cloud.gcs", { Req })
"#;

#[test]
fn gcs_service_account_round_trip() {
    let email = "uploader@p.iam.gserviceaccount.com";
    let mock = gcs_server(email);
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{}"], "secrets": ["gcp"] }}"#, mock.port);
    let sa = service_account(&mock.url("/token"), email);
    let mut p = load(&caps, GCS_PAGE, secrets(&[("gcp", &sa)])).unwrap();
    let ep = mock.url("");
    let base = |extra: serde_json::Value| {
        let mut v = json!({ "bucket": "bkt", "endpoint": ep });
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    set(&mut p, "req", base(json!({ "op": "put", "object": "dir/筆記 1.txt", "body": "gcs hello", "content_type": "text/plain" })));
    let r = job_result(&mut p, "res").unwrap();
    assert_eq!(r.get("name").as_str(), Some("dir/筆記 1.txt"), "{r:?}");
    assert_eq!(r.get("size").num(), Some(9.0));
    set(&mut p, "req", base(json!({ "op": "put", "object": "dir/b.json", "body": "{}", "content_type": "application/json" })));
    wait_for(&mut p, "res", 10, |d| d.get("name").as_str() == Some("dir/b.json"));
    set(&mut p, "req", base(json!({ "op": "get", "object": "dir/筆記 1.txt" })));
    let r = wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("gcs hello"));
    assert_eq!(r.get("content_type").as_str(), Some("text/plain"));
    set(&mut p, "req", base(json!({ "op": "list", "prefix": "dir/" })));
    let r = wait_for(&mut p, "res", 10, |d| d.get("objects").items().len() == 2);
    assert_eq!(r.get("objects").at(1).get("name").as_str(), Some("dir/b.json"));
    set(&mut p, "req", base(json!({ "op": "head", "object": "dir/b.json" })));
    wait_for(&mut p, "res", 10, |d| d.get("exists").as_bool() == Some(true) && d.get("content_type").as_str() == Some("application/json"));
    set(&mut p, "req", base(json!({ "op": "delete", "object": "dir/b.json" })));
    wait_for(&mut p, "res", 10, |d| d.get("deleted").as_bool() == Some(true));
    set(&mut p, "req", base(json!({ "op": "head", "object": "dir/b.json" })));
    wait_for(&mut p, "res", 10, |d| d.get("exists").as_bool() == Some(false));
    set(&mut p, "req", base(json!({ "op": "get", "object": "missing" })));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("HTTP 404: No such object: bkt/missing"), "{e}");
    assert!(!e.contains("PRIVATE KEY"));
    // one token per scope (read-only, read-write), cached for the rest
    assert_eq!(mock.count(|r| r.path() == "/token"), 2, "tokens were not cached");
}

#[test]
fn gcs_raw_token_and_refusals() {
    let email = "x@p.iam.gserviceaccount.com";
    let mock = gcs_server(email);
    let port = mock.port;
    let caps = format!(r#"{{ "modules": ["cloud"], "net": ["127.0.0.1:{port}", "localhost:{port}", "www.googleapis.com"], "secrets": ["gcp", "gcp2", "gcp3", "gcp4"] }}"#);
    // a raw token bound by its owner to the emulator's host
    let bound_raw = r#"{"access_token": "raw-token-1", "hosts": ["127.0.0.1"]}"#;
    // the token endpoint of this key is not declared
    let sa_elsewhere = service_account("https://oauth2.googleapis.com/token", email);
    // a key whose token endpoint is the mock
    let sa_here = service_account(&mock.url("/token"), email);
    let mut p = load(&caps, GCS_PAGE, secrets(&[("gcp", bound_raw), ("gcp2", &sa_elsewhere), ("gcp3", "raw-token-1"), ("gcp4", &sa_here)])).unwrap();
    set(&mut p, "req", json!({ "op": "put", "bucket": "bkt", "object": "r.txt", "body": "raw", "endpoint": mock.url("") }));
    job_result(&mut p, "res").unwrap();
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": mock.url("") }));
    wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("raw"));
    // a raw token without a binding goes to Google only
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": mock.url(""), "secret": "gcp3" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("secret \"gcp3\" is bound to googleapis.com, *.googleapis.com") && e.contains("may not be sent to 127.0.0.1"), "{e}");
    // a key's token goes to Google and to its own token host, not to
    // another declared host
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": format!("http://localhost:{port}"), "secret": "gcp4" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("is bound to") && e.contains("may not be sent to localhost"), "{e}");
    // undeclared endpoint: refused before a JWT is signed or a token fetched
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": "https://evil.example", "secret": "gcp4" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("evil.example is not declared"), "{e}");
    // a Google key's token is not sent to the emulator either
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": mock.url(""), "secret": "gcp2" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("secret \"gcp2\" is bound to googleapis.com, *.googleapis.com and may not be sent to 127.0.0.1"), "{e}");
    // to a declared Google host: the key's token endpoint must be declared too
    // (refused before any network call)
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": "https://www.googleapis.com", "secret": "gcp2" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("Google access token") && e.contains("oauth2.googleapis.com is not declared"), "{e}");
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt" }));
    let e = job_result(&mut p, "res").unwrap_err();
    assert!(e.contains("storage.googleapis.com is not declared"), "{e}");
    assert_eq!(mock.count(|r| r.path() == "/token"), 0, "a token was fetched for a refused request");
    // the same key to its own host works
    set(&mut p, "req", json!({ "op": "get", "bucket": "bkt", "object": "r.txt", "endpoint": mock.url(""), "secret": "gcp4", "n": 2 }));
    wait_for(&mut p, "res", 10, |d| d.get("text").as_str() == Some("raw"));
}

#[test]
fn jwt_signature_verifies_with_the_public_key() {
    let sa = match anypage_cloud::gcp::parse_credential(&service_account("https://oauth2.googleapis.com/token", "a@b.c")).unwrap() {
        anypage_cloud::gcp::Credential::ServiceAccount(sa) => sa,
        _ => panic!("not a service account"),
    };
    let jwt = anypage_cloud::gcp::make_jwt(&sa, "s1 s2", Some("user@x.com"), 1_000).unwrap();
    let (h, c) = verify_jwt(&jwt).unwrap();
    assert_eq!(h, json!({ "alg": "RS256", "typ": "JWT", "kid": "kid-1" }));
    assert_eq!(c, json!({ "iss": "a@b.c", "scope": "s1 s2", "aud": "https://oauth2.googleapis.com/token", "iat": 1000, "exp": 4600, "sub": "user@x.com" }));
    // a flipped bit fails
    let mut bad = jwt.clone().into_bytes();
    let i = bad.len() - 5;
    bad[i] = if bad[i] == b'A' { b'B' } else { b'A' };
    assert!(verify_jwt(std::str::from_utf8(&bad).unwrap()).is_err());
}
