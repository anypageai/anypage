//! Amazon S3 (and S3-compatible stores: MinIO, Cloudflare R2, Wasabi,
//! Backblaze B2, Ceph) over SigV4.
//!
//! Addressing: virtual-hosted style (`https://bucket.s3.region.amazonaws.com/key`)
//! for AWS, path style (`https://endpoint/bucket/key`) for custom endpoints,
//! buckets with dots, or when `path_style = true`. Keys are encoded once and
//! never normalised, exactly as S3 signs them.

use std::sync::Arc;

use anypage_runtime::{Data, Grants};

use crate::net::{self, CheckedUrl, Redirects, Request};
use crate::sigv4::{self, Credentials, Options, Scope};
use crate::util::{encode_path, sha256_hex, unix_now, EMPTY_SHA256};
use crate::xml;

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
    pub fn name(self) -> &'static str {
        match self {
            Op::Get => "get",
            Op::Put => "put",
            Op::List => "list",
            Op::Delete => "delete",
            Op::Head => "head",
        }
    }
    fn method(self) -> &'static str {
        match self {
            Op::Get | Op::List => "GET",
            Op::Put => "PUT",
            Op::Delete => "DELETE",
            Op::Head => "HEAD",
        }
    }
}

/// Where a bucket/key lives: scheme+authority and the raw (unencoded) path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// `https://bucket.s3.us-east-1.amazonaws.com`
    pub origin: String,
    /// `bucket.s3.us-east-1.amazonaws.com[:port]`
    pub host: String,
    /// Raw path: `/key` or `/bucket/key`.
    pub raw_path: String,
}

fn valid_bucket(b: &str, virtual_host: bool) -> Result<(), String> {
    let ok = !b.is_empty()
        && b.len() <= 255
        && b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.') || (!virtual_host && (c.is_ascii_uppercase() || c == b'_')));
    if ok {
        Ok(())
    } else {
        Err(format!("\"{b}\" is not a valid bucket name"))
    }
}

/// Resolve where `bucket`/`key` is for `region` / `endpoint` / `path_style`.
pub fn locate(bucket: &str, key: &str, region: &str, endpoint: Option<&str>, path_style: Option<bool>) -> Result<Location, String> {
    if region.is_empty() || !region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err(format!("\"{region}\" is not a valid region"));
    }
    let (scheme, authority, custom) = match endpoint {
        Some(e) => {
            let e = e.trim_end_matches('/');
            let (scheme, rest) = e.split_once("://").ok_or("endpoint must be a URL like https://s3.example.com")?;
            if rest.contains(['/', '?', '#', '@']) || rest.is_empty() {
                return Err("endpoint must be scheme://host[:port] with no path".into());
            }
            (scheme.to_ascii_lowercase(), rest.to_ascii_lowercase(), true)
        }
        None => ("https".to_string(), format!("s3.{region}.amazonaws.com"), false),
    };
    let virtual_host = match path_style {
        Some(p) => !p,
        None => !custom && !bucket.contains('.'),
    };
    valid_bucket(bucket, virtual_host)?;
    let (host, raw_path) = if virtual_host { (format!("{bucket}.{authority}"), format!("/{key}")) } else { (authority, if key.is_empty() { format!("/{bucket}") } else { format!("/{bucket}/{key}") }) };
    Ok(Location { origin: format!("{scheme}://{host}"), host, raw_path })
}

/// Everything needed to sign and send one S3 request.
#[derive(Debug)]
pub struct Prepared {
    pub request: Request,
    pub url: String,
    pub checked: CheckedUrl,
}

fn query_of(op: Op, req: &Data) -> Vec<(String, String)> {
    let mut q = Vec::new();
    if op == Op::List {
        q.push(("list-type".to_string(), "2".to_string()));
        for (k, name) in [("prefix", "prefix"), ("delimiter", "delimiter"), ("continuation-token", "continuation_token"), ("start-after", "start_after")] {
            if let Some(v) = req.get(name).as_str().filter(|v| !v.is_empty()) {
                q.push((k.to_string(), v.to_string()));
            }
        }
        if let Some(n) = req.get("max_keys").num().or_else(|| req.get("max").num()) {
            q.push(("max-keys".into(), (n.clamp(1.0, 1000.0) as u64).to_string()));
        }
    }
    if let Some(v) = req.get("version_id").as_str() {
        q.push(("versionId".into(), v.to_string()));
    }
    q
}

