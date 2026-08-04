// src/relay.ts
var DEFAULT_URL = "ws://127.0.0.1:32189/v1/extension";

// src/popup.ts
var $ = (id) => document.getElementById(id);
var status = $("relay-status");
var detail = $("tab-detail");
var toggle = $("toggle-tab");
var url = $("relay-url");
var token = $("pairing-token");
var result = $("pair-result");
var activeTabId;
async function load() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  activeTabId = tab?.id;
  const state = await chrome.runtime.sendMessage({ type: "state" });
  status.textContent = `Companion: ${state.relayState}`;
  url.value = state.config?.url ?? DEFAULT_URL;
  const shared = activeTabId !== void 0 && state.tabs.some((item) => item.tabId === activeTabId);
  detail.textContent = shared ? "This tab is explicitly shared." : "This tab is private.";
  toggle.textContent = shared ? "Stop sharing this tab" : "Share with TinyFlows";
  toggle.disabled = activeTabId === void 0 || !tab?.url?.startsWith("http");
}
toggle.addEventListener("click", async () => {
  if (activeTabId === void 0) return;
  toggle.disabled = true;
  try {
    await chrome.runtime.sendMessage({ type: "tab.toggle", tabId: activeTabId });
    await load();
  } catch (error) {
    detail.textContent = error instanceof Error ? error.message : String(error);
  }
});
$("pair").addEventListener("click", async () => {
  result.textContent = "Pairing\u2026";
  const response = await chrome.runtime.sendMessage({ type: "relay.configure", url: url.value.trim(), pairingToken: token.value.trim() });
  result.textContent = response.ok ? "Pairing saved. Connecting\u2026" : response.error;
  result.className = response.ok ? "success" : "error";
  token.value = "";
});
$("open-panel").addEventListener("click", async () => {
  if (activeTabId !== void 0) await chrome.sidePanel.open({ tabId: activeTabId });
  window.close();
});
void load();
