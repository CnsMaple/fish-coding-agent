// SSE helpers: locate a complete message boundary in a byte buffer.
// A message ends at a blank line (LF LF or CRLF CRLF).

pub fn find_boundary(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 2);
        }
    }
    None
}

// Note: the eventsource-stream crate is not used by the current implementation
// (we parse the byte stream inline to keep memory bounded). The helper is
// retained for future use.