/// Build and sign an S3 request at time `now`.
pub fn prepare(grants: &Grants, op: Op, req: &Data, creds: &Credentials, now: u64) -> Result<Prepared, String> {
    let bucket = req.get("bucket").as_str().ok_or("s3 needs a bucket")?;
    let key = req.get("key").as_str().unwrap_or("");
    if op != Op::List && key.is_empty() {
        return Err(format!("s3 {} needs a key", op.name()));
    }
    if key.len() > 1024 {
        return Err("S3 keys are at most 1024 bytes".into());
    }
    let region = req.get("region").as_str().unwrap_or("us-east-1");
    let loc = locate(bucket, key, region, req.get("endpoint").as_str(), req.get("path_style").as_bool())?;
    let query = query_of(op, req);
    let cq = sigv4::canonical_query(&query);
    let url = format!("{}{}{}{}", loc.origin, encode_path(&loc.raw_path), if cq.is_empty() { "" } else { "?" }, cq);
    // check before signing anything
    let checked: CheckedUrl = net::check(grants, &url)?;
    let body: Option<Vec<u8>> = if op == Op::Put { Some(req.get("body").as_bytes().ok_or("s3 put needs a body (bytes or a string)")?.to_vec()) } else { None };
    let unsigned = req.get("payload").as_str() == Some("unsigned");
    let payload_hash = match (&body, unsigned) {
        (_, true) => sigv4::UNSIGNED_PAYLOAD.to_string(),
        (Some(b), false) => sha256_hex(b),
        (None, false) => EMPTY_SHA256.to_string(),
    };
    let mut headers: Vec<(String, String)> = vec![("Host".into(), checked.authority())];
    if let (Op::Get | Op::Head, Some(range)) = (op, req.get("range").as_str()) {
        if !range.starts_with("bytes=") || range.len() > 100 {
            return Err("range must look like \"bytes=0-1023\"".into());
        }
        headers.push(("Range".into(), range.to_string()));
    }
    if op == Op::Put {
        headers.push(("Content-Type".into(), req.get("content_type").as_str().unwrap_or("application/octet-stream").to_string()));
        for (k, v) in req.get("metadata").entries() {
            let name = format!("x-amz-meta-{}", k.to_ascii_lowercase());
            net::check_header_name(&name)?;
            let v = v.as_str().ok_or("metadata values must be strings")?;
            headers.push((name, v.to_string()));
        }
    }
    let scope = Scope { region, service: "s3", time: now };
    let signed = sigv4::sign_request(op.method(), &loc.raw_path, &query, &headers, &payload_hash, creds, &scope, Options::s3());
    let mut r = Request::new(op.method(), &url);
    r.headers = headers.into_iter().filter(|(k, _)| k != "Host").collect();
    r.headers.push(("Host".into(), checked.authority()));
    for (k, v) in &signed.add_headers {
        if k == "X-Amz-Security-Token" {
            r.secret_headers.push((k.clone(), v.clone()));
        } else {
            r.headers.push((k.clone(), v.clone()));
        }
    }
    r.secret_headers.push(("Authorization".into(), signed.authorization));
    r.redact = creds.secrets();
    r.body = body.map(Arc::new);
    // a redirect would need a new signature for the new host: report it instead
    r.redirects = Redirects::Return;
    Ok(Prepared { request: r, url, checked })
}

/// A presigned URL (`cloud.s3_presign`).
pub fn presign(grants: &Grants, req: &Data, creds: &Credentials, now: u64) -> Result<String, String> {
    let bucket = req.get("bucket").as_str().ok_or("s3_presign needs a bucket")?;
    let key = req.get("key").as_str().filter(|k| !k.is_empty()).ok_or("s3_presign needs a key")?;
    let region = req.get("region").as_str().unwrap_or("us-east-1");
    let method = req.get("method").as_str().unwrap_or("GET").to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "PUT" | "HEAD" | "DELETE") {
        return Err(format!("s3_presign: method {method} is not GET, PUT, HEAD or DELETE"));
    }
    let expires = req.get("expires").num().unwrap_or(3600.0);
    if !(1.0..=sigv4::MAX_PRESIGN_SECS as f64).contains(&expires) {
        return Err(format!("expires must be 1..{} seconds", sigv4::MAX_PRESIGN_SECS));
    }
    let loc = locate(bucket, key, region, req.get("endpoint").as_str(), req.get("path_style").as_bool())?;
    let base = format!("{}{}", loc.origin, encode_path(&loc.raw_path));
    let checked = net::check(grants, &base)?;
    let scope = Scope { region, service: "s3", time: now };
    let (q, _) = sigv4::presign(&method, &checked.authority(), &loc.raw_path, &[], expires as u64, sigv4::UNSIGNED_PAYLOAD, creds, &scope, Options::s3());
    Ok(format!("{}{}?{q}", checked.origin(), checked.path()))
}

