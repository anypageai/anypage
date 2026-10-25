//! Server-sent events (the WHATWG `text/event-stream` format), parsed
//! incrementally from arbitrary byte chunks.
//!
//! Handles LF, CRLF and lone CR line endings (also when a CRLF pair is split
//! across chunks), multi-line `data:` fields, comments (`: keep-alive`),
//! `event:`, `id:` and `retry:` fields, a leading byte-order mark, and UTF-8
//! sequences split across chunks. Lines and events longer than the
//! configured limits are an error instead of unbounded buffering.

/// One dispatched event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// `event:` field, `"message"` when absent.
    pub event: String,
    /// `data:` lines joined with `\n`.
    pub data: String,
    /// Last event id seen so far (persists across events, as in browsers).
    pub id: Option<String>,
}

pub struct Parser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    event: String,
    last_id: Option<String>,
    retry: Option<u64>,
    /// The previous chunk ended in CR: a leading LF in the next one belongs to it.
    after_cr: bool,
    started: bool,
    max_line: usize,
    max_event: usize,
    /// Comment lines seen (keep-alives), for diagnostics.
    pub comments: u64,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new(8 << 20)
    }
}

impl Parser {
    /// `max_line`: the longest line accepted, in bytes (also the largest
    /// event).
    pub fn new(max_line: usize) -> Self {
        Self::with_limits(max_line, max_line)
    }

    /// `max_line`: the longest line; `max_event`: the most `data` one event
    /// may collect over its lines. Both in bytes; going over is an error.
    pub fn with_limits(max_line: usize, max_event: usize) -> Self {
        Self { line: Vec::new(), data: String::new(), has_data: false, event: String::new(), last_id: None, retry: None, after_cr: false, started: false, max_line, max_event, comments: 0 }
    }

    /// The reconnection delay the server asked for (`retry:`), in ms.
    pub fn retry_ms(&self) -> Option<u64> {
        self.retry
    }

