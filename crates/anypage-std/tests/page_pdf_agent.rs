//! pages/pdf-agent driven headlessly, the way people and agents use it:
//!
//! * the sample PDF is generated with the pdf module (pages/pdf-agent/sample.luau)
//!   and written to pages/pdf-agent/assets/sample.pdf when missing (or when
//!   ANYPAGE_WRITE_SAMPLE=1);
//! * the semantic port: `describe` lists every action with JSON-schema
//!   parameters, `act` runs them and returns small JSON results;
//! * the in-page assistant against a 127.0.0.1 mock of an OpenAI-compatible
//!   server that streams tool calls (search, then highlight) and then a final
//!   answer: the actions run (highlights exist in the PDF) and the chat panel
//!   shows every step;
//! * pointer selection, the highlighter tool, thumbnails, keys and the wheel;
//! * people's unsaved edits (Esc, replacing the document, the assistant's
//!   tools), agent-written cells of the wrong type, and the narrow layout a
//!   phone-sized viewport picks.
//!
//! Run: `cargo test -p anypage-std --test page_pdf_agent`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anypage_engine::core::Status;
use anypage_engine::{HostResult, Page, PageRequest, PointerResponse, TextRaster, Wake};
use anypage_runtime::{Data, DisplayList, Package, ScriptPage};
use serde_json::{json, Value as Json};

// ---------------------------------------------------------------- helpers

fn text() -> Arc<TextRaster> {
    Arc::new(TextRaster::load().expect("a system font"))
}

fn wake() -> Wake {
    Arc::new(|| {})
}

/// The repository root: this crate's grandparent, or ANYPAGE_ROOT.
fn repo() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if here.join("pages/pdf-agent/page.json").is_file() {
        return here;
    }
    PathBuf::from(std::env::var("ANYPAGE_ROOT").expect("run from crates/anypage-std or set ANYPAGE_ROOT"))
}

fn package_dir() -> PathBuf {
    repo().join("pages/pdf-agent")
}

fn load_page() -> ScriptPage {
    let pkg = Package::from_dir(&package_dir()).expect("pdf-agent package");
    let t0 = Instant::now();
    let p = ScriptPage::load(&pkg, &anypage_std::natives(), text(), wake()).unwrap_or_else(|e| panic!("pdf-agent failed to load: {e}"));
    eprintln!("pdf-agent loaded in {:.1} ms (script {:.1} ms)", t0.elapsed().as_secs_f64() * 1000.0, p.load_ms);
    assert!(p.load_ms < p.manifest.limits.load_ms, "load took {} ms, limit {}", p.load_ms, p.manifest.limits.load_ms);
    p
}

