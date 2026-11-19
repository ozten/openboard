//! A minimal parser for the server-sent event stream.
//!
//! The board sends one event per frame with the event JSON in `data`, plus
//! comment lines as keep-alives. Only `data` is needed: the event carries its
//! own cursor.

#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds bytes from the stream and returns the `data` of every frame
    /// they complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            }
            // Comments (`:keep-alive`) and other fields are not needed.
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_any_chunking() {
        let stream = ": keep-alive\n\nid: ev_1\ndata: {\"a\":1}\n\nid: ev_2\r\ndata: {\"a\":\r\ndata: 2}\r\n\r\ndata: tail";
        let whole = SseParser::default().push(stream.as_bytes());
        assert_eq!(
            whole,
            vec!["{\"a\":1}".to_owned(), "{\"a\":\n2}".to_owned()]
        );

        let mut parser = SseParser::default();
        let mut pieces = Vec::new();
        for byte in stream.as_bytes() {
            pieces.extend(parser.push(&[*byte]));
        }
        assert_eq!(pieces, whole);
    }
}