    /// Feed bytes; complete events are appended to `out`.
    pub fn push(&mut self, mut chunk: &[u8], out: &mut Vec<Event>) -> Result<(), String> {
        if !self.started {
            // a UTF-8 BOM may only appear at the very start of the stream
            let bom = [0xEF, 0xBB, 0xBF];
            let have = self.line.len() + chunk.len();
            if have < 3 && bom.starts_with(&[self.line.as_slice(), chunk].concat()) {
                self.line.extend_from_slice(chunk);
                return Ok(());
            }
            let mut head = std::mem::take(&mut self.line);
            head.extend_from_slice(chunk);
            self.started = true;
            let rest = head.strip_prefix(&bom[..]).unwrap_or(&head).to_vec();
            return self.push(&rest, out);
        }
        if self.after_cr {
            self.after_cr = false;
            if chunk.first() == Some(&b'\n') {
                chunk = &chunk[1..];
            }
        }
        while !chunk.is_empty() {
            match chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(i) => {
                    self.extend_line(&chunk[..i])?;
                    let cr = chunk[i] == b'\r';
                    chunk = &chunk[i + 1..];
                    if cr {
                        match chunk.first() {
                            Some(b'\n') => chunk = &chunk[1..],
                            Some(_) => {}
                            None => self.after_cr = true,
                        }
                    }
                    let line = std::mem::take(&mut self.line);
                    self.process_line(&line, out)?;
                }
                None => {
                    self.extend_line(chunk)?;
                    break;
                }
            }
        }
        Ok(())
    }

    /// End of stream. A final line without a terminator is processed, and a
    /// pending event is dispatched even without the blank line the format
    /// asks for (lenient: some servers close right after the last `data:`).
    pub fn finish(&mut self, out: &mut Vec<Event>) -> Result<(), String> {
        if !self.started {
            self.started = true;
        }
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, out)?;
        }
        self.dispatch(out);
        Ok(())
    }

    fn extend_line(&mut self, part: &[u8]) -> Result<(), String> {
        if self.line.len() + part.len() > self.max_line {
            return Err(format!("event-stream line longer than {} bytes", self.max_line));
        }
        self.line.extend_from_slice(part);
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<Event>) {
        if !self.has_data {
            self.event.clear();
            return;
        }
        if self.data.ends_with('\n') {
            self.data.pop();
        }
        let event = if self.event.is_empty() { "message".to_string() } else { std::mem::take(&mut self.event) };
        out.push(Event { event, data: std::mem::take(&mut self.data), id: self.last_id.clone() });
        self.has_data = false;
        self.event.clear();
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<Event>) -> Result<(), String> {
        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if line[0] == b':' {
            self.comments += 1;
            return Ok(());
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.find(':') {
            Some(i) => {
                let v = &text[i + 1..];
                (&text[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (&text[..], ""),
        };
        match field {
            "data" => {
                if self.data.len() + value.len() + 1 > self.max_event {
                    return Err(format!("event-stream event larger than {} bytes", self.max_event));
                }
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "event" => self.event = value.to_string(),
            "id" => {
                if !value.contains('\0') {
                    self.last_id = Some(value.to_string());
                }
            }
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => self.retry = value.parse().ok(),
            _ => {}
        }
        Ok(())
    }
}

/// Parse a complete event-stream body at once.
pub fn parse_all(body: &[u8]) -> Result<Vec<Event>, String> {
    let mut p = Parser::default();
    let mut out = Vec::new();
    p.push(body, &mut out)?;
    p.finish(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(data: &str) -> Event {
        Event { event: "message".into(), data: data.into(), id: None }
    }

    /// Feeding the same stream in every possible two-way split, and byte by
    /// byte, must give the same events as feeding it whole.
    fn all_splits(stream: &[u8]) -> Vec<Event> {
        let whole = parse_all(stream).unwrap();
        for cut in 0..=stream.len() {
            let mut p = Parser::default();
            let mut out = Vec::new();
            p.push(&stream[..cut], &mut out).unwrap();
            p.push(&stream[cut..], &mut out).unwrap();
            p.finish(&mut out).unwrap();
            assert_eq!(out, whole, "split at {cut}");
        }
        let mut p = Parser::default();
        let mut out = Vec::new();
        for b in stream {
            p.push(std::slice::from_ref(b), &mut out).unwrap();
        }
        p.finish(&mut out).unwrap();
        assert_eq!(out, whole, "byte by byte");
        whole
    }

    #[test]
    fn line_endings_and_multiline_data() {
        let lf = all_splits(b"data: a\ndata: b\n\ndata:c\n\n");
        let crlf = all_splits(b"data: a\r\ndata: b\r\n\r\ndata:c\r\n\r\n");
        let cr = all_splits(b"data: a\rdata: b\r\rdata:c\r\r");
        assert_eq!(lf, vec![ev("a\nb"), ev("c")]);
        assert_eq!(crlf, lf);
        assert_eq!(cr, lf);
    }

    #[test]
    fn comments_fields_and_done() {
        let s = b": keep-alive\n\n:another\nevent: delta\nid: 7\nretry: 1500\ndata: {\"x\":1}\n\ndata: [DONE]\n\n";
        let out = all_splits(s);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], Event { event: "delta".into(), data: "{\"x\":1}".into(), id: Some("7".into()) });
        assert_eq!(out[1], Event { event: "message".into(), data: "[DONE]".into(), id: Some("7".into()) });
        let mut p = Parser::default();
        let mut v = Vec::new();
        p.push(s, &mut v).unwrap();
        assert_eq!(p.retry_ms(), Some(1500));
        assert_eq!(p.comments, 2);
    }

    #[test]
    fn field_edge_cases() {
        // no space after the colon, two spaces (one kept), bare field name,
        // empty data line, unknown fields ignored, event without data dropped
        let out = all_splits(b"data:x\ndata:  y\ndata\nfoo: bar\n\nevent: lonely\n\ndata: z\n\n");
        assert_eq!(out, vec![ev("x\n y\n"), ev("z")]);
    }

    #[test]
    fn bom_utf8_and_unterminated_tail() {
        let s = "\u{FEFF}data: 你好\n\ndata: 世界".as_bytes();
        let out = all_splits(s);
        assert_eq!(out, vec![ev("你好"), ev("世界")]);
    }

    #[test]
    fn throughput() {
        // ~4 MB of OpenAI-style chunks fed in 16 KB reads
        let one = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello world \"}}]}\r\n\r\n: keep-alive\r\n\r\n";
        let stream = one.repeat(4 << 20 >> 6);
        let t0 = std::time::Instant::now();
        let mut p = Parser::default();
        let mut out = Vec::new();
        let mut n = 0;
        for c in stream.as_bytes().chunks(16 * 1024) {
            p.push(c, &mut out).unwrap();
            n += out.len();
            out.clear();
        }
        let secs = t0.elapsed().as_secs_f64();
        let mb = stream.len() as f64 / 1048576.0;
        eprintln!("sse: {mb:.1} MB, {n} events in {:.1} ms = {:.0} MB/s", secs * 1000.0, mb / secs);
        assert_eq!(n, 4 << 20 >> 6);
        assert!(mb / secs > 20.0, "parser slower than 20 MB/s");
    }

    #[test]
    fn line_limit() {
        let mut p = Parser::new(16);
        let mut out = Vec::new();
        assert!(p.push(b"data: 0123456789abcdef", &mut out).is_err());
    }

    #[test]
    fn event_limit() {
        // short lines, but one event collecting more than the limit
        let mut p = Parser::with_limits(64, 20);
        let mut out = Vec::new();
        p.push(b"data: 0123456789\ndata: 0123\n\n", &mut out).unwrap();
        assert_eq!(out, vec![ev("0123456789\n0123")]);
        let e = p.push(b"data: 0123456789\ndata: 0123456789\n", &mut out).unwrap_err();
        assert!(e.contains("event-stream event larger than 20 bytes"), "{e}");
        // the limit is per event: many small events go through
        let mut p = Parser::with_limits(64, 20);
        let mut out = Vec::new();
        for _ in 0..10_000 {
            p.push(b"data: 0123456789\n\n", &mut out).unwrap();
        }
        assert_eq!(out.len(), 10_000);
    }
}