/// Run the page's graph (commit, react to finished jobs) until `ok` holds.
fn pump_until(p: &mut ScriptPage, what: &str, secs: u64, mut ok: impl FnMut(&mut ScriptPage) -> bool) {
    let t0 = Instant::now();
    loop {
        p.pump();
        if ok(p) {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A named cell once it is ready (jobs evaluated).
fn ready(p: &mut ScriptPage, name: &str) -> Data {
    let id = p.cell_id(name).unwrap_or_else(|| panic!("no cell {name}"));
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(id);
        if matches!(i.status, Status::Ready) {
            return anypage_runtime::data::input_to_data(&i);
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{name} not ready: {:?}", i.status);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn port(p: &mut ScriptPage, req: Json) -> Json {
    let answer = p.act(&req.to_string());
    serde_json::from_str(&answer).unwrap_or_else(|e| panic!("port answer is not JSON ({e}): {answer}"))
}

/// `{"action": name, "params": params}` through the port; panics on an error answer.
fn action(p: &mut ScriptPage, name: &str, params: Json) -> Json {
    let a = port(p, json!({ "action": name, "params": params }));
    assert_eq!(a["ok"], json!(true), "{name} {params}: {a}");
    a["result"].clone()
}

fn act_data(p: &mut ScriptPage, name: &str, params: Data) -> Data {
    p.run_action(name, &params).unwrap_or_else(|e| panic!("action {name}: {e}"))
}

/// The display list a panel draws now, found by its world rect's origin.
fn display_at(p: &mut ScriptPage, x: f32, y: f32) -> Data {
    let raster = p.spec().panels.iter().find(|pa| pa.rect.x == x && pa.rect.y == y).unwrap_or_else(|| panic!("no panel at {x},{y}")).cell;
    let script = p.graph().inputs_of(raster)[0];
    let g = p.parts().0;
    g.commit();
    let i = g.demand(script);
    assert!(matches!(i.status, Status::Ready), "panel at {x},{y}: {:?}", i.status);
    i.get::<DisplayList>().expect("a display list").cmds.clone()
}

fn texts(cmds: &Data) -> Vec<String> {
    cmds.items().iter().filter(|c| c.at(1).as_str() == Some("text")).filter_map(|c| c.at(6).as_str().map(str::to_string)).collect()
}

fn no_error_card(cmds: &Data, what: &str) {
    let t = texts(cmds);
    assert!(!t.iter().any(|s| s.contains("the view failed")), "{what} shows the error card: {t:?}");
}

/// The side panel's display list, after switching to tab `tab`.
fn side_tab(p: &mut ScriptPage, tab: u32) -> Data {
    let mut st = p.cell_value("side.state").expect("side.state");
    if let Data::Map(m) = &mut st {
        Arc::make_mut(m).insert(Arc::from("tab"), Data::Num(tab as f64));
    }
    let r = port(p, json!({ "set": "side.state", "value": st.to_json() }));
    assert_eq!(r["ok"], json!(true), "set side.state: {r}");
    let cmds = display_at(p, 1000.0, 56.0);
    no_error_card(&cmds, &format!("side tab {tab}"));
    cmds
}

/// Annotations of `page` in a saved PDF.
fn saved_annotations(bytes: &[u8], page: f64) -> Vec<Data> {
    let doc = anypage_pdf::PdfDoc::open(Arc::new(bytes.to_vec()), "").expect("saved PDF opens");
    doc.with_lo(|d| Ok(anypage_pdf::edit::annotations(d, anypage_pdf::edit::page_id(d, page)?))).expect("annotations")
}

fn saved_fields(bytes: &[u8]) -> Vec<Data> {
    let doc = anypage_pdf::PdfDoc::open(Arc::new(bytes.to_vec()), "").expect("saved PDF opens");
    doc.with_lo(|d| Ok(anypage_pdf::edit::fields_data(d))).expect("fields")
}

/// The side panel's toasts now.
fn toasts(p: &mut ScriptPage) -> Vec<String> {
    let u = p.cell_value("side.ui").unwrap_or(Data::Nil);
    u.get("toasts").items().iter().map(|t| t.get("text").str_or("").to_string()).collect()
}

fn side_state(p: &mut ScriptPage) -> Data {
    p.cell_value("side.state").expect("side.state")
}

fn annotation_count(p: &mut ScriptPage, page: u32) -> u64 {
    action(p, "list_annotations", json!({ "page": page }))["count"].as_u64().unwrap_or(0)
}

/// Pump the page for a while (jobs finish, their done callbacks run).
fn settle(p: &mut ScriptPage, ms: u64) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        p.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The next save request: (file name, bytes).
fn take_save(p: &mut ScriptPage) -> (u64, String, Arc<Vec<u8>>) {
    while let Some(r) = p.take_request() {
        if let PageRequest::SaveFile { id, name, bytes } = r {
            return (id, name, bytes);
        }
    }
    panic!("no save request");
}

// ---------------------------------------------------------------- the sample

const GEN_MAIN: &str = r#"
local S = import("sample")
page.action({ name = "build", run = function() return S.build():save() end })
page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
"#;

const GEN_JSON: &str = r#"{ "format": 1, "id": "pdf-agent.sample", "bounds": [0, 0, 10, 10],
    "capabilities": { "modules": ["pdf"] }, "limits": { "handler_ms": 5000, "load_ms": 5000, "memory_mb": 128 } }"#;

fn build_sample() -> Vec<u8> {
    let src = std::fs::read_to_string(package_dir().join("sample.luau")).expect("sample.luau");
    let pkg = Package::from_files("test:sample", [("page.json", GEN_JSON.to_string()), ("main.luau", GEN_MAIN.to_string()), ("sample.luau", src)]).expect("generator package");
    let mut p = ScriptPage::load(&pkg, &anypage_std::natives(), text(), wake()).expect("generator loads");
    let t0 = Instant::now();
    let d = act_data(&mut p, "build", Data::Nil);
    eprintln!("sample built in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
    d.as_bytes().expect("PDF bytes").to_vec()
}

fn check_sample(bytes: &[u8]) {
    assert!(bytes.starts_with(b"%PDF-"), "not a PDF");
    let doc = anypage_pdf::PdfDoc::open(Arc::new(bytes.to_vec()), "").expect("sample opens");
    let snap = doc.snapshot().expect("snapshot");
    assert_eq!(snap.page_count(), 4, "four pages");
    let t1 = snap.text(0).expect("page 1 text");
    let plain = t1.plain();
    assert!(plain.contains("AnyPage Quarterly Report"), "{plain}");
    assert!(plain.contains("營收"), "CJK text extracts: {plain}");
    assert!(!t1.tables.is_empty(), "the revenue table is detected");
    assert_eq!(t1.tables[0].columns.len(), 5, "columns {:?}", t1.tables[0].columns);
    let t2 = snap.text(1).expect("page 2 text");
    assert!(!t2.tables.is_empty(), "the expenses table is detected");
    let fields = doc.with_lo(|d| Ok(anypage_pdf::edit::fields_data(d))).expect("fields");
    let names: Vec<&str> = fields.iter().filter_map(|f| f.get("name").as_str()).collect();
    for n in ["reviewer", "department", "date", "amount", "approved", "comments"] {
        assert!(names.contains(&n), "field {n} missing: {names:?}");
    }
    let page4 = snap.text(3).expect("page 4").plain();
    assert!(page4.contains("Approval Form"), "{page4}");
}

#[test]
fn sample_pdf_is_generated_and_bundled() {
    let bytes = build_sample();
    check_sample(&bytes);
    let out = repo().join("target/test-output");
    let _ = std::fs::create_dir_all(&out);
    let _ = std::fs::write(out.join("pdf-agent-sample.pdf"), &bytes);
    let asset = package_dir().join("assets/sample.pdf");
    if !asset.is_file() || std::env::var("ANYPAGE_WRITE_SAMPLE").is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(asset.parent().expect("assets dir")).expect("assets dir");
        std::fs::write(&asset, &bytes).expect("write assets/sample.pdf");
        eprintln!("wrote {} ({} bytes)", asset.display(), bytes.len());
    }
    // the bundled file is what the page opens: it must hold the same content
    check_sample(&std::fs::read(&asset).expect("assets/sample.pdf"));
}

// ---------------------------------------------------------------- the semantic port

const ACTIONS: [&str; 9] = ["open_sample", "search", "highlight", "add_note", "fill_field", "extract_tables", "goto_page", "save_pdf", "summarize_page"];

#[test]
fn semantic_port_describes_and_acts() {
    let mut p = load_page();
    let d: Json = serde_json::from_str(&p.describe()).expect("describe is JSON");
    assert_eq!(d["page"]["id"], json!("com.anypage.pdf-agent"));
    let actions = d["actions"].as_array().expect("actions");
    for name in ACTIONS {
        let a = actions.iter().find(|a| a["name"] == json!(name)).unwrap_or_else(|| panic!("no action {name}"));
        assert!(a["description"].as_str().is_some_and(|s| s.len() > 10), "{name} has no description");
        assert_eq!(a["params"]["type"], json!("object"), "{name} params are a JSON schema: {a}");
    }
    let search = actions.iter().find(|a| a["name"] == json!("search")).expect("search");
    assert_eq!(search["params"]["properties"]["query"]["type"], json!("string"));
    assert_eq!(search["params"]["required"], json!(["query"]));

    let r = action(&mut p, "open_sample", json!({}));
    assert_eq!(r["pages"], json!(4));
    assert_eq!(r["fields"], json!(6));

    // search: count, pages, hits with context; the view goes to the first hit
    let r = action(&mut p, "search", json!({ "query": "revenue" }));
    let count = r["count"].as_u64().expect("count");
    assert!(count >= 6, "revenue hits: {r}");
    assert_eq!(r["pages"][0], json!(1));
    assert!(r["hits"][0]["context"].as_str().is_some_and(|c| c.to_lowercase().contains("revenue")), "{r}");
    assert_eq!(port(&mut p, json!({ "get": "query" }))["value"], json!("revenue"));
    let hits = ready(&mut p, "hits");
    assert_eq!(hits.items().len() as u64, count, "the background search agrees");

    // highlight every occurrence on page 2
    let r = action(&mut p, "highlight", json!({ "query": "revenue", "page": 2, "color": "green", "note": "agent" }));
    assert!(r["highlighted"].as_u64().unwrap_or(0) >= 1, "{r}");
    assert_eq!(r["pages"], json!([2]));
    let r = action(&mut p, "list_annotations", json!({ "page": 2 }));
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["type"] == json!("Highlight") && a["contents"] == json!("agent")), "{r}");

    // a note next to some text
    let r = action(&mut p, "add_note", json!({ "page": 1, "text": "Check the East figure", "near": "East" }));
    assert_eq!(r["page"], json!(1));
    let r = action(&mut p, "list_annotations", json!({ "page": 1 }));
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["type"] == json!("Text") && a["contents"] == json!("Check the East figure")), "{r}");

    // form fields, one and several at once; check boxes take words too
    let r = action(&mut p, "list_fields", json!({}));
    assert_eq!(r["count"], json!(6));
    action(&mut p, "fill_field", json!({ "name": "reviewer", "value": "Ada Lovelace" }));
    let r = action(&mut p, "fill_field", json!({ "values": { "department": "Finance", "approved": "yes", "amount": 4611.25 } }));
    assert_eq!(r["filled"], json!(["amount", "approved", "department"]));
    let r = action(&mut p, "list_fields", json!({}));
    let fields = r["fields"].as_array().expect("fields");
    let value = |n: &str| fields.iter().find(|f| f["name"] == json!(n)).map(|f| f["value"].clone()).unwrap_or(Json::Null);
    assert_eq!(value("reviewer"), json!("Ada Lovelace"));
    assert_eq!(value("department"), json!("Finance"));
    assert_eq!(value("approved"), json!("true"));
    assert_eq!(value("amount"), json!("4611.25"));
    let bad = port(&mut p, json!({ "action": "fill_field", "params": { "name": "department", "value": "Marketing" } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("not one of the options")), "{bad}");
    let bad = port(&mut p, json!({ "action": "fill_field", "params": { "name": "nope", "value": "x" } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("reviewer")), "the error lists the fields: {bad}");

    // pages
    let r = action(&mut p, "goto_page", json!({ "page": 3 }));
    assert_eq!(r["page"], json!(3));
    assert_eq!(port(&mut p, json!({ "get": "page" }))["value"], json!(3));
    let bad = port(&mut p, json!({ "action": "goto_page", "params": { "page": 9 } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("out of range")), "{bad}");
    assert_eq!(port(&mut p, json!({ "set": "page", "value": 2 }))["ok"], json!(true));

    // the computable form of a page
    let r = action(&mut p, "summarize_page", json!({ "page": 1 }));
    assert!(r["headings"].as_array().is_some_and(|h| h.iter().any(|x| x == "AnyPage Quarterly Report")), "{r}");
    assert_eq!(r["tables"][0]["columns"], json!("Region | Q1 | Q2 | Q3 | Revenue"), "{r}");
    assert!(r["lead"].as_str().is_some_and(|s| s.contains("Revenue grew")), "{r}");
    assert!(r["annotations"]["Text"].as_u64() == Some(1), "{r}");
    let r = action(&mut p, "summarize_page", json!({ "page": 4 }));
    assert!(r["form_fields"].as_array().is_some_and(|f| f.iter().any(|x| x == "reviewer = Ada Lovelace")), "{r}");

    let r = action(&mut p, "extract_tables", json!({ "page": 1 }));
    assert_eq!(r["count"], json!(1));
    assert!(r["tables"][0]["csv"].as_str().is_some_and(|c| c.starts_with("Region,Q1,Q2,Q3,Revenue") && c.contains("East,143,160,171,1620.75")), "{r}");
    let r = action(&mut p, "extract_tables", json!({ "all_pages": true }));
    assert_eq!(r["count"], json!(2), "{r}");
    // long results come in pages the port does not cut
    let r = action(&mut p, "get_table", json!({ "page": 1 }));
    assert_eq!(r["header"], json!("Region,Q1,Q2,Q3,Revenue"), "{r}");
    assert_eq!(r["rows"], json!(5));
    assert_eq!(r["lines"][2], json!("East,143,160,171,1620.75"), "{r}");
    assert!(r["next"].is_null());
    let r = action(&mut p, "get_text", json!({ "page": 1 }));
    let lines = r["lines"].as_array().expect("lines");
    assert!(lines.iter().any(|l| l == "AnyPage Quarterly Report"), "{r}");
    assert!(lines.iter().all(|l| l.is_string()), "lines are plain strings: {r}");
    let total = r["total_lines"].as_u64().expect("total_lines");
    assert!(total > 40 && lines.len() == 40 && r["next"] == json!(41), "{r}");
    let r = action(&mut p, "get_text", json!({ "page": 1, "from": 41 }));
    assert_eq!(r["lines"].as_array().map(|l| l.len() as u64), Some(total - 40), "{r}");
    assert!(r["next"].is_null(), "{r}");

    // xlsx: one sheet per table, through the host's save dialog
    let r = action(&mut p, "export_xlsx", json!({ "name": "tables" }));
    assert_eq!(r["name"], json!("tables.xlsx"));
    let (id, name, bytes) = take_save(&mut p);
    assert_eq!(name, "tables.xlsx");
    assert!(bytes.starts_with(b"PK"), "an xlsx (zip) file");
    let wb = anypage_sheet::Workbook::open_xlsx(&bytes, &anypage_sheet::BookOptions::default(), &|| false, &|_| {}).expect("xlsx opens");
    let book = wb.lock().expect("book");
    assert_eq!(book.sheet_names(), vec!["P1-T1".to_string(), "P2-T1".to_string()]);
    drop(book);
    p.host_result(id, HostResult::Saved { name: "C:/tmp/tables.xlsx".into() });

    // save the PDF: every edit is in the file
    let r = action(&mut p, "save_pdf", json!({}));
    assert_eq!(r["name"], json!("sample-edited.pdf"));
    let (id, _, pdf) = take_save(&mut p);
    assert!(pdf.starts_with(b"%PDF"));
    assert!(saved_annotations(&pdf, 2.0).iter().any(|a| a.get("type").as_str() == Some("Highlight")));
    assert!(saved_annotations(&pdf, 1.0).iter().any(|a| a.get("type").as_str() == Some("Text")));
    let f = saved_fields(&pdf);
    assert!(f.iter().any(|x| x.get("name").as_str() == Some("reviewer") && x.get("value").as_str() == Some("Ada Lovelace")));
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(true));
    p.host_result(id, HostResult::Saved { name: "C:/tmp/sample-edited.pdf".into() });
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(false));

    // the saved file dropped back in: parsed by the pdf.load job, edits intact
    p.file_drop("sample-edited.pdf", pdf.clone());
    pump_until(&mut p, "the dropped file to open", 20, |p| p.cell_value("doc_name").is_some_and(|d| d.as_str() == Some("sample-edited.pdf")));
    let r = action(&mut p, "list_annotations", json!({ "page": 2 }));
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["type"] == json!("Highlight")), "{r}");

    // rotate and delete
    let r = action(&mut p, "rotate_page", json!({ "page": 1 }));
    assert_eq!(r["rotation"], json!(90));
    let r = action(&mut p, "delete_page", json!({ "page": 3 }));
    assert_eq!(r["pages"], json!(3));
    let r = action(&mut p, "get_text", json!({ "page": 3 }));
    assert!(r["text"].as_str().is_some_and(|t| t.contains("Approval Form")), "page 4 moved up: {r}");
    let r = action(&mut p, "zoom", json!({ "zoom": 200 }));
    assert_eq!(r["zoom"], json!(2));

    // nothing failed along the way
    assert_eq!(p.handler_errors, 3, "only the three refused requests above count as handler errors");
    for x in [0.0, 1000.0] {
        let y = if x == 0.0 { 0.0 } else { 56.0 };
        no_error_card(&display_at(&mut p, x, y), "app");
    }
}