fn s3_error(op: Op, status: u16, body: &[u8], headers: &[(String, String)]) -> String {
    let text = String::from_utf8_lossy(body);
    let mut s = format!("s3 {}: HTTP {status}", op.name());
    if let Some(c) = xml::text(&text, "Code") {
        s.push(' ');
        s.push_str(&c);
    }
    if let Some(m) = xml::text(&text, "Message") {
        s.push_str(": ");
        s.push_str(&crate::util::clip(&m, 500));
    }
    if let Some((_, r)) = headers.iter().find(|(k, _)| k == "x-amz-bucket-region") {
        s.push_str(&format!(" (the bucket is in region {r}; pass region = \"{r}\")"));
    }
    s
}

/// Run one S3 operation. Requests are signed with the device clock.
pub fn run(grants: &Grants, req: &Data, max: usize, cancel: &dyn Fn() -> bool, progress: &dyn Fn(f32)) -> Result<Data, String> {
    let op = Op::parse(req.get("op").as_str().unwrap_or("get"))?;
    let name = req.get("secret").as_str().unwrap_or("aws");
    let secret = grants.secret(name)?;
    let m = crate::auth::material(&secret)?;
    let creds = Credentials::from_secret(&secret)?;
    // prepare checks the URL against the capabilities before signing
    let mut p = prepare(grants, op, req, &creds, unix_now())?;
    crate::auth::check_binding(name, &m, &p.checked)?;
    grants.check_secret_url(name, &p.checked.as_string())?;
    let body_len = p.request.body.as_ref().map_or(0, |b| b.len());
    p.request.timeouts = net::Timeouts::for_job(req, max.saturating_add(body_len))?;
    p.request.redact.extend(m.secret_parts);
    let resp = net::send(grants, &p.request, cancel).map_err(|e| format!("s3 {}: {e}", op.name()))?;
    // nothing handed back to the page carries the session token or the key
    let scrub = resp.scrubber();
    let clean = |v: &str| Data::str(&scrub.text(v).unwrap_or_else(|| v.to_string()));
    let status = resp.status;
    let header = |name: &str| resp.header(name).map(clean).unwrap_or_default();
    let etag = header("etag");
    let ctype = header("content-type");
    let last_modified = header("last-modified");
    let version = header("x-amz-version-id");
    let length = resp.content_length();
    let meta: Vec<(String, Data)> = resp.headers.iter().filter_map(|(k, v)| k.strip_prefix("x-amz-meta-").map(|m| (m.to_string(), clean(v)))).collect();
    if op == Op::Head {
        if status == 404 {
            return Ok(Data::map([("exists", Data::Bool(false)), ("status", Data::Num(404.0))]));
        }
        if !(200..300).contains(&status) {
            return Err(s3_error(op, status, &[], &resp.headers));
        }
        return Ok(Data::map([
            ("exists", Data::Bool(true)),
            ("status", Data::Num(status as f64)),
            ("size", length.map(|n| Data::Num(n as f64)).unwrap_or_default()),
            ("etag", etag),
            ("content_type", ctype),
            ("last_modified", last_modified),
            ("version_id", version),
            ("metadata", Data::map(meta.iter().map(|(k, v)| (k.as_str(), v.clone())))),
        ]));
    }
    if !(200..300).contains(&status) {
        let headers = resp.headers.clone();
        let body = resp.read_all(64 * 1024, cancel, &|_| {}).unwrap_or_default();
        return Err(s3_error(op, status, &body, &headers));
    }
    let body = scrub.bytes(resp.read_all(max, cancel, progress)?);
    Ok(match op {
        Op::Get => {
            let mut m = vec![
                ("status", Data::Num(status as f64)),
                ("size", Data::Num(body.len() as f64)),
                ("etag", etag),
                ("content_type", ctype),
                ("last_modified", last_modified),
                ("version_id", version),
                ("metadata", Data::map(meta.iter().map(|(k, v)| (k.as_str(), v.clone())))),
            ];
            if let Ok(t) = std::str::from_utf8(&body) {
                m.push(("text", Data::str(t)));
            }
            m.push(("body", Data::bytes(body)));
            Data::map(m)
        }
        Op::Put => Data::map([("status", Data::Num(status as f64)), ("etag", etag), ("version_id", version)]),
        Op::Delete => Data::map([("status", Data::Num(status as f64)), ("deleted", Data::Bool(true))]),
        Op::List => parse_list(&String::from_utf8_lossy(&body), status),
        Op::Head => Data::Nil,
    })
}

