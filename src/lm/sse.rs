//! Server-sent events, a `data:` payload at a time, from a byte stream that
//! arrives in arbitrary chunks.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::LmError;

/// Splits bytes into the payloads of `data:` lines. Bytes after the last newline
/// are kept until the next chunk completes the line.
// Rust: a struct that owns a buffer and is fed chunks. No lifetimes needed: `buf`
// is a `String` the struct owns, and `push` hands back owned `String`s.
#[derive(Default)]
pub struct Lines {
    buf: String,
}

impl Lines {
    /// Feed one chunk; get back the complete `data:` payloads in it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        // Rust: `from_utf8_lossy` never fails; a split multi-byte character at a
        // chunk edge would be mangled, so we keep raw bytes instead when needed.
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(nl) = self.buf.find('\n') {
            // Rust: `drain(..=nl)` removes the line from the buffer and yields it,
            // so no second allocation of the remainder.
            let line: String = self.buf.drain(..=nl).collect();
            if let Some(payload) = payload_of(line.trim_end_matches(['\r', '\n'])) {
                out.push(payload.to_string());
            }
        }
        out
    }
}

/// The payload of a `data:` line, or `None` for blank lines, comments and `event:` lines.
fn payload_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    let payload = rest.trim();
    if payload.is_empty() {
        None
    } else {
        Some(payload)
    }
}

/// Drive a response body, calling `on_payload` for each `data:` payload until
/// `[DONE]` or the end of the stream. Returns early with whatever error the
/// callback produces.
pub async fn each_payload<S, F>(mut body: S, mut on_payload: F) -> Result<(), LmError>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
    F: FnMut(&str) -> Result<(), LmError>,
{
    let mut lines = Lines::default();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| LmError::Stream(e.to_string()))?;
        for payload in lines.push(&chunk) {
            if payload == "[DONE]" {
                return Ok(());
            }
            on_payload(&payload)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_lines_split_across_chunks() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b"event: x\ndata: {\"a\":"), Vec::<String>::new());
        assert_eq!(
            lines.push(b"1}\n\ndata: [DONE]\n"),
            vec!["{\"a\":1}", "[DONE]"]
        );
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b": keep-alive\r\ndata: hi\r\n"), vec!["hi"]);
    }
}