// ---------------------------------------------------------------- the assistant against a mock model

#[derive(Default)]
struct Seen {
    bodies: Vec<Json>,
}

fn sse(chunks: &[Json]) -> Vec<String> {
    let mut out: Vec<String> = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    out.push("data: [DONE]\n\n".into());
    out
}

fn tool_call_stream(id: &str, name: &str, args: &str) -> Vec<String> {
    // the arguments arrive in pieces, as models stream them
    let (a, b) = args.split_at(args.len() / 2);
    sse(&[
        json!({ "id": "c1", "model": "mock", "choices": [{ "index": 0, "delta": { "role": "assistant", "tool_calls": [{ "index": 0, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": a } }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": b } }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ])
}

fn text_stream(text: &str) -> Vec<String> {
    let mut chunks: Vec<Json> = text.split_inclusive(' ').map(|piece| json!({ "choices": [{ "index": 0, "delta": { "content": piece } }] })).collect();
    chunks.push(json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }));
    chunks.push(json!({ "choices": [], "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 } }));
    sse(&chunks)
}

fn serve_one(mut s: TcpStream, seen: &Mutex<Seen>, reply: &dyn Fn(&Json) -> Vec<String>) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut len = 0usize;
    let mut line = String::new();
    r.read_line(&mut line)?;
    assert!(line.starts_with("POST /v1/chat/completions"), "request line: {line}");
    loop {
        line.clear();
        r.read_line(&mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            assert!(!k.eq_ignore_ascii_case("authorization"), "no key may go to a local server");
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let j: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
    let chunks = reply(&j);
    seen.lock().expect("seen").bodies.push(j);
    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
    for c in chunks {
        s.write_all(format!("{:x}\r\n{c}\r\n", c.len()).as_bytes())?;
        s.flush()?;
        std::thread::sleep(Duration::from_millis(3));
    }
    s.write_all(b"0\r\n\r\n")?;
    s.flush()
}

/// An OpenAI-compatible mock: the reply depends on how many tool results the
/// conversation already holds.
fn mock_model() -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let seen = s2.clone();
            std::thread::spawn(move || {
                let _ = serve_one(conn, &seen, &|req: &Json| {
                    let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                    let tool_results = msgs.iter().filter(|m| m["role"] == json!("tool")).count();
                    match tool_results {
                        0 => tool_call_stream("call_search", "search", r#"{"query": "revenue"}"#),
                        1 => tool_call_stream("call_hl", "highlight", r#"{"query": "revenue", "color": "yellow"}"#),
                        // as a crafted PDF might ask it to: only the person can delete
                        2 => tool_call_stream("call_del", "delete_page", r#"{"page": 2}"#),
                        _ => text_stream("已把 revenue 全部標示起來，分布在第 1、2、3 頁。 Done."),
                    }
                });
            });
        }
    });
    (port, seen)
}

#[test]
fn assistant_runs_streamed_tool_calls_and_shows_the_steps() {
    let (mock_port, seen) = mock_model();
    let mut p = load_page();
    let base = format!("http://127.0.0.1:{mock_port}/v1");
    let r = action(&mut p, "assistant.config", json!({ "base_url": base, "model": "mock-model" }));
    assert_eq!(r["allowed"], json!(true), "{r}");

    let before = action(&mut p, "list_annotations", json!({ "page": 1 }))["count"].as_u64().unwrap_or(0);
    let r = action(&mut p, "assistant.ask", json!({ "text": "把所有 revenue 標示起來" }));
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(port(&mut p, json!({ "get": "assistant.busy" }))["value"], json!(true));

    let t0 = Instant::now();
    pump_until(&mut p, "the assistant to finish", 30, |p| p.cell_value("assistant.busy").is_some_and(|b| b.as_bool() == Some(false)));
    eprintln!("assistant loop (4 streamed replies, 3 tool calls) took {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);

    // the steps: the question, the tools (ran, succeeded), the answer
    let steps = p.cell_value("assistant.steps").expect("steps");
    let kinds: Vec<String> = steps.items().iter().map(|s| format!("{}:{}", s.get("kind").str_or(""), s.get("name").str_or(""))).collect();
    assert_eq!(kinds, vec!["user:", "tool:search", "tool:highlight", "tool:delete_page", "assistant:"], "steps: {steps:?}");
    for s in &steps.items()[1..4] {
        assert_eq!(s.get("ok").as_bool(), Some(true), "tool step failed: {s:?}");
    }
    assert!(steps.items()[1].get("result").str_or("").contains("revenue"), "search result shown: {:?}", steps.items()[1]);
    assert!(steps.items()[4].get("text").str_or("").contains("全部標示"), "{:?}", steps.items()[4]);

    // delete_page from the model only asked the person: nothing is deleted yet
    assert!(steps.items()[3].get("result").str_or("").contains("確認"), "{:?}", steps.items()[3]);
    assert_eq!(port(&mut p, json!({ "get": "info" }))["value"]["pages"], json!(4), "no page was deleted");
    let st = side_state(&mut p);
    assert_eq!(st.get("confirm_delete").num(), Some(2.0), "{st:?}");
    assert_eq!(st.get("delete_from").as_str(), Some("assistant"), "{st:?}");
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t.contains("助理要刪除頁面")), "the person sees the request: {shown:?}");
    action(&mut p, "side.do", json!({ "id": "delete-cancel", "op": "click" }));
    assert!(side_state(&mut p).get("confirm_delete").is_nil());
    assert_eq!(port(&mut p, json!({ "get": "info" }))["value"]["pages"], json!(4), "cancelled: still 4 pages");

    // the tools really ran: highlights on every page with revenue
    for pg in [1, 2, 3] {
        let r = action(&mut p, "list_annotations", json!({ "page": pg }));
        let n = r["annotations"].as_array().expect("list").iter().filter(|a| a["type"] == json!("Highlight")).count();
        assert!(n >= 1, "page {pg} has no highlight: {r}");
    }
    let after = action(&mut p, "list_annotations", json!({ "page": 1 }))["count"].as_u64().unwrap_or(0);
    assert!(after > before);

    // what went over the wire: tools from page.actions(), tool results back
    let bodies = seen.lock().expect("seen").bodies.clone();
    assert_eq!(bodies.len(), 4, "four requests");
    let tools = bodies[0]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    for name in ACTIONS.iter().filter(|n| **n != "open_sample") {
        assert!(names.contains(name), "tool {name} missing: {names:?}");
    }
    // replacing the document would drop the person's edits: never the model's
    // call; nor is moving the person's camera
    for name in ["open_sample", "open_file", "set_layout"] {
        assert!(!names.contains(&name), "{name} is offered to the model: {names:?}");
    }
    assert!(!names.iter().any(|n| n.contains('.')), "dotted actions are not offered: {names:?}");
    let search = tools.iter().find(|t| t["function"]["name"] == json!("search")).expect("search tool");
    assert_eq!(search["function"]["parameters"]["required"], json!(["query"]));
    assert_eq!(bodies[0]["messages"][0]["role"], json!("system"));
    assert_eq!(bodies[0]["stream"], json!(true));
    let last = bodies[2]["messages"].as_array().expect("messages");
    let assistant_calls: Vec<&Json> = last.iter().filter(|m| m["role"] == json!("assistant")).collect();
    assert_eq!(assistant_calls[0]["tool_calls"][0]["id"], json!("call_search"));
    assert_eq!(assistant_calls[0]["tool_calls"][0]["function"]["arguments"], json!(r#"{"query": "revenue"}"#));
    let tool_msgs: Vec<&Json> = last.iter().filter(|m| m["role"] == json!("tool")).collect();
    assert_eq!(tool_msgs[0]["tool_call_id"], json!("call_search"));
    let search_result: Json = serde_json::from_str(tool_msgs[0]["content"].as_str().expect("content")).expect("tool result is JSON");
    assert!(search_result["count"].as_u64().unwrap_or(0) >= 6, "{search_result}");
    let hl_result: Json = serde_json::from_str(tool_msgs[1]["content"].as_str().expect("content")).expect("tool result is JSON");
    assert_eq!(hl_result["highlighted"], search_result["count"], "highlighted every hit");

    // the chat panel shows every step
    let cmds = side_tab(&mut p, 1);
    let shown = texts(&cmds);
    for want in ["search", "highlight", "工具"] {
        assert!(shown.iter().any(|t| t == want), "{want} not shown: {shown:?}");
    }
    assert!(shown.iter().any(|t| t.contains("把所有 revenue")), "the question is shown: {shown:?}");
    assert!(shown.iter().any(|t| t.contains("全部標示")), "the answer is shown: {shown:?}");

    let r = action(&mut p, "assistant.status", json!({}));
    assert_eq!(r["busy"], json!(false));
    assert_eq!(r["steps"], json!(5));
    assert_eq!(p.handler_errors, 0, "no handler failed");
}

/// A model that always answers with a short text.
fn text_model() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
    let port = listener.local_addr().expect("addr").port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let _ = serve_one(conn, &seen, &|_| text_stream("這是一段回答。 This is an answer that takes a few lines in the chat panel, as real answers do."));
            });
        }
    });
    port
}

#[test]
fn long_chats_stay_cheap_to_draw() {
    let mock = text_model();
    let mut p = load_page();
    action(&mut p, "assistant.config", json!({ "base_url": format!("http://127.0.0.1:{mock}/v1"), "model": "m" }));
    let mut times = vec![];
    for round in 1..=60 {
        let r = action(&mut p, "assistant.ask", json!({ "text": format!("question {round}") }));
        assert_eq!(r["ok"], json!(true), "{r}");
        pump_until(&mut p, "an answer", 20, |p| p.cell_value("assistant.busy").is_some_and(|b| b.as_bool() == Some(false)));
        if round % 20 == 0 {
            // a new draft rebuilds the side view: time its draw
            let mut best = f64::MAX;
            for k in 0..3 {
                action(&mut p, "side.do", json!({ "id": "ask", "op": "set", "value": format!("draft {round} {k}") }));
                let t0 = Instant::now();
                display_at(&mut p, 1000.0, 56.0);
                best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
            }
            times.push((round * 2, best));
        }
    }
    eprintln!("side view draw by steps: {times:?}");
    // only the newest 30 steps are laid out, so 120 steps cost what 40 do
    let more = |p: &mut ScriptPage| {
        let d = action(p, "side.describe", json!({ "within": "chat-log", "types": "button" }));
        d["nodes"].as_array().and_then(|n| n.iter().find(|n| n["id"] == json!("chat-more")).cloned()).unwrap_or(Json::Null).to_string()
    };
    let laid_out = |p: &mut ScriptPage| action(p, "side.describe", json!({ "within": "chat-log", "types": "text" }))["total"].as_u64().unwrap_or(0);
    assert!(more(&mut p).contains("顯示較早的步驟（還有 90 個）"), "{}", more(&mut p));
    let n30 = laid_out(&mut p);
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t == "question 60"), "the newest step is in view: {shown:?}");
    action(&mut p, "side.do", json!({ "id": "chat-more", "op": "click" }));
    assert!(more(&mut p).contains("還有 60 個"), "{}", more(&mut p));
    let n60 = laid_out(&mut p);
    eprintln!("text elements in the chat: {n30} (30 steps), {n60} (60 steps)");
    assert!(n30 < 80 && n60 > n30, "{n30} {n60}");
    let (first, last) = (times[0].1, times[times.len() - 1].1);
    assert!(last < first * 2.0 + 5.0, "the draw grows with the log: {times:?}");
    no_error_card(&display_at(&mut p, 1000.0, 56.0), "long chat");
}

