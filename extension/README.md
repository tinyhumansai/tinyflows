# TinyFlows Browser Companion

This Manifest V3 extension executes explicit TinyFlows browser actions in Chrome tabs that the user shares. The workflow runtime, credentials, approvals, retries, and non-browser integrations remain in the native TinyFlows companion.

## Development install

```bash
npm ci
npm run verify
npm run test:e2e
npm run package
```

Open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select `extension/dist`. The package command also creates a reproducible store-ready ZIP and SHA-256 file under `extension/artifacts/`.

Use the toolbar popup to enter the pairing string printed by the native companion. Pairing credentials are stored in `chrome.storage.local` and sent only through the WebSocket subprotocol to the loopback relay; they are never placed in a URL. The relay URL must resolve to `localhost`, `127.0.0.1`, or `::1` over `ws://`.

## Sharing and running

The popup shares only the active ordinary HTTP(S) tab. Shared tabs are placed in the clearly named **TinyFlows shared tabs** group and attached through `chrome.debugger`. Removing a tab from that group, closing it, renaming/removing the group, or detaching the debugger revokes automation. Restricted browser pages are refused.

The side panel lists workflows exposed by the paired companion, starts a workflow bound to its active tab, shows native run events, and can cancel the current run. It never runs workflow graphs itself.

Browser tool nodes use the explicit `browser` slug and a typed action object. Supported actions are `open`, `snapshot`, `click`, `fill`, `type`, `get_text`, `get_title`, `get_url`, `screenshot`, `wait`, `press`, `hover`, `scroll`, `is_visible`, `close`, and `find`. Other tool slugs stay with the host-provided integration invoker.

## Permissions

- `debugger`: send Chrome DevTools Protocol commands after explicit sharing.
- `tabs` and `tabGroups`: bind and visibly group only shared tabs.
- `storage`: persist pairing configuration and attachment metadata across service-worker restarts.
- `sidePanel`: display workflows and run progress.
- `<all_urls>`: allow ordinary sites to be automated after explicit sharing; it does not expose unshared tabs.

All extension JavaScript is bundled locally. There are no remotely hosted scripts or permanent content-script bridges.
