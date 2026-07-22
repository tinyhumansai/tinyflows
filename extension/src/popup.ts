import { DEFAULT_URL } from './relay';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = $('relay-status');
const detail = $('tab-detail');
const toggle = $<HTMLButtonElement>('toggle-tab');
const url = $<HTMLInputElement>('relay-url');
const token = $<HTMLInputElement>('pairing-token');
const result = $('pair-result');
let activeTabId: number | undefined;

async function load(): Promise<void> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  activeTabId = tab?.id;
  const state = await chrome.runtime.sendMessage({ type: 'state' });
  status.textContent = `Companion: ${state.relayState}`;
  url.value = state.config?.url ?? DEFAULT_URL;
  const shared = activeTabId !== undefined && state.tabs.some((item: {tabId:number}) => item.tabId === activeTabId);
  detail.textContent = shared ? 'This tab is explicitly shared.' : 'This tab is private.';
  toggle.textContent = shared ? 'Stop sharing this tab' : 'Share with TinyFlows';
  toggle.disabled = activeTabId === undefined || !tab?.url?.startsWith('http');
}

toggle.addEventListener('click', async () => {
  if (activeTabId === undefined) return;
  toggle.disabled = true;
  try { await chrome.runtime.sendMessage({ type: 'tab.toggle', tabId: activeTabId }); await load(); }
  catch (error) { detail.textContent = error instanceof Error ? error.message : String(error); }
});
$('pair').addEventListener('click', async () => {
  result.textContent = 'Pairing…';
  const response = await chrome.runtime.sendMessage({ type: 'relay.configure', url: url.value.trim(), pairingToken: token.value.trim() });
  result.textContent = response.ok ? 'Pairing saved. Connecting…' : response.error;
  result.className = response.ok ? 'success' : 'error';
  token.value = '';
});
$('open-panel').addEventListener('click', async () => {
  if (activeTabId !== undefined) await chrome.sidePanel.open({ tabId: activeTabId });
  window.close();
});
void load();
