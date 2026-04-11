//! Server-Sent Events (SSE) parser shared by OpenAI and Anthropic providers.

use std::io;

use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::Stream;
use tokio_util::io::StreamReader;

/// A single parsed SSE event.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// Optional event type from `event:` field.
    pub event: Option<String>,
    /// Accumulated data from one or more `data:` fields.
    pub data: String,
}

/// Wrapper that maps `reqwest::Error` → `io::Error` so `StreamReader` accepts it.
struct MapStream<S>(S);

impl<S> Stream for MapStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = io::Result<Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<io::Result<Bytes>>> {
        use std::pin::Pin;
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|opt| opt.map(|res| res.map_err(io::Error::other)))
    }
}

/// Async SSE stream built on top of a reqwest byte stream.
pub struct SseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    reader: BufReader<StreamReader<MapStream<S>, Bytes>>,
    current_event: Option<String>,
    data_buf: Vec<String>,
}

impl<S> SseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    /// Construct a new `SseStream` from a reqwest response byte stream.
    pub fn new(stream: S) -> Self {
        let map = MapStream(stream);
        let reader = BufReader::new(StreamReader::new(map));
        Self {
            reader,
            current_event: None,
            data_buf: Vec::new(),
        }
    }

    /// Read the next complete SSE event.
    ///
    /// Returns `Ok(None)` when the stream ends or `data: [DONE]` is received.
    pub async fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line).await?;

            // End of stream
            if n == 0 {
                return Ok(self.flush_event());
            }

            let line = line.trim_end_matches(['\n', '\r']);

            if line.is_empty() {
                // Blank line — event boundary
                if let Some(event) = self.flush_event() {
                    return Ok(Some(event));
                }
                // Nothing accumulated yet, keep going
            } else if let Some(value) = line.strip_prefix("event:") {
                self.current_event = Some(value.trim_start().to_owned());
            } else if let Some(value) = line.strip_prefix("data:") {
                let value = value.trim_start();
                if value == "[DONE]" {
                    return Ok(None);
                }
                self.data_buf.push(value.to_owned());
            }
            // Lines starting with ':' are comments — skip them
        }
    }

    fn flush_event(&mut self) -> Option<SseEvent> {
        if self.data_buf.is_empty() {
            self.current_event = None;
            return None;
        }
        let data = self.data_buf.join("\n");
        self.data_buf.clear();
        let event = self.current_event.take();
        Some(SseEvent { event, data })
    }
}

// ─── Synchronous parser used in tests ────────────────────────────────────────

/// Parse raw SSE bytes synchronously. Useful for unit tests.
pub fn parse_sse_events(raw: &[u8]) -> Vec<SseEvent> {
    let text = std::str::from_utf8(raw).unwrap_or_default();
    let mut events = Vec::new();
    let mut current_event: Option<String> = None;
    let mut data_buf: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            // Event boundary
            if !data_buf.is_empty() {
                events.push(SseEvent {
                    event: current_event.take(),
                    data: data_buf.join("\n"),
                });
                data_buf.clear();
            } else {
                current_event = None;
            }
        } else if let Some(value) = line.strip_prefix("event:") {
            current_event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.trim_start();
            if value == "[DONE]" {
                return events; // termination signal
            }
            data_buf.push(value.to_owned());
        }
        // Lines starting with ':' are comments — skip
    }

    // Flush any trailing event (no final blank line)
    if !data_buf.is_empty() {
        events.push(SseEvent {
            event: current_event,
            data: data_buf.join("\n"),
        });
    }

    events
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_data_events() {
        let raw = b"data: hello\n\ndata: world\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, None);
        assert_eq!(events[1].data, "world");
        assert_eq!(events[1].event, None);
    }

    #[test]
    fn test_parse_event_with_type() {
        let raw = b"event: message\ndata: {\"text\":\"hi\"}\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("message".to_owned()));
        assert_eq!(events[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn test_parse_multiline_data() {
        let raw = b"data: line1\ndata: line2\ndata: line3\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn test_parse_done_terminates() {
        let raw = b"data: first\n\ndata: [DONE]\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first");
    }

    #[test]
    fn test_parse_skips_comments_and_empty() {
        let raw = b": this is a comment\ndata: real\n\n: another comment\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn test_parse_multiple_events_with_types() {
        let raw =
            b"event: delta\ndata: chunk1\n\nevent: delta\ndata: chunk2\n\nevent: stop\ndata: done\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event, Some("delta".to_owned()));
        assert_eq!(events[0].data, "chunk1");
        assert_eq!(events[1].event, Some("delta".to_owned()));
        assert_eq!(events[1].data, "chunk2");
        assert_eq!(events[2].event, Some("stop".to_owned()));
        assert_eq!(events[2].data, "done");
    }

    #[test]
    fn test_parse_empty_input() {
        let events = parse_sse_events(b"");
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_no_trailing_newline() {
        let raw = b"data: no-trailing-newline";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "no-trailing-newline");
    }
}
