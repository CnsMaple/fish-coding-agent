// SSE / streaming helpers. Currently empty: providers parse the byte stream
// directly in their own `chat_stream` implementations, which keeps allocations
// tight and avoids double-buffering.

pub mod sse;
