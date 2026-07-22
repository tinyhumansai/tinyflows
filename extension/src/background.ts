import { CdpExecutor } from './cdp';
import { BrowserError, toBrowserError } from './errors';
import { PROTOCOL_VERSION, tabSharedEvent } from './protocol';
import type { BrowserRequest, BrowserResponse } from './protocol';
import { RelayClient } from './relay';
import type { RelayState } from './relay';
import { TabManager } from './tab-manager';

const tabs = new TabManager();
const executor = new CdpExecutor();
let relayState: RelayState = 'unpaired';

const relay = new RelayClient(handleBrowserRequest, handleRelayState, (event) => {
  void chrome.runtime.sendMessage({ type: 'run.event', event }).catch(() => undefined);
});

async function handleBrowserRequest(request: BrowserRequest): Promise<BrowserResponse> {
  relay.send({ event: 'action_started', protocol_version: PROTOCOL_VERSION, request_id: request.request_id, run_id: request.run_id, tab_id: request.tab_id });
  try {
    await tabs.assertShared(request.tab_id);
    const data = await executor.execute(request);
    relay.send({ event: 'action_completed', protocol_version: PROTOCOL_VERSION, request_id: request.request_id, result: { data } });
    return { status: 'ok', protocol_version: PROTOCOL_VERSION, request_id: request.request_id, result: { data } };
  } catch (cause) {
    const error = toBrowserError(cause);
    const data = { code: error.code, message: error.message };
    relay.send({ event: 'action_failed', protocol_version: PROTOCOL_VERSION, request_id: request.request_id, error: data });
    return { status: 'error', protocol_version: PROTOCOL_VERSION, request_id: request.request_id, error: data };
  }
}

function handleRelayState(state: RelayState): void {
  relayState = state;
  const badge = state === 'connected' ? 'connected' : state === 'failed' ? 'failed' : 'reconnecting';
  void tabs.markAll(badge);
  if (state === 'connected') void announceAllSharedTabs();
  void chrome.runtime.sendMessage({ type: 'relay.state', state }).catch(() => undefined);
}

chrome.runtime.onInstalled.addListener(() => {
  void chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: false });
});
chrome.runtime.onStartup.addListener(() => { void boot(); });
chrome.tabs.onRemoved.addListener((tabId) => {
  if (tabs.has(tabId)) { void tabs.revoke(tabId, false); relay.send({ event: 'tab_revoked', protocol_version: PROTOCOL_VERSION, tab_id: tabId }); }
});
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (tabs.has(tabId) && changeInfo.groupId !== undefined) {
    void tabs.assertShared(tabId).catch(() => relay.send({ event: 'tab_revoked', protocol_version: PROTOCOL_VERSION, tab_id: tabId }));
  }
});
chrome.debugger.onDetach.addListener((source) => {
  if (source.tabId !== undefined && tabs.has(source.tabId)) {
    void tabs.revoke(source.tabId, false);
    relay.send({ event: 'tab_revoked', protocol_version: PROTOCOL_VERSION, tab_id: source.tabId });
  }
});

chrome.runtime.onMessage.addListener((message: unknown, _sender, sendResponse) => {
  void handleUiMessage(message).then(sendResponse, (error: unknown) => {
    const result = error instanceof Error ? error.message : String(error);
    sendResponse({ ok: false, error: result });
  });
  return true;
});

async function handleUiMessage(message: unknown): Promise<unknown> {
  if (!message || typeof message !== 'object') throw new BrowserError('invalid_request', 'Invalid UI request');
  const item = message as Record<string, unknown>;
  switch (item.type) {
    case 'state': return { ok: true, relayState, tabs: tabs.list(), config: await relay.getConfig() };
    case 'tab.toggle': {
      if (!Number.isInteger(item.tabId)) throw new Error('Missing tab id');
      const tabId = item.tabId as number;
      const shared = await tabs.toggle(tabId);
      if (shared && relayState === 'connected') relay.send(tabSharedEvent(await tabs.announcement(tabId)));
      else if (!shared) relay.send({ event: 'tab_revoked', protocol_version: PROTOCOL_VERSION, tab_id: tabId });
      return { ok: true, shared };
    }
    case 'relay.configure':
      await relay.configure({ url: String(item.url ?? ''), pairingToken: String(item.pairingToken ?? '') });
      return { ok: true };
    case 'workflow.list': return { ok: true, result: await relay.request('workflow.list', {}) };
    case 'workflow.start': return { ok: true, result: await relay.request('workflow.start', { workflow_id: item.workflowId, tab_id: item.tabId }) };
    case 'workflow.cancel': return { ok: true, result: await relay.request('workflow.cancel', { run_id: item.runId }) };
    default: throw new Error('Unknown UI request');
  }
}

async function announceAllSharedTabs(): Promise<void> {
  for (const { tabId } of tabs.list()) {
    try { relay.send(tabSharedEvent(await tabs.announcement(tabId))); }
    catch { /* assertShared revokes stale attachment metadata */ }
  }
}

async function boot(): Promise<void> {
  await tabs.rehydrate();
  await relay.start();
}
void boot();
