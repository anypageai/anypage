//! Bytes in and out of files the user picked (Storage Access Framework) or
//! dropped. Kotlin only opens the document and hands over its file
//! descriptor; reading and writing happen here, in native memory, so a large
//! file never passes through the Java heap or a JNI array copy.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anypage_engine::HostResult;

/// Opened and dropped files larger than this are refused (the desktop host
/// allows 1 GB; phones have less memory).
pub const MAX_OPEN: u64 = 512 << 20;
const CHUNK: usize = 1 << 20;

/// Read everything from `r`, at most `limit` bytes, checking `cancel` between
/// chunks. `size_hint` (the file size when known) avoids regrowing the buffer.
pub fn read_limited(mut r: impl Read, name: &str, limit: u64, size_hint: Option<u64>, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    if let Some(n) = size_hint {
        if n > limit {
            return Err(format!("{name}: larger than {} MB", limit >> 20));
        }
    }
    let mut out = Vec::with_capacity(size_hint.unwrap_or(0).min(limit) as usize);
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("{name}: reading was cancelled"));
        }
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("{name}: {e}")),
        };
        if out.len() as u64 + n as u64 > limit {
            return Err(format!("{name}: larger than {} MB", limit >> 20));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Write all of `bytes` to `w`, checking `cancel` between chunks.
pub fn write_all(mut w: impl Write, name: &str, bytes: &[u8], cancel: &AtomicBool) -> Result<(), String> {
    for chunk in bytes.chunks(CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("{name}: writing was cancelled"));
        }
        w.write_all(chunk).map_err(|e| format!("{name}: {e}"))?;
    }
    w.flush().map_err(|e| format!("{name}: {e}"))
}

/// The answer to an open request from a reader.
pub fn opened(r: impl Read, name: &str, size_hint: Option<u64>, cancel: &AtomicBool) -> HostResult {
    match read_limited(r, name, MAX_OPEN, size_hint, cancel) {
        Ok(bytes) => HostResult::File { name: name.to_string(), bytes: Arc::new(bytes) },
        Err(e) => HostResult::Failed(e),
    }
}

/// The answer to a save request after writing to `w`.
pub fn saved(w: impl Write, name: &str, bytes: &[u8], cancel: &AtomicBool) -> HostResult {
    match write_all(w, name, bytes, cancel) {
        Ok(()) => HostResult::Saved { name: name.to_string() },
        Err(e) => HostResult::Failed(e),
    }
}

/// File descriptors handed over by Kotlin (`ParcelFileDescriptor.detachFd()`).
#[cfg(unix)]
pub mod fd {
    use std::fs::File;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::atomic::AtomicBool;

    use anypage_engine::HostResult;

    /// Take ownership of a detached descriptor; it is closed when dropped.
    ///
    /// # Safety
    /// `fd` must be an open descriptor that nothing else owns or closes.
    pub unsafe fn take(fd: i32) -> Option<File> {
        (fd >= 0).then(|| File::from(OwnedFd::from_raw_fd(fd)))
    }

    /// Read a picked or dropped document.
    ///
    /// # Safety
    /// As for [`take`].
    pub unsafe fn read(fd: i32, name: &str, cancel: &AtomicBool) -> HostResult {
        let Some(f) = take(fd) else { return HostResult::Failed(format!("{name}: could not be opened")) };
        // pipes from remote providers have no size; regular files do
        let hint = f.metadata().ok().filter(|m| m.is_file()).map(|m| m.len());
        super::opened(f, name, hint, cancel)
    }

    /// Write a document the user chose a place for.
    ///
    /// # Safety
    /// As for [`take`].
    pub unsafe fn write(fd: i32, name: &str, bytes: &[u8], cancel: &AtomicBool) -> HostResult {
        let Some(f) = take(fd) else { return HostResult::Failed(format!("{name}: could not be opened for writing")) };
        // "wt" truncates on most providers; make sure for those that ignore it
        if f.metadata().is_ok_and(|m| m.is_file()) {
            let _ = f.set_len(0);
        }
        super::saved(f, name, bytes, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Hands out at most 3 bytes per read, like a pipe.
    struct Trickle(Cursor<Vec<u8>>);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(3);
            self.0.read(&mut buf[..n])
        }
    }

    #[test]
    fn reads_within_the_limit() {
        let no = AtomicBool::new(false);
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        assert_eq!(read_limited(Cursor::new(data.clone()), "a", 10_000, Some(10_000), &no).unwrap(), data);
        assert_eq!(read_limited(Trickle(Cursor::new(data.clone())), "a", 10_000, None, &no).unwrap(), data);
        let e = read_limited(Cursor::new(data.clone()), "big.bin", 9_999, None, &no).unwrap_err();
        assert!(e.contains("big.bin") && e.contains("larger"), "{e}");
        // a known size over the limit fails before reading anything
        let e = read_limited(Cursor::new(Vec::new()), "huge.bin", 100, Some(101), &no).unwrap_err();
        assert!(e.contains("larger"), "{e}");
    }

    #[test]
    fn cancellation_stops_reading_and_writing() {
        let yes = AtomicBool::new(true);
        assert!(read_limited(Cursor::new(vec![1, 2, 3]), "x", 10, None, &yes).unwrap_err().contains("cancelled"));
        assert!(matches!(saved(Vec::new(), "x", &[1], &yes), HostResult::Failed(_)));
    }

    #[test]
    fn host_results() {
        let no = AtomicBool::new(false);
        match opened(Cursor::new(b"hello".to_vec()), "a.txt", Some(5), &no) {
            HostResult::File { name, bytes } => assert_eq!((name.as_str(), bytes.as_slice()), ("a.txt", &b"hello"[..])),
            other => panic!("{other:?}"),
        }
        let mut out = Vec::new();
        assert_eq!(saved(&mut out, "b.txt", b"bytes", &no), HostResult::Saved { name: "b.txt".into() });
        assert_eq!(out, b"bytes");
    }
}
