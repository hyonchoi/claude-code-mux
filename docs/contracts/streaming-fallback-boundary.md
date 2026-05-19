# Streaming Fallback Boundary

## The Invariant

**Once the first SSE byte is emitted to the client, mid-stream fallback is
architecturally impossible.**

The relay commits to a provider when it begins streaming the response. The client
HTTP connection is already open and bytes have been sent. There is no mechanism to
switch providers partway through a stream without terminating and re-opening the
client connection.

## Before First Byte: Fallback Is Possible

If a streaming provider call fails **before any SSE byte has been sent to the client**,
the relay falls back to the next provider in priority order — identical to non-streaming
fallback behavior.

Implementation: `src/server/mod.rs` around line 1270. The `send_message_stream()` call
returns a `Result` before any bytes flow. If it returns `Err`, the fallback loop continues.

## After First Byte: No Recovery

If a provider fails **mid-stream** (after at least one SSE byte has been sent),
the relay has no recovery path. The stream terminates and the client receives a
truncated or malformed response. The client must retry the full request.

This is intentional and not a bug. Adding mid-stream recovery would require buffering
the entire stream (defeating the purpose of streaming) or proxy-level connection
re-establishment (complex, error-prone, not in scope).

## Error Codes That Trigger Fallback vs Passthrough

| Scenario | Behavior |
|----------|----------|
| `send_message_stream()` returns `Err` before first byte | Fallback to next provider |
| Stream starts then provider drops connection | Stream terminates; client retries |
| Provider returns non-2xx before first SSE byte | Fallback if `Err`, passthrough if `Ok` with error body |

## Reference

See `src/server/mod.rs` ~line 1303 (`send_message_stream` call) for the exact
boundary in code.