// ---------------------------------------------------------------- people's edits are not lost

#[test]
fn unsaved_edits_are_protected() {
    let mut p = load_page();
    action(&mut p, "highlight", json!({ "query": "revenue" }));
    let marks = annotation_count(&mut p, 1);
    assert!(marks >= 1);
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(true));

    // Esc first clears the search; then, with edits unsaved, it is always
    // taken (a host may treat an Esc nobody takes as "leave": Android's back)
    assert!(p.key("escape"));
    assert_eq!(port(&mut p, json!({ "get": "query" }))["value"], json!(""));
    for _ in 0..3 {
        assert!(p.key("escape"), "Esc must not reach the host while edits are unsaved");
    }
    assert_eq!(annotation_count(&mut p, 1), marks, "Esc drops nothing");

    // an agent's open_sample is refused unless it says discard
    let bad = port(&mut p, json!({ "action": "open_sample", "params": {} }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("unsaved") && e.contains("discard")), "{bad}");
    assert_eq!(annotation_count(&mut p, 1), marks, "the edits are still there");

    // the 範例 button asks; 取消 keeps everything
    action(&mut p, "toolbar.do", json!({ "id": "sample", "op": "click" }));
    assert_eq!(side_state(&mut p).get("confirm_replace").get("name").as_str(), Some("sample.pdf"));
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t.contains("捨棄未儲存的修改")), "{shown:?}");
    action(&mut p, "side.do", json!({ "id": "replace-cancel", "op": "click" }));
    assert!(side_state(&mut p).get("confirm_replace").is_nil());
    assert_eq!(annotation_count(&mut p, 1), marks);
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(true));

    // a drop asks too; 捨棄並開啟 opens it
    let sample = std::fs::read(package_dir().join("assets/sample.pdf")).expect("sample");
    p.file_drop("other.pdf", Arc::new(sample.clone()));
    settle(&mut p, 100);
    assert_eq!(p.cell_value("doc_name").and_then(|d| d.as_str().map(str::to_string)).as_deref(), Some("sample.pdf"), "not replaced yet");
    assert_eq!(side_state(&mut p).get("confirm_replace").get("name").as_str(), Some("other.pdf"));
    action(&mut p, "side.do", json!({ "id": "replace-ok", "op": "click" }));
    pump_until(&mut p, "other.pdf to open", 20, |p| p.cell_value("doc_name").is_some_and(|d| d.as_str() == Some("other.pdf")));
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(false));
    assert_eq!(annotation_count(&mut p, 1), 0, "a fresh copy");

    // 先儲存…: the edits are saved, then the new file opens
    action(&mut p, "highlight", json!({ "query": "East" }));
    p.file_drop("third.pdf", Arc::new(sample));
    settle(&mut p, 50);
    action(&mut p, "side.do", json!({ "id": "replace-save", "op": "click" }));
    let (id, name, bytes) = take_save(&mut p);
    assert_eq!(name, "other-edited.pdf");
    assert!(saved_annotations(&bytes, 1.0).iter().any(|a| a.get("type").as_str() == Some("Highlight")), "the edit is in the saved file");
    assert_eq!(p.cell_value("doc_name").and_then(|d| d.as_str().map(str::to_string)).as_deref(), Some("other.pdf"), "waits for the save");
    p.host_result(id, HostResult::Saved { name: "C:/tmp/other-edited.pdf".into() });
    pump_until(&mut p, "third.pdf to open", 20, |p| p.cell_value("doc_name").is_some_and(|d| d.as_str() == Some("third.pdf")));

    // a new edit: Esc is taken again
    action(&mut p, "add_note", json!({ "text": "x", "page": 1 }));
    port(&mut p, json!({ "set": "query", "value": "" }));
    assert!(p.key("escape"));

    // the open_sample refusal holds for this file too, then an agent may
    // discard on purpose
    let bad = port(&mut p, json!({ "action": "open_sample", "params": { "discard": false } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("unsaved")), "{bad}");
    assert_eq!(p.cell_value("doc_name").and_then(|d| d.as_str().map(str::to_string)).as_deref(), Some("third.pdf"));
    let r = action(&mut p, "open_sample", json!({ "discard": true }));
    assert_eq!(r["pages"], json!(4));
    assert_eq!(port(&mut p, json!({ "get": "modified" }))["value"], json!(false));
    assert!(p.key("escape") == false, "nothing unsaved: Esc goes to the host");

    assert_eq!(p.handler_errors, 2, "only the refused open_sample calls");
    for (x, y) in [(0.0, 0.0), (1000.0, 56.0), (176.0, 56.0)] {
        no_error_card(&display_at(&mut p, x, y), "after the replacements");
    }
}

