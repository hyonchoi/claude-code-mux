# Incoming Auth Validation Spec

## Accepted Auth Schemes

The relay accepts the following authentication schemes for incoming requests:

| Scheme | Header | Example |
|--------|--------|---------|
| Bearer token | `Authorization: Bearer <token>` | `Authorization: Bearer sk-ant-...` |
| API key header | `X-Api-Key: <key>` | `X-Api-Key: my-server-key` |

Both schemes are read and validated. Only one needs to be present.

## Precedence When Both Headers Are Present

When a request includes both `Authorization: Bearer <token>` AND `X-Api-Key: <key>`:
- **`Authorization: Bearer`** takes precedence for passthrough auth (relayed to upstream).
- **`X-Api-Key`** is used for relay gate validation (server.api_key check).
- The two headers serve different purposes and are not in conflict.

## Relay Gate Order

The `server.api_key` relay gate is checked **before** the passthrough auth header is
read. If the relay gate rejects the request (wrong or missing API key), the request
never reaches the provider routing or passthrough logic.

Order of operations for an incoming request:
1. Auth middleware checks `X-Api-Key` against `server.api_key` (if configured).
2. If gate passes (or no `server.api_key` configured), handler runs.
3. Handler reads `Authorization: Bearer` for passthrough auth (if applicable).

## Error Response Format

All auth validation failures return:

```
HTTP 401 Unauthorized
Content-Type: text/plain
Body: "Unauthorized"
```

No error detail is included in the response body to avoid leaking configuration state.

## Missing or Malformed Auth

- Missing `X-Api-Key` when `server.api_key` is configured → 401
- Wrong `X-Api-Key` value → 401
- `Authorization` header present but not `Bearer <token>` format → passthrough auth
  is treated as absent (not an error at the relay level; upstream provider will reject)