/// ListObjectsV2 XML to `{ objects, prefixes, truncated, next_token, count }`.
pub fn parse_list(xml_text: &str, status: u16) -> Data {
    let objects: Vec<Data> = xml::blocks(xml_text, "Contents")
        .into_iter()
        .map(|c| {
            let t = |k: &str| xml::text(c, k).map(|s| Data::str(&s)).unwrap_or_default();
            let size = xml::text(c, "Size").and_then(|s| s.parse::<f64>().ok()).map(Data::Num).unwrap_or_default();
            Data::map([("key", t("Key")), ("size", size), ("etag", t("ETag")), ("last_modified", t("LastModified")), ("storage_class", t("StorageClass"))])
        })
        .collect();
    let prefixes: Vec<Data> = xml::blocks(xml_text, "CommonPrefixes").into_iter().filter_map(|b| xml::text(b, "Prefix")).map(|p| Data::str(&p)).collect();
    // the top-level <Prefix> also exists; only the parts after </Contents> or
    // outside <CommonPrefixes> matter for the fields below, which are unique
    Data::map([
        ("count", Data::Num(objects.len() as f64)),
        ("objects", Data::list(objects)),
        ("prefixes", Data::list(prefixes)),
        ("truncated", Data::Bool(xml::text(xml_text, "IsTruncated").as_deref() == Some("true"))),
        ("next_token", xml::text(xml_text, "NextContinuationToken").map(|s| Data::str(&s)).unwrap_or_default()),
        ("status", Data::Num(status as f64)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use anypage_runtime::Caps;

    fn grants() -> Grants {
        Grants::new("t", Caps { net: vec!["*.amazonaws.com".into(), "127.0.0.1".into(), "minio.local".into()], ..Caps::default() })
    }

    fn creds() -> Credentials {
        Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), session_token: None }
    }

    #[test]
    fn addressing() {
        let l = locate("my-bucket", "a b/c.txt", "eu-west-1", None, None).unwrap();
        assert_eq!(l.origin, "https://my-bucket.s3.eu-west-1.amazonaws.com");
        assert_eq!(l.raw_path, "/a b/c.txt");
        let l = locate("my.bucket", "k", "us-east-1", None, None).unwrap();
        assert_eq!((l.host.as_str(), l.raw_path.as_str()), ("s3.us-east-1.amazonaws.com", "/my.bucket/k"));
        let l = locate("b", "k", "us-east-1", Some("http://127.0.0.1:9000/"), None).unwrap();
        assert_eq!((l.origin.as_str(), l.raw_path.as_str()), ("http://127.0.0.1:9000", "/b/k"));
        let l = locate("b", "", "us-east-1", Some("https://minio.local"), Some(false)).unwrap();
        assert_eq!(l.origin, "https://b.minio.local");
        assert!(locate("Bad_Bucket", "k", "us-east-1", None, None).is_err());
        assert!(locate("b", "k", "us-east-1", Some("https://x/y"), None).is_err());
        assert!(locate("b", "k", "us east", None, None).is_err());
    }

    #[test]
    fn signed_requests() {
        let req = Data::map([("bucket", Data::str("examplebucket")), ("key", Data::str("test.txt")), ("op", Data::str("get"))]);
        let p = prepare(&grants(), Op::Get, &req, &creds(), 1_369_353_600).unwrap();
        assert_eq!(p.url, "https://examplebucket.s3.us-east-1.amazonaws.com/test.txt");
        let auth = &p.request.secret_headers.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="));
        // list: query is signed and sent in the same canonical order
        let req = Data::map([("bucket", Data::str("b")), ("prefix", Data::str("photos/2024 x")), ("max_keys", Data::Num(5.0))]);
        let p = prepare(&grants(), Op::List, &req, &creds(), 1_369_353_600).unwrap();
        assert_eq!(p.url, "https://b.s3.us-east-1.amazonaws.com/?list-type=2&max-keys=5&prefix=photos%2F2024%20x");
        // put hashes the payload; unsigned payload on request
        let req = Data::map([("bucket", Data::str("b")), ("key", Data::str("k")), ("body", Data::str("hello")), ("metadata", Data::map([("Owner", Data::str("me"))]))]);
        let p = prepare(&grants(), Op::Put, &req, &creds(), 1_369_353_600).unwrap();
        let h = |n: &str| p.request.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        assert_eq!(h("x-amz-content-sha256").as_deref(), Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"));
        assert_eq!(h("x-amz-meta-owner").as_deref(), Some("me"));
        let req = Data::map([("bucket", Data::str("b")), ("key", Data::str("k")), ("body", Data::str("hello")), ("payload", Data::str("unsigned"))]);
        let p = prepare(&grants(), Op::Put, &req, &creds(), 1_369_353_600).unwrap();
        assert_eq!(p.request.headers.iter().find(|(k, _)| k == "X-Amz-Content-Sha256").map(|(_, v)| v.as_str()), Some("UNSIGNED-PAYLOAD"));
        // not declared
        let req = Data::map([("bucket", Data::str("b")), ("key", Data::str("k")), ("endpoint", Data::str("https://evil.example"))]);
        assert!(prepare(&grants(), Op::Get, &req, &creds(), 0).unwrap_err().contains("not declared"));
    }

    /// The examples of the S3 SigV4 documentation ("Authenticating Requests
    /// (AWS Signature Version 4)", header and query-parameter examples):
    /// AKIAIOSFODNN7EXAMPLE, examplebucket, 20130524T000000Z, us-east-1.
    fn doc_creds() -> Credentials {
        Credentials { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), session_token: None }
    }

    const DOC_TIME: u64 = 1_369_353_600; // 2013-05-24T00:00:00Z

    fn doc_signature(req: &Data, op: Op) -> String {
        let p = prepare(&grants(), op, req, &doc_creds(), DOC_TIME).unwrap();
        let auth = &p.request.secret_headers.iter().find(|(k, _)| k == "Authorization").unwrap().1;
        auth.rsplit("Signature=").next().unwrap().to_string()
    }

    #[test]
    fn s3_documentation_examples() {
        let base = |pairs: Vec<(&'static str, Data)>| {
            let mut v = vec![("bucket", Data::str("examplebucket")), ("endpoint", Data::str("https://s3.amazonaws.com")), ("path_style", Data::Bool(false))];
            v.extend(pairs);
            Data::map(v)
        };
        // GET Object with a Range header
        let get = base(vec![("key", Data::str("test.txt")), ("range", Data::str("bytes=0-9"))]);
        assert_eq!(doc_signature(&get, Op::Get), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
        // GET Bucket (list objects): max-keys=2&prefix=J, sent as ListObjectsV1 in
        // the docs, so sign the same query by hand here
        let s = sigv4::sign_request(
            "GET",
            "/",
            &[("max-keys".into(), "2".into()), ("prefix".into(), "J".into())],
            &[("Host".into(), "examplebucket.s3.amazonaws.com".into())],
            EMPTY_SHA256,
            &doc_creds(),
            &Scope { region: "us-east-1", service: "s3", time: DOC_TIME },
            Options::s3(),
        );
        assert_eq!(s.signed.signature, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
        // presigned GET, 86400 s
        let url = presign(&grants(), &base(vec![("key", Data::str("test.txt")), ("expires", Data::Num(86400.0))]), &doc_creds(), DOC_TIME).unwrap();
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
        assert!(!url.contains("wJalr"));
    }

    #[test]
    fn list_xml() {
        let x = "<ListBucketResult><Name>b</Name><Prefix>p/</Prefix><KeyCount>1</KeyCount><IsTruncated>false</IsTruncated><Contents><Key>p/x</Key><Size>3</Size><ETag>\"e\"</ETag></Contents><CommonPrefixes><Prefix>p/d/</Prefix></CommonPrefixes></ListBucketResult>";
        let d = parse_list(x, 200);
        assert_eq!(d.get("count").num(), Some(1.0));
        assert_eq!(d.get("objects").at(1).get("key").as_str(), Some("p/x"));
        assert_eq!(d.get("objects").at(1).get("size").num(), Some(3.0));
        assert_eq!(d.get("prefixes").at(1).as_str(), Some("p/d/"));
        assert_eq!(d.get("truncated").as_bool(), Some(false));
    }
}