// ---------------------------------------------------------------- agent-written values

#[test]
fn agent_writable_cells_are_checked() {
    let mut p = load_page();
    // values of the wrong type, as an agent might write them
    for (cell, v) in [("scroll", json!(5)), ("zoom", json!("huge")), ("tool", json!({ "x": 1 })), ("query", json!([1, 2])), ("page", json!("two"))] {
        assert_eq!(port(&mut p, json!({ "set": cell, "value": v }))["ok"], json!(true), "set {cell}");
    }
    let viewer = texts(&display_at(&mut p, 176.0, 56.0));
    assert!(viewer.iter().any(|t| t.contains("第 1 / 4 頁")), "{viewer:?}");
    no_error_card(&display_at(&mut p, 0.0, 0.0), "toolbar with bad cells");
    // every operation that reads them still works
    let r = action(&mut p, "goto_page", json!({ "page": 3 }));
    assert_eq!(r["page"], json!(3));
    let r = action(&mut p, "zoom", json!({ "zoom": 150 }));
    assert_eq!(r["zoom"], json!(1.5));
    assert!(p.key("right"));
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(4.0));
    port(&mut p, json!({ "set": "scroll", "value": { "x": "left", "y": null } }));
    assert!(p.key("down"));
    assert!(p.cell_value("scroll").expect("scroll").get("y").num().unwrap_or(0.0) > 0.0);
    assert!(p.wheel(500.0, 500.0, 0.0, -1.0));
    action(&mut p, "toolbar.do", json!({ "id": "zoom-in", "op": "click" }));
    // a percentage written straight into the zoom cell reads as one
    port(&mut p, json!({ "set": "zoom", "value": 250 }));
    let viewer = texts(&display_at(&mut p, 176.0, 56.0));
    assert!(viewer.iter().any(|t| t.contains("250%")), "{viewer:?}");
    // a selection drag with a bogus tool selects
    port(&mut p, json!({ "set": "tool", "value": 7 }));
    port(&mut p, json!({ "set": "zoom", "value": "fit" }));
    let (x, y) = world_of(&mut p, 60.0, 92.0);
    p.pointer_down(x, y);
    p.pointer_drag(x + 200.0, y + 4.0);
    p.pointer_up();

    // the side panel's own state, written with the wrong types
    let garbage = json!({ "tab": "z", "q": 5, "draft": [1], "form": "x", "note_at": { "page": "a" }, "confirm_delete": "q", "chat_more": "many", "confirm_replace": 3 });
    assert_eq!(port(&mut p, json!({ "set": "side.state", "value": garbage }))["ok"], json!(true));
    for tab in 1..=5 {
        side_tab(&mut p, tab);
    }
    // an endpoint that is not text: refused, no error card
    port(&mut p, json!({ "set": "assistant.base_url", "value": 5 }));
    let r = action(&mut p, "assistant.ask", json!({ "text": "hi" }));
    assert_eq!(r["ok"], json!(false), "{r}");
    side_tab(&mut p, 1);

    // search: an oversized query is refused for what it is, and not kept
    action(&mut p, "search", json!({ "query": "revenue" }));
    let long = "x".repeat(5000);
    let bad = port(&mut p, json!({ "action": "search", "params": { "query": long } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("too long")), "{bad}");
    assert_eq!(port(&mut p, json!({ "get": "query" }))["value"], json!("revenue"));
    // highlight with no hits says so
    let r = action(&mut p, "highlight", json!({ "query": "zebra" }));
    assert_eq!(r["highlighted"], json!(0));
    assert!(r["summary"].as_str().is_some_and(|s| s.contains("沒有找到") && !s.contains("第  頁")), "{r}");

    assert_eq!(p.handler_errors, 1, "only the oversized search");
    // the narrow layout's toolbar and viewer read the same cells
    port(&mut p, json!({ "set": "scroll", "value": "down" }));
    port(&mut p, json!({ "set": "tool", "value": false }));
    let viewer = texts(&display_at(&mut p, 1800.0, 112.0));
    assert!(viewer.iter().any(|t| t.contains("第 1 / 4 頁") && t.contains("選取")), "{viewer:?}");
    for (x, y) in [(0.0, 0.0), (1000.0, 56.0), (176.0, 56.0), (0.0, 56.0), (1800.0, 0.0), (1800.0, 112.0)] {
        no_error_card(&display_at(&mut p, x, y), "after bad values");
    }
}

// ---------------------------------------------------------------- fidelity

