//! Shared harness: load pages headlessly and wait for cell values.

#![allow(dead_code)]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anypage_core::Status;
use anypage_engine::{Page, TextRaster, Wake};
use anypage_runtime::{Data, Natives, Package, ScriptPage};

pub fn text() -> Arc<TextRaster> {
    static T: OnceLock<Arc<TextRaster>> = OnceLock::new();
    T.get_or_init(|| Arc::new(TextRaster::load().expect("a system font"))).clone()
}

pub fn wake() -> Wake {
    Arc::new(|| {})
}

pub fn load(caps: &str, src: &str, natives: &Natives) -> Result<ScriptPage, String> {
    let m = format!(r#"{{ "format": 1, "id": "devices-test", "bounds": [0, 0, 400, 300], "capabilities": {caps}, "limits": {{ "job_workers": 4 }} }}"#);
    let src = format!("page.panel({{ rect = {{ 0, 0, 10, 10 }}, draw = function(d) end }})\n{src}");
    let pkg = Package::from_files("test", [("page.json", m.as_str()), ("main.luau", src.as_str())])?;
    ScriptPage::load(&pkg, natives, text(), wake())
}

pub const ALL: &str = r#"{ "modules": ["ble", "usb", "serial"] }"#;

/// Evaluate a named cell until `ok` holds (5 s limit).
pub fn wait_for(p: &mut ScriptPage, name: &str, mut ok: impl FnMut(&Data) -> bool) -> Data {
    wait_for_in(p, name, Duration::from_secs(5), &mut ok)
}

pub fn wait_for_in(p: &mut ScriptPage, name: &str, limit: Duration, ok: &mut dyn FnMut(&Data) -> bool) -> Data {
    let cell = p.cell_id(name).unwrap_or_else(|| panic!("no cell {name}"));
    let t0 = Instant::now();
    loop {
        let g = p.parts().0;
        g.commit();
        let i = g.demand(cell);
        let d = anypage_runtime::data::input_to_data(&i);
        if matches!(i.status, Status::Ready | Status::Error(_)) && ok(&d) {
            return d;
        }
        assert!(t0.elapsed() < limit, "timed out waiting for {name}; last {d:?} {:?}", i.status);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The latest item of a stream cell (`{ items, total, last }`).
pub fn last(d: &Data) -> &Data {
    d.get("last")
}

/// The error message of a job cell that failed.
pub fn job_error(d: &Data) -> Option<&str> {
    (d.get("state").as_str() == Some("error")).then(|| d.get("message").as_str()).flatten()
}

pub fn set(p: &mut ScriptPage, name: &str, json: &str) {
    let r = p.act(&format!(r#"{{"set": "{name}", "value": {json}}}"#));
    assert!(r.contains("\"ok\":true"), "set {name}: {r}");
}

pub fn strs(d: &Data) -> Vec<String> {
    d.items().iter().map(|x| x.as_str().unwrap_or("?").to_string()).collect()
}

/// A minimal PNG writer (stored deflate blocks): no image crate needed.
/// `rgba` is premultiplied, as panels produce it; the file is straight alpha.
pub fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks_exact(w as usize * 4) {
        raw.push(0);
        for px in row.as_chunks::<4>().0 {
            let a = px[3] as u32;
            let un = |c: u8| (c as u32 * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
            raw.extend_from_slice(&[un(px[0]), un(px[1]), un(px[2]), px[3]]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    let (mut s1, mut s2) = (1u32, 0u32);
    for &b in &raw {
        s1 = (s1 + b as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    z.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, png)
}
