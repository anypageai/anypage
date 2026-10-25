//! The demo package `pages/cloud-demo` loaded headlessly and driven like a
//! user would: click the field, type (with IME composition), Enter; the
//! reply streams from a 127.0.0.1 mock; panels are rendered and checked.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anypage_engine::{Image, Page, PointerResponse};
use anypage_runtime::{Data, LoadOptions, Package, ScriptPage};
use common::*;
use serde_json::json;

fn demo_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pages/cloud-demo")
}

fn load_demo() -> ScriptPage {
    load_demo_with(&[])
}

fn load_demo_with(keys: &[(&str, &str)]) -> ScriptPage {
    let pkg = Package::from_dir(&demo_dir()).expect("demo package");
    ScriptPage::load_with(&pkg, &natives(), text(), wake(), LoadOptions { secrets: Some(secrets(keys)) }).expect("demo loads")
}

/// Render the panel whose rect starts at world `y`.
fn render_panel(p: &mut ScriptPage, y: f32) -> Image {
    let cell = p.spec().panels.iter().find(|pa| pa.rect.y == y).expect("panel").cell;
    let g = p.parts().0;
    g.commit();
    let i = g.demand(cell);
    let img = i.get::<Image>().expect("a rendered panel");
    Image { w: img.w, h: img.h, rgba: img.rgba.clone(), newest: None }
}

fn count(img: &Image, f: impl Fn([u8; 4]) -> bool) -> usize {
    img.rgba.as_chunks::<4>().0.iter().filter(|c| f(**c)).count()
}

/// Bright, near-white pixels: text in TEXT_HI.
fn text_pixels(img: &Image) -> usize {
    count(img, |c| c[0] > 190 && c[1] > 190 && c[2] > 190)
}

fn reply_stream(text: &str, n_messages: usize) -> String {
    let mut s = String::new();
    for piece in text.split_inclusive(' ') {
        s.push_str(&format!("data: {}\n\n", json!({ "choices": [{ "index": 0, "delta": { "content": piece } }] })));
    }
    s.push_str(&format!("data: {}\n\n", json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] })));
    s.push_str(&format!("data: {}\n\n", json!({ "choices": [], "usage": { "prompt_tokens": n_messages, "completion_tokens": 5, "total_tokens": n_messages + 5 } })));
    s.push_str("data: [DONE]\n\n");
    s
}

#[test]
fn demo_page_streams_a_conversation() {
    let mock = Mock::start(|r| {
        let n = r.json()["messages"].as_array().map(|a| a.len()).unwrap_or(0);
        let text = if n <= 2 { "你好！ 我是 模擬的 本機 模型。 This reply streams in pieces." } else { "第二輪 回覆。" };
        Reply::sse(&reply_stream(text, n), 9, Duration::from_millis(2))
    });
    let mut p = load_demo();
    assert!(p.summary().contains("com.anypage.cloud-demo"));
    let empty_chat = render_panel(&mut p, 108.0);
    assert_eq!((empty_chat.w, empty_chat.h), (1200, 560));
    let user_bubble = |img: &Image| count(img, |c| c == [37, 64, 110, 255]);
    assert_eq!(user_bubble(&empty_chat), 0);

    // point the page at the mock through the semantic port
    set(&mut p, "base_url", json!(mock.url("/v1")));
    set(&mut p, "model", json!("mock-model"));
    let header = render_panel(&mut p, 0.0);
    assert!(text_pixels(&header) > 200, "header text missing");

    // click the field: focused, the host is asked to show the keyboard
    assert_eq!(p.pointer_down(100.0, 720.0), PointerResponse::Handled);
    let caret = p.text_focus().expect("text focus while editing");
    assert!(caret.x >= 30.0 && (caret.y - 706.0).abs() < 1.0, "{caret:?}");
    // IME composition shows inline, then commits
    p.ime_preedit("ni");
    assert_eq!(p.cell_value("ime"), Some(Data::str("ni")));
    p.ime_preedit("");
    p.text_input("你好");
    p.text_input("嗎");
    assert!(p.key("backspace"));
    assert_eq!(p.cell_value("draft"), Some(Data::str("你好")));
    let typed = p.text_focus().expect("still focused");
    assert!(typed.x > caret.x, "the caret moved with the text");
    assert!(p.key("enter"));
    assert_eq!(p.cell_value("draft"), Some(Data::str("")));

    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert!(last.get("error").is_nil(), "{last:?}");
    assert_eq!(last.get("text").as_str(), Some("你好！ 我是 模擬的 本機 模型。 This reply streams in pieces."));
    assert_eq!(last.get("usage").get("total_tokens").num(), Some(7.0));
    let first = &mock.requests()[0];
    let body = first.json();
    assert_eq!(body["model"], "mock-model");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1], json!({ "role": "user", "content": "你好" }));
    assert_eq!(first.header("authorization"), None, "no key is sent to a local server");

    // the transcript is a pure function of the stream
    let t = p.cell_value("transcript").unwrap();
    let blocks = t.items();
    assert_eq!(blocks.len(), 2, "{t:?}");
    assert_eq!(blocks[0].get("text").as_str(), Some("你好"));
    assert_eq!(blocks[1].get("who").as_str(), Some("助理"));

    // second turn through the agent action: the module kept the history
    let r = p.act(r#"{"action": "send", "params": {"text": "再一次"}}"#);
    assert!(r.contains("\"sent\":true"), "{r}");
    let last = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(2.0) && l.get("done").truthy());
    assert_eq!(last.get("text").as_str(), Some("第二輪 回覆。"));
    let second = mock.requests()[1].json();
    let roles: Vec<&str> = second["messages"].as_array().unwrap().iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    assert_eq!(second["messages"][2]["content"], "你好！ 我是 模擬的 本機 模型。 This reply streams in pieces.");

    // what is on screen
    let chat = render_panel(&mut p, 108.0);
    let input = render_panel(&mut p, 680.0);
    let header = render_panel(&mut p, 0.0);
    // the status badge turned green ("完成 · stop · N tokens")
    let green = count(&header, |c| c[1] > 180 && c[0] < 140 && c[2] < 170);
    assert!(green > 50, "no done status in the header ({green})");
    assert!(user_bubble(&chat) > 5_000, "user bubbles: {}", user_bubble(&chat));
    assert!(text_pixels(&chat) > text_pixels(&empty_chat) + 500, "reply text not drawn ({} vs {})", text_pixels(&chat), text_pixels(&empty_chat));
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-output");
    std::fs::create_dir_all(&out).unwrap();
    write_png(&out.join("cloud-demo-chat.png"), &chat);
    write_png(&out.join("cloud-demo-header.png"), &header);
    write_png(&out.join("cloud-demo-input.png"), &input);

    // stop and clear through the port
    let r = p.act(r#"{"action": "clear"}"#);
    assert!(r.contains("cleared"), "{r}");
    let cleared = last_item(&mut p, "chat", 10, |l| l.get("seq").num() == Some(3.0));
    assert_eq!(cleared.get("messages").items().len(), 0);
    assert_eq!(p.cell_value("transcript").map(|t| t.items().len()), Some(0));
    p.shutdown();
}