#[test]
fn tables_keep_their_decimals_and_damaged_files_open_read_only() {
    let mut p = load_page();
    // the expenses table writes 820.00: integer-valued cells keep their places
    let r = action(&mut p, "extract_tables", json!({ "page": 2 }));
    let csv = r["tables"][0]["csv"].as_str().expect("csv").to_string();
    assert!(csv.contains("Salaries,820.00,845.20,-25.20"), "{csv}");
    let r = action(&mut p, "get_table", json!({ "page": 1 }));
    assert_eq!(r["lines"][0], json!("North,120,135,142,1250.00"), "{r}");
    action(&mut p, "goto_page", json!({ "page": 2 }));
    let _ = ready(&mut p, "extract");
    let shown = texts(&side_tab(&mut p, 5));
    assert!(shown.iter().any(|t| t == "820.00"), "the tables tab: {shown:?}");

    // a truncated file that still renders opens read-only, edits refused
    let sample = std::fs::read(package_dir().join("assets/sample.pdf")).expect("sample");
    let mut read_only = None;
    for pct in [98, 95, 90, 75, 50] {
        let bytes = sample[..sample.len() * pct / 100].to_vec();
        let name = format!("cut{pct}.pdf");
        p.file_drop(&name, Arc::new(bytes.clone()));
        pump_until(&mut p, "the drop to settle", 20, |p| p.cell_value("loading").is_some_and(|l| l.is_nil()));
        settle(&mut p, 50);
        let info = ready(&mut p, "info");
        let now = info.get("name").str_or("").to_string();
        let ro = info.get("read_only").as_bool() == Some(true);
        eprintln!("{name}: open as {now:?}, read-only {ro}, toasts {:?}", toasts(&mut p));
        if now == name && ro && read_only.is_none() {
            read_only = Some((name.clone(), bytes));
        }
        no_error_card(&display_at(&mut p, 1000.0, 56.0), &name);
        no_error_card(&display_at(&mut p, 176.0, 56.0), &name);
    }
    let (name, bytes) = read_only.expect("a truncated sample that renders opens read-only");
    p.file_drop(&name, Arc::new(bytes));
    pump_until(&mut p, "the read-only file", 20, |p| p.cell_value("doc_name").is_some_and(|d| d.as_str() == Some(name.as_str())) && p.cell_value("read_only").is_some_and(|r| !r.is_nil()));
    let bad = port(&mut p, json!({ "action": "highlight", "params": { "query": "revenue" } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("read-only")), "{bad}");
    let r = action(&mut p, "search", json!({ "query": "revenue" }));
    assert!(r["count"].as_u64().unwrap_or(0) >= 1, "it can still be searched: {r}");
    action(&mut p, "summarize_page", json!({ "page": 1 }));
    let tb = action(&mut p, "toolbar.describe", json!({ "types": "button", "limit": 60 }));
    let disabled = |id: &str| tb["nodes"].as_array().expect("nodes").iter().find(|n| n["id"] == json!(id)).map(|n| n["disabled"].clone());
    assert_eq!(disabled("save"), Some(json!(true)), "{tb}");
    assert_eq!(disabled("rotate"), Some(json!(true)));
    let shown = texts(&display_at(&mut p, 0.0, 0.0));
    assert!(shown.iter().any(|t| t == "唯讀"), "{shown:?}");
}

#[test]
fn assistant_reports_an_unreachable_model() {
    let mut p = load_page();
    // a port nothing listens on: the error is a step, not a stuck "busy"
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead = l.local_addr().expect("addr").port();
    drop(l);
    action(&mut p, "assistant.config", json!({ "base_url": format!("http://127.0.0.1:{dead}/v1") }));
    action(&mut p, "assistant.ask", json!({ "text": "hello" }));
    pump_until(&mut p, "the error", 30, |p| p.cell_value("assistant.busy").is_some_and(|b| b.as_bool() == Some(false)));
    let steps = p.cell_value("assistant.steps").expect("steps");
    assert_eq!(steps.items().last().map(|s| s.get("kind").str_or("").to_string()), Some("error".to_string()), "{steps:?}");
    // a host the page does not declare is refused before anything is sent
    action(&mut p, "assistant.config", json!({ "base_url": "https://example.com/v1" }));
    let r = port(&mut p, json!({ "action": "assistant.ask", "params": { "text": "hello" } }));
    assert_eq!(r["result"]["ok"], json!(false), "{r}");
    no_error_card(&side_tab(&mut p, 1), "assistant tab with errors");
}

// ---------------------------------------------------------------- people: pointer, keys, wheel

/// World point of page point (x, y) on the page shown.
fn world_of(p: &mut ScriptPage, x: f64, y: f64) -> (f32, f32) {
    let vp = p.cell_value("view_params").expect("view_params");
    let r = vp.get("rect").items().iter().filter_map(Data::num).collect::<Vec<f64>>();
    let s = r[2] / 612.0;
    ((r[0] + x * s) as f32, (r[1] + y * s) as f32)
}

#[test]
fn people_select_highlight_and_navigate() {
    let mut p = load_page();
    // where "Revenue grew" is: select it through the port to learn its rect
    let r = action(&mut p, "select_text", json!({ "query": "Revenue grew", "page": 1 }));
    assert_eq!(r["page"], json!(1));
    let sel = p.cell_value("selection").expect("selection");
    // Data::at is 1-based, like Luau
    let rect: Vec<f64> = sel.get("rects").at(1).items().iter().filter_map(Data::num).collect();
    assert_eq!(rect.len(), 4, "selection rect: {sel:?}");
    port(&mut p, json!({ "set": "page", "value": 1 }));

    // drag across the words with the select tool
    let (x0, y0) = world_of(&mut p, rect[0] + 2.0, rect[1] + rect[3] / 2.0);
    let (x1, y1) = world_of(&mut p, rect[0] + rect[2] - 2.0, rect[1] + rect[3] / 2.0);
    assert_eq!(p.pointer_down(x0, y0), PointerResponse::Captured);
    p.pointer_drag((x0 + x1) / 2.0, y0);
    p.pointer_drag(x1, y1);
    p.pointer_up();
    let sel = p.cell_value("selection").expect("selection");
    assert_eq!(sel.get("text").as_str(), Some("Revenue grew"), "{sel:?}");
    let r = action(&mut p, "get_selection", json!({}));
    assert_eq!(r["text"], json!("Revenue grew"));
    // the assistant tab stays; a bar of actions appears over the selection
    assert_eq!(p.cell_value("side.state").expect("state").get("tab").num(), Some(1.0));
    let viewer = display_at(&mut p, 176.0, 56.0);
    let shown = texts(&viewer);
    for want in ["複製", "螢光標示", "加註記", "問助理"] {
        assert!(shown.iter().any(|t| t == want), "{want} missing from the selection bar: {shown:?}");
    }
    let n0 = action(&mut p, "list_annotations", json!({ "page": 1 }))["count"].as_u64().unwrap_or(0);
    // its 螢光標示 button writes the highlight
    let hl = viewer.items().iter().find(|c| c.at(1).as_str() == Some("text") && c.at(6).as_str() == Some("螢光標示")).expect("button text").clone();
    let (bx, by) = (176.0 + hl.at(2).num_or(0.0) as f32 + 10.0, 56.0 + hl.at(3).num_or(0.0) as f32 + 6.0);
    assert_eq!(p.pointer_down(bx, by), PointerResponse::Handled);
    p.pointer_up();
    let r = action(&mut p, "list_annotations", json!({ "page": 1 }));
    assert_eq!(r["count"].as_u64(), Some(n0 + 1), "{r}");
    assert!(p.cell_value("selection").is_some_and(|s| s.is_nil()), "the selection is used up");

    // from another tab, a finished selection opens the text tab
    port(&mut p, json!({ "set": "side.state", "value": { "tab": 2 } }));
    p.pointer_down(x0, y0);
    p.pointer_drag(x1, y1);
    p.pointer_up();
    assert_eq!(p.cell_value("side.state").expect("state").get("tab").num(), Some(3.0));
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t.contains("選取的文字")), "{shown:?}");
    // Ctrl+C copies it through the host
    assert!(p.key("ctrl+c"));
    let copied = std::iter::from_fn(|| p.take_request()).find_map(|r| if let PageRequest::Copy(t) = r { Some(t) } else { None });
    assert_eq!(copied.as_deref(), Some("Revenue grew"));

    // the highlighter: dragging writes a highlight annotation
    let n0 = action(&mut p, "list_annotations", json!({ "page": 1 }))["count"].as_u64().unwrap_or(0);
    port(&mut p, json!({ "set": "tool", "value": "highlight" }));
    p.pointer_down(x0, y0);
    p.pointer_drag(x1, y1);
    p.pointer_up();
    let r = action(&mut p, "list_annotations", json!({ "page": 1 }));
    assert_eq!(r["count"].as_u64(), Some(n0 + 1), "{r}");
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["contents"] == json!("Revenue grew")), "{r}");

    // the note tool opens the note dialog at the click
    port(&mut p, json!({ "set": "tool", "value": "note" }));
    let (nx, ny) = world_of(&mut p, 300.0, 700.0);
    assert_eq!(p.pointer_down(nx, ny), PointerResponse::Handled);
    p.pointer_up();
    let st = p.cell_value("side.state").expect("state");
    assert_eq!(st.get("note_at").get("page").num(), Some(1.0), "{st:?}");
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t.contains("新增註記")), "dialog shown: {shown:?}");
    // typed into and confirmed through the UI library's own agent actions
    action(&mut p, "side.do", json!({ "id": "note-text", "op": "set", "value": "Typed note" }));
    action(&mut p, "side.do", json!({ "id": "note-add", "op": "click" }));
    let r = action(&mut p, "list_annotations", json!({ "page": 1 }));
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["type"] == json!("Text") && a["contents"] == json!("Typed note")), "{r}");
    assert!(p.cell_value("side.state").expect("state").get("note_at").is_nil(), "dialog closed");

    // keys turn pages; a thumbnail click goes to its page; the wheel turns pages at fit zoom
    port(&mut p, json!({ "set": "tool", "value": "select" }));
    assert!(p.key("right"));
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(2.0));
    assert!(p.key("end"));
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(4.0));
    assert_eq!(p.pointer_down(88.0, 56.0 + 16.0 + 60.0), PointerResponse::Handled);
    p.pointer_up();
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(1.0));
    assert!(p.wheel(500.0, 500.0, 0.0, -1.0));
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(2.0));
    // zoomed in, the wheel scrolls the page instead
    action(&mut p, "zoom", json!({ "zoom": 200 }));
    p.wheel(500.0, 500.0, 0.0, -1.0);
    let sc = p.cell_value("scroll").expect("scroll");
    assert!(sc.get("y").num().unwrap_or(0.0) > 0.0, "{sc:?}");
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(2.0));
    // the camera never pans or zooms: every press and wheel on the page is taken
    assert_ne!(p.pointer_down(1500.0, 990.0), PointerResponse::Ignored);
    p.pointer_up();
    assert!(p.wheel(1300.0, 20.0, 0.0, 1.0));

    // every panel draws, every side tab renders without the error card
    for tab in 1..=5 {
        let cmds = side_tab(&mut p, tab);
        assert!(!texts(&cmds).is_empty(), "tab {tab} is empty");
    }
    no_error_card(&display_at(&mut p, 0.0, 0.0), "toolbar");
    let viewer = texts(&display_at(&mut p, 176.0, 56.0));
    assert!(viewer.iter().any(|t| t.contains("第 2 / 4 頁")), "{viewer:?}");
    assert_eq!(p.handler_errors, 0, "no handler failed");
}

