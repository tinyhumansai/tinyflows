# Chrome workflow companion

TinyFlows can drive ordinary signed-in Chrome tabs through an unpacked Manifest V3 extension. The native companion remains the workflow runtime: it validates and executes `WorkflowGraph`s, owns retries and credentials, and dispatches non-browser integrations. The extension receives only a correlated browser action and the tab already bound to that run.

## Build, install, and pair

```bash
cd extension
npm ci
npm run verify
npm run build
cd ..
cargo run -- extension path
```

Open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the printed `extension/dist` directory. Copy the extension id shown by Chrome, then start the companion:

```bash
cargo run -- pair
cargo run -- companion start \
  --extension-id <32-character-extension-id> \
  --workflows-dir "$PWD/workflows"
```

`pair` prints a loopback relay URL and a pairing token. Paste both into the toolbar popup. The token is stored in `~/.tinyflows/credentials/chrome-extension-relay.secret` with mode `0600` on Unix. It is carried in `Sec-WebSocket-Protocol`, never in the URL. Rotate it with `tinyflows pair --rotate`, restart the companion, and pair the extension again.

Click **Share with TinyFlows** on an ordinary HTTP(S) tab. Chrome places it in the clearly named **TinyFlows shared tabs** group and shows its debugger banner. Moving the tab out of that group, closing it, or detaching the debugger revokes access immediately. The side panel starts a workflow in its own shared tab; the CLI always requires an explicit tab:

```bash
tinyflows tabs
tinyflows workflows
tinyflows run checkout --tab 42 --input '{"query":"boots"}'
```

Workflow files are `<workflow-id>.json` in the configured directory. The standalone CLI intentionally rejects LLM, HTTP, code, and non-browser integration effects because it has no credentials. An embedding host constructs `CompanionServer` with its own `Capabilities`; `RoutingToolInvoker` then sends only `slug: "browser"` to Chrome and delegates every other slug, arguments object, and `connection_ref` unchanged.

## Browser node contract

Browser work remains an ordinary `tool_call`; `WorkflowGraph`, `NodeKind`, and `Capabilities` are unchanged:

```json
{
  "kind": "tool_call",
  "config": {
    "slug": "browser",
    "args": {
      "action": "click",
      "selector": "button[type=submit]"
    },
    "retry": { "max_attempts": 2, "backoff": "fixed", "backoff_ms": 500 }
  }
}
```

`args.action` is mandatory and deterministic. Supported actions are `open`, `snapshot`, `click`, `fill`, `type`, `get_text`, `get_title`, `get_url`, `screenshot`, `wait`, `press`, `hover`, `scroll`, `is_visible`, `close`, and semantic `find`. The canonical v1 schema and cross-language fixtures live in [`protocol/`](../protocol/).

Failures reach normal TinyFlows retry and error-port handling as capability errors prefixed with a stable code, including `tab_not_shared`, `tab_revoked`, `relay_disconnected`, `unsupported_page`, `action_timeout`, and `element_not_found`. A relay disconnect or user revocation always fails the in-flight action; it is retried only if the node declares a retry policy.

## Security boundary

- The HTTP/WebSocket listener is fixed to `127.0.0.1`; non-loopback policies are rejected.
- The WebSocket upgrade requires the exact paired `chrome-extension://<id>` origin, protocol v1, and host-local token.
- Native CLI endpoints require the same token in an `Authorization` header.
- The native registry contains only tabs announced after group and debugger validation. Every run is immutably bound to one tab generation.
- Restricted Chrome pages and non-HTTP(S) navigation are refused. Unshared tabs are never enumerated to a run.
- The MV3 worker uses `chrome.debugger` directly and bundles all executable JavaScript locally; there is no page bridge or remotely hosted code.
- Workflow state, credentials, approvals, integration dispatch, and retry policy never cross into the extension.

## Upgrade and troubleshooting

Protocol messages declare `protocol_version: 1` and reject unknown fields. Upgrade the companion and extension together when the major protocol version changes. A version mismatch fails closed with `protocol_mismatch`.

- **Popup stays reconnecting:** confirm the companion is listening on the same port and rerun `tinyflows pair`; tokens must contain at least 32 ASCII alphanumeric characters.
- **Unauthorized relay:** pass the extension id displayed by the currently loaded unpacked extension. Reloading from a path can change the id if Chrome does not preserve it.
- **Tab missing from `tinyflows tabs`:** share it after the relay connects. Remove and re-add it if the debugger or group was changed while disconnected.
- **`unsupported_page`:** navigate to a regular `http://` or `https://` page; Chrome internal, extension, DevTools, and view-source pages are unavailable.
- **`relay_disconnected` or `tab_revoked`:** the action stopped. Reconnect/re-share, then rerun or rely on an explicitly declared workflow retry.

Build a deterministic store-ready archive with `npm run package`; it writes the ZIP and SHA-256 checksum under `extension/artifacts/`. Chrome Web Store submission and OpenHuman host wiring are intentionally separate follow-up work.