#[test]
fn demo_page_reports_undeclared_endpoints() {
    let mut p = load_demo();
    let r = p.act(r#"{"action": "use_endpoint", "params": {"base_url": "https://example.com/v1", "model": "m"}}"#);
    assert!(r.contains("\"ok\":false"), "{r}");
    let r = p.act(r#"{"action": "send", "params": {"text": "hi"}}"#);
    assert!(r.contains("\"sent\":true"), "{r}");
    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert!(last.get("error").as_str().unwrap().contains("example.com is not declared"), "{last:?}");
    // the header says so too
    let header = render_panel(&mut p, 0.0);
    let red = count(&header, |c| c[0] > 200 && c[1] < 140 && c[2] < 140);
    assert!(red > 50, "no error colour in the header ({red})");
}

/// The OpenAI key is attached by host, not by a substring: an endpoint
/// whose path merely mentions api.openai.com gets no key.
#[test]
fn demo_page_keeps_the_key_from_lookalike_endpoints() {
    let mock = Mock::start(|_| Reply::sse(&reply_stream("ok", 2), 1000, Duration::ZERO));
    let mut p = load_demo_with(&[("openai", "sk-demo-REAL-KEY-777")]);
    let lookalike = mock.url("/api.openai.com/v1");
    let r = p.act(&json!({ "action": "use_endpoint", "params": { "base_url": lookalike, "model": "m" } }).to_string());
    assert!(r.contains("\"ok\":true"), "{r}");
    let r = p.act(r#"{"action": "send", "params": {"text": "hi"}}"#);
    assert!(r.contains("\"sent\":true"), "{r}");
    let last = last_item(&mut p, "chat", 10, |l| l.get("done").truthy());
    assert!(last.get("error").is_nil(), "{last:?}");
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].header("authorization"), None, "the key went to a lookalike endpoint");
    p.shutdown();
}

// ---------------------------------------------------------------- PNG

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, t) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut c = !0u32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// Uncompressed (stored-deflate) RGBA PNG; enough for test output.
fn write_png(path: &Path, img: &Image) {
    let mut raw = Vec::with_capacity((img.w as usize * 4 + 1) * img.h as usize);
    for row in img.rgba.chunks_exact(img.w as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        png.extend_from_slice(&c);
        png.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&img.w.to_be_bytes());
    ihdr.extend_from_slice(&img.h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    std::fs::write(path, png).unwrap();
}