// ---------------------------------------------------------------- narrow windows and phones

/// The rects of the camera fits the page asked for since the last call.
fn fits(p: &mut ScriptPage) -> Vec<(f32, f32, f32, f32)> {
    std::iter::from_fn(|| p.take_request()).filter_map(|r| if let PageRequest::FitRect(r) = r { Some((r.x, r.y, r.w, r.h)) } else { None }).collect()
}

fn layout(p: &mut ScriptPage) -> String {
    p.cell_value("layout").and_then(|d| d.as_str().map(str::to_string)).unwrap_or_default()
}

/// When the panel at (x, y) must be drawn again because of the clock alone
/// (infinity: only when an input changes).
fn panel_expiry(p: &mut ScriptPage, x: f32, y: f32) -> f64 {
    let raster = p.spec().panels.iter().find(|pa| pa.rect.x == x && pa.rect.y == y).unwrap_or_else(|| panic!("no panel at {x},{y}")).cell;
    p.parts().0.next_expiry(&[raster])
}

/// World point of page point (x, y) in the narrow viewer.
fn narrow_world_of(p: &mut ScriptPage, x: f64, y: f64) -> (f32, f32) {
    let vp = p.cell_value("nview_params").expect("nview_params");
    let r = vp.get("rect").items().iter().filter_map(Data::num).collect::<Vec<f64>>();
    let s = r[2] / 612.0;
    ((r[0] + x * s) as f32, (r[1] + y * s) as f32)
}

#[test]
fn narrow_windows_get_the_narrow_layout() {
    const NBAR: (f32, f32) = (1800.0, 0.0);
    const NVIEW: (f32, f32) = (1800.0, 112.0);
    let mut p = load_page();
    assert_eq!(layout(&mut p), "wide", "a desktop starts wide");

    // a phone in portrait (logical pixels): the narrow column, fitted
    p.viewport(411.0, 869.0, 2.625);
    assert_eq!(layout(&mut p), "narrow");
    // fitted by width, top at the window's top (a taller window shows more below)
    let (x, y, w, h) = fits(&mut p).last().copied().expect("a fit");
    assert_eq!((x, y, w), (1800.0, 0.0, 420.0));
    assert!((h - 420.0 * 821.0 / 363.0).abs() < 1.0 && h >= 940.0, "fit height {h}");
    // the wide toolbar is out of use (the narrow one has its controls)
    assert!(texts(&display_at(&mut p, 0.0, 0.0)).is_empty(), "the wide toolbar is blank in the narrow layout");
    let bar = display_at(&mut p, NBAR.0, NBAR.1);
    no_error_card(&bar, "narrow toolbar");
    let shown = texts(&bar);
    for want in ["範例", "面板 ›", "1 / 4", "選取", "螢光"] {
        assert!(shown.iter().any(|t| t == want), "{want} missing from the narrow toolbar: {shown:?}");
    }
    let shown = texts(&display_at(&mut p, NVIEW.0, NVIEW.1));
    assert!(shown.iter().any(|t| t.contains("第 1 / 4 頁")), "{shown:?}");
    // it reads the clock (for its message line) but is not drawn every frame
    assert_eq!(panel_expiry(&mut p, NVIEW.0, NVIEW.1), f64::INFINITY);
    let vp = p.cell_value("nview_params").expect("nview_params");
    let r: Vec<f64> = vp.get("rect").items().iter().filter_map(Data::num).collect();
    assert!(r[0] >= 1800.0 && r[0] + r[2] <= 2220.0 && r[2] > 300.0, "the page fills the narrow viewer: {r:?}");

    // people select in the narrow viewer as in the wide one
    let r = action(&mut p, "select_text", json!({ "query": "Revenue grew", "page": 1 }));
    assert_eq!(r["page"], json!(1));
    let sel = p.cell_value("selection").expect("selection");
    let rect: Vec<f64> = sel.get("rects").at(1).items().iter().filter_map(Data::num).collect();
    assert!(p.key("escape"), "Esc clears the selection");
    let (x0, y0) = narrow_world_of(&mut p, rect[0] + 2.0, rect[1] + rect[3] / 2.0);
    let (x1, y1) = narrow_world_of(&mut p, rect[0] + rect[2] - 2.0, rect[1] + rect[3] / 2.0);
    assert_eq!(p.pointer_down(x0, y0), PointerResponse::Captured);
    p.pointer_drag(x1, y1);
    p.pointer_up();
    assert_eq!(p.cell_value("selection").expect("selection").get("text").as_str(), Some("Revenue grew"));
    let shown = texts(&display_at(&mut p, NVIEW.0, NVIEW.1));
    assert!(shown.iter().any(|t| t == "螢光標示"), "the selection bar fits the narrow viewer: {shown:?}");
    assert!(p.key("escape"), "Esc clears the selection");

    // presses and the wheel that miss the page and the controls move the camera
    assert_eq!(p.pointer_down(1700.0, 500.0), PointerResponse::Ignored);
    p.pointer_up();
    assert_eq!(p.pointer_down(NVIEW.0 + 4.0, NVIEW.1 + 4.0), PointerResponse::Ignored, "the empty view around a page at fit");
    p.pointer_up();
    assert!(!p.wheel(1700.0, 500.0, 0.0, 1.0));
    // the controls still work: the toolbar turns the page
    action(&mut p, "nbar.do", json!({ "id": "next", "op": "click" }));
    assert_eq!(p.cell_value("page").and_then(|d| d.num()), Some(2.0));
    // an agent's action shows as a line over the narrow viewer (the side
    // panel's toasts are out of view)
    action(&mut p, "goto_page", json!({ "page": 3 }));
    let shown = texts(&display_at(&mut p, NVIEW.0, NVIEW.1));
    assert!(shown.iter().any(|t| t.contains("代理程式")), "{shown:?}");
    let ends = panel_expiry(&mut p, NVIEW.0, NVIEW.1);
    assert!(ends.is_finite(), "the message line goes away by itself: {ends}");

    // 面板: the camera moves to the side panel, which has a way back
    action(&mut p, "nbar.do", json!({ "id": "panel", "op": "click" }));
    assert_eq!(layout(&mut p), "panel");
    let (x, y, w, h) = fits(&mut p).last().copied().expect("a fit");
    assert_eq!((x, y, w), (1000.0, 56.0, 600.0));
    assert!(h >= 944.0, "fit height {h}");
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(shown.iter().any(|t| t == "‹ 頁面"), "{shown:?}");
    for tab in 1..=5 {
        side_tab(&mut p, tab);
    }
    // a search hit chosen there shows the page
    action(&mut p, "search", json!({ "query": "revenue" }));
    let _ = ready(&mut p, "hits");
    side_tab(&mut p, 2);
    action(&mut p, "side.do", json!({ "id": "hits", "op": "set", "value": 2 }));
    assert_eq!(layout(&mut p), "narrow", "back to the page");
    assert_eq!(port(&mut p, json!({ "get": "hit_index" }))["value"], json!(2));
    // Esc goes back from the panel too
    action(&mut p, "set_layout", json!({ "layout": "panel" }));
    assert!(p.key("escape"));
    assert_eq!(layout(&mut p), "narrow");

    // a dialog brings the panel into view, and closing it goes back
    action(&mut p, "highlight", json!({ "query": "revenue" }));
    let marks = annotation_count(&mut p, 1);
    action(&mut p, "nbar.do", json!({ "id": "sample", "op": "click" }));
    assert_eq!(layout(&mut p), "panel", "the replace dialog is in the side panel");
    assert_eq!(side_state(&mut p).get("confirm_replace").get("name").as_str(), Some("sample.pdf"));
    action(&mut p, "side.do", json!({ "id": "replace-cancel", "op": "click" }));
    assert_eq!(layout(&mut p), "narrow");
    assert_eq!(annotation_count(&mut p, 1), marks, "nothing was replaced");
    // the note tool: the dialog in the panel, then back to the page
    action(&mut p, "goto_page", json!({ "page": 1 }));
    action(&mut p, "nbar.do", json!({ "id": "tool-note", "op": "click" }));
    let (nx, ny) = narrow_world_of(&mut p, 300.0, 700.0);
    assert_eq!(p.pointer_down(nx, ny), PointerResponse::Handled);
    p.pointer_up();
    assert_eq!(layout(&mut p), "panel");
    action(&mut p, "side.do", json!({ "id": "note-text", "op": "set", "value": "From a phone" }));
    action(&mut p, "side.do", json!({ "id": "note-add", "op": "click" }));
    assert_eq!(layout(&mut p), "narrow");
    let r = action(&mut p, "list_annotations", json!({ "page": 1 }));
    assert!(r["annotations"].as_array().expect("list").iter().any(|a| a["contents"] == json!("From a phone")), "{r}");

    // a wide window again: the workspace, fitted, takes its presses again
    fits(&mut p);
    p.viewport(1280.0, 800.0, 1.0);
    assert_eq!(layout(&mut p), "wide");
    assert_eq!(fits(&mut p).last().copied(), Some((0.0, 0.0, 1600.0, 1000.0)));
    assert_ne!(p.pointer_down(1700.0, 500.0), PointerResponse::Ignored);
    p.pointer_up();
    assert!(p.wheel(1300.0, 20.0, 0.0, 1.0));
    let shown = texts(&display_at(&mut p, 1000.0, 56.0));
    assert!(!shown.iter().any(|t| t == "‹ 頁面"), "{shown:?}");
    let shown = texts(&display_at(&mut p, 0.0, 0.0));
    assert!(shown.iter().any(|t| t == "範例"), "the wide toolbar is back: {shown:?}");
    action(&mut p, "toolbar.do", json!({ "id": "next", "op": "click" }));
    assert_eq!(p.handler_errors, 0, "no handler failed");
    for (x, y) in [(0.0, 0.0), (1000.0, 56.0), (176.0, 56.0), NBAR, NVIEW] {
        no_error_card(&display_at(&mut p, x, y), "after the layouts");
    }
}

