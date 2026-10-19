# Screen query API v1

Screen region trees and stable v1 selectors are now available through
`cleat inspect ID --screen`, `cleat wait ID --select SEL`, and the corresponding
HTTP screen and wait routes. See [screen queries](../screen-queries.md) and the
[JSON schema](../screen-query-v1.schema.json).

## Error compatibility

Successful `GET /sessions/{id}/screen` responses without parameters retain
`{"text":"…"}`. Unsupported screen requests, including this plain endpoint,
now return HTTP 409 with a structured error object instead of the previous
`{"error":"message"}` string envelope:

```json
{"error":{"code":"unsupported","message":"screen access is unavailable"}}
```

HTTP consumers should read `error.code` and `error.message`. Malformed selectors
use HTTP 400 and `invalid_selector`. A provider failure after selector wait
registration uses HTTP 409 and `screen_query_failed`.

The CLI prints this structured JSON on stderr, followed by a newline. `wait` exits with status 2 for these
errors, including with `--json`; error details are not a success JSON object on
stdout. Scripts should use the exit status to distinguish errors from readiness
(0) and timeout (1), and use HTTP for a standalone structured error envelope.
