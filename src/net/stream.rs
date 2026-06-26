// Incremental SSE stream driver. Reads a `reqwest::Response` body chunk by
// chunk, splits events on blank lines, and hands each event's name and
// concatenated `data:` payload to a caller-supplied closure. Returns an error
// when the server closes the stream without a terminal marker or when no
// chunk arrives within `idle`, so silent disconnects no longer look like
// successful streams.

use crate::net::sse::find_boundary;
use crate::providers::ProviderError;
use anyhow::Result;
use futures_util::StreamExt;
use std::time::Duration;

pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Drive `resp` as an SSE byte stream.
///
/// `on_event` is invoked once per complete event with the event name (the
/// SSE `event:` field, or `""` if absent) and the concatenated `data:`
/// payload (joined with `\n` if an event carried multiple `data:` lines). If
/// the closure returns an error the stream is aborted and that error is
/// returned. The closure may also return `Ok(SseControl::Stop)` to indicate
/// it observed a terminal marker (e.g. `[DONE]` or `message_stop`) and the
/// stream should be treated as successfully completed.
///
/// Returns `Err` if the stream ended without a terminal marker, if a chunk
/// failed to decode, or if the connection went idle past `idle`.
pub async fn drive_sse_stream<F>(
    resp: reqwest::Response,
    idle: Duration,
    mut on_event: F,
) -> Result<()>
where
    F: FnMut(SseEvent<'_>) -> Result<SseControl>,
{
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut total_bytes: usize = 0;

    loop {
        let chunk = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(c))) => c,
            Ok(Some(Err(e))) => {
                return Err(ProviderError::Http(e).into());
            }
            Ok(None) => break,
            Err(_) => {
                return Err(ProviderError::Other(format!(
                    "stream idle for {}s with {} bytes buffered - server stopped sending",
                    idle.as_secs(),
                    buf.len()
                ))
                .into());
            }
        };

        total_bytes += chunk.len();
        buf.extend_from_slice(&chunk);

        while let Some(pos) = find_boundary(&buf) {
            let end = boundary_end(&buf, pos);
            let raw: Vec<u8> = buf.drain(..end).collect();
            if let Ok(text) = std::str::from_utf8(&raw) {
                let mut data_lines: Vec<&str> = Vec::new();
                let mut event_name: Option<&str> = None;
                for line in text.lines() {
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if let Some(name) = line.strip_prefix("event:") {
                        event_name = Some(name.trim());
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data_lines.push(d.trim_start());
                    }
                }
                if data_lines.is_empty() && event_name.is_none() {
                    continue;
                }
                let data = data_lines.join("\n");
                let ev = SseEvent {
                    event: event_name.unwrap_or(""),
                    data: &data,
                };
                if matches!(on_event(ev)?, SseControl::Stop) {
                    return Ok(());
                }
            }
        }
    }

    Err(ProviderError::Other(format!(
        "stream closed by server after {total_bytes} bytes without terminal marker"
    ))
    .into())
}

fn boundary_end(buf: &[u8], pos: usize) -> usize {
    if pos + 1 < buf.len() && buf[pos] == b'\r' && buf[pos + 1] == b'\n' {
        pos + 4
    } else {
        pos + 2
    }
}

pub struct SseEvent<'a> {
    pub event: &'a str,
    pub data: &'a str,
}

pub enum SseControl {
    Continue,
    Stop,
}