const LONG_MAIN: &str = r#"
page.action({ name = "build", run = function()
    local d = pdf.new({ pages = 40 })
    for i = 1, 40 do
        d = pdf.add_text(d, i, { x = 56, y = 60, size = 14, text = "Chapter " .. i .. ": revenue and costs" })
    end
    return d:save()
end })
page.panel({ rect = { 0, 0, 10, 10 }, draw = function(d) end })
"#;

#[test]
fn long_documents_and_hosts_without_optional_modules() {
    // a host with only the pdf module (and the ui library): no sheet, no cloud
    let mut n = anypage_runtime::Natives::new();
    anypage_ui::register(&mut n);
    anypage_pdf::register(&mut n);
    let pkg = Package::from_dir(&package_dir()).expect("package");
    let mut p = ScriptPage::load(&pkg, &n, text(), wake()).expect("loads without sheet and cloud");
    no_error_card(&display_at(&mut p, 0.0, 0.0), "toolbar without optional modules");
    let xlsx = action(&mut p, "toolbar.describe", json!({ "types": "button", "limit": 40 }));
    let node = xlsx["nodes"].as_array().expect("nodes").iter().find(|n| n["id"] == json!("xlsx")).cloned().unwrap_or_else(|| panic!("no xlsx button: {xlsx}"));
    assert_eq!(node["disabled"], json!(true), "{node}");
    let r = action(&mut p, "assistant.ask", json!({ "text": "hi" }));
    assert_eq!(r["ok"], json!(false));
    let bad = port(&mut p, json!({ "action": "export_xlsx", "params": {} }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("sheet module")), "{bad}");
    let shown = texts(&side_tab(&mut p, 1));
    assert!(shown.iter().any(|t| t.contains("沒有 cloud 模組")), "{shown:?}");
    let r = action(&mut p, "extract_tables", json!({ "page": 1 }));
    assert_eq!(r["count"], json!(1), "tables still work: {r}");

    // a 40-page document: whole-document searches go to the background job
    let gen = Package::from_files("test:long", [("page.json", GEN_JSON), ("main.luau", LONG_MAIN)]).expect("generator");
    let mut g = ScriptPage::load(&gen, &anypage_std::natives(), text(), wake()).expect("generator loads");
    let bytes = act_data(&mut g, "build", Data::Nil).as_bytes().expect("bytes").to_vec();
    p.file_drop("long.pdf", Arc::new(bytes));
    pump_until(&mut p, "long.pdf to open", 20, |p| p.cell_value("doc_name").is_some_and(|d| d.as_str() == Some("long.pdf")));
    let r = action(&mut p, "search", json!({ "query": "revenue" }));
    assert!(r["note"].as_str().is_some_and(|s| s.contains("background")), "{r}");
    let bad = port(&mut p, json!({ "action": "highlight", "params": { "query": "costs" } }));
    assert!(bad["error"].as_str().is_some_and(|e| e.contains("background")), "{bad}");
    let hits = ready(&mut p, "hits");
    assert_eq!(hits.items().len(), 40);
    let r = action(&mut p, "highlight", json!({ "query": "costs" }));
    assert_eq!(r["highlighted"], json!(40), "{r}");
    // one page at a time stays inline
    let r = action(&mut p, "search", json!({ "query": "Chapter 33", "page": 33 }));
    assert_eq!(r["count"], json!(1), "{r}");
    assert_eq!(port(&mut p, json!({ "get": "page" }))["value"], json!(33));
    let shown = texts(&display_at(&mut p, 0.0, 56.0));
    assert!(shown.iter().any(|t| t == "33"), "the thumbnails follow: {shown:?}");
}

#[test]
fn view_and_layout_timings() {
    let mut p = load_page();
    // every tab, measured as the engine would draw it: the side app's view
    for tab in 1..=5 {
        let t0 = Instant::now();
        side_tab(&mut p, tab);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        eprintln!("side tab {tab}: {ms:.1} ms (first draw)");
        assert!(ms < p.manifest.limits.call_ms, "tab {tab} took {ms:.1} ms");
    }
    // a drag selection step: one handler
    let t0 = Instant::now();
    let (x, y) = world_of(&mut p, 60.0, 130.0);
    p.pointer_down(x, y);
    for k in 0..20 {
        p.pointer_drag(x + k as f32 * 20.0, y + k as f32 * 2.0);
    }
    p.pointer_up();
    let per = t0.elapsed().as_secs_f64() * 1000.0 / 22.0;
    eprintln!("selection: {per:.2} ms per pointer event");
    assert!(per < 20.0);
    let t0 = Instant::now();
    display_at(&mut p, 176.0, 56.0);
    eprintln!("viewer overlay: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);
}
