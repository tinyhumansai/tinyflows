// src/errors.ts
var BrowserError = class extends Error {
  constructor(code, message, retryable = false) {
    super(message);
    this.code = code;
    this.retryable = retryable;
    this.name = "BrowserError";
  }
};
function toBrowserError(error) {
  if (error instanceof BrowserError) return error;
  const message = error instanceof Error ? error.message : String(error);
  if (message.toLowerCase().includes("no tab with id")) {
    return new BrowserError("tab_revoked", "The shared tab no longer exists");
  }
  return new BrowserError("browser_failure", message || "Unknown browser error");
}

// src/cdp.ts
var CdpExecutor = class {
  constructor(debuggerApi = chrome.debugger, tabsApi = chrome.tabs) {
    this.debuggerApi = debuggerApi;
    this.tabsApi = tabsApi;
  }
  async execute(request, controller = new AbortController()) {
    const operation = this.run(request.tab_id, request.action, controller.signal);
    return withTimeout(operation, request.timeout_ms, controller);
  }
  async run(tabId, action, signal) {
    ensureActive(signal);
    const target = { tabId };
    switch (action.action) {
      case "open": {
        const url = action.url;
        if (!/^https?:\/\//i.test(url)) throw new BrowserError("unsupported_page", "Only HTTP(S) URLs can be opened");
        const marker = `__tinyflows_navigation_${Date.now()}_${Math.random().toString(36).slice(2)}`;
        await evaluate(this.debuggerApi, target, `globalThis[${JSON.stringify(marker)}]=true`);
        const navigation = await this.debuggerApi.sendCommand(target, "Page.navigate", { url });
        if (navigation.errorText) throw new BrowserError("browser_failure", navigation.errorText);
        await waitForReady(this.debuggerApi, target, signal, navigation.loaderId ? marker : void 0);
        return { url };
      }
      case "snapshot":
        return this.debuggerApi.sendCommand(target, "Accessibility.getFullAXTree", {});
      case "get_title":
        return evaluate(this.debuggerApi, target, "document.title");
      case "get_url":
        return evaluate(this.debuggerApi, target, "location.href");
      case "get_text":
        return evaluate(this.debuggerApi, target, textExpression(action.selector));
      case "is_visible":
        return evaluate(this.debuggerApi, target, visibleExpression(action.selector));
      case "find":
        return evaluate(this.debuggerApi, target, findExpression(action.query));
      case "screenshot": {
        const options = { format: "png", fromSurface: true, captureBeyondViewport: action.full_page ?? false };
        if (action.selector) options.clip = await elementRect(this.debuggerApi, target, action.selector);
        else if (action.full_page) {
          const metrics = await this.debuggerApi.sendCommand(target, "Page.getLayoutMetrics", {});
          if (metrics.cssContentSize) options.clip = { ...metrics.cssContentSize, scale: 1 };
        }
        const result = await this.debuggerApi.sendCommand(target, "Page.captureScreenshot", options);
        return result;
      }
      case "click": {
        const point = await elementPoint(this.debuggerApi, target, action.selector);
        ensureActive(signal);
        await mouse(this.debuggerApi, target, "mousePressed", point, 1);
        await mouse(this.debuggerApi, target, "mouseReleased", point, 1);
        return { clicked: true };
      }
      case "hover": {
        const point = await elementPoint(this.debuggerApi, target, action.selector);
        ensureActive(signal);
        await mouse(this.debuggerApi, target, "mouseMoved", point, 0);
        return { hovered: true };
      }
      case "fill": {
        const { selector, value } = action;
        const ok = await evaluate(this.debuggerApi, target, fillExpression(selector, value));
        if (!ok) throw new BrowserError("element_not_found", `No element matches ${selector}`);
        return { filled: true };
      }
      case "type": {
        if (action.selector) {
          const ok = await evaluate(this.debuggerApi, target, `(() => { const e=document.querySelector(${JSON.stringify(action.selector)}); if(!e)return null; e.focus(); return true; })()`);
          if (ok !== true) throw new BrowserError("element_not_found", `No element matches ${action.selector}`);
        }
        await this.debuggerApi.sendCommand(target, "Input.insertText", { text: action.text });
        return { typed: true };
      }
      case "press": {
        const key = action.key;
        await this.debuggerApi.sendCommand(target, "Input.dispatchKeyEvent", { type: "keyDown", key });
        await this.debuggerApi.sendCommand(target, "Input.dispatchKeyEvent", { type: "keyUp", key });
        return { pressed: key };
      }
      case "scroll": {
        const x = action.x ?? 0;
        const y = action.y ?? 0;
        await this.debuggerApi.sendCommand(target, "Runtime.evaluate", {
          expression: `window.scrollBy(${JSON.stringify(x)},${JSON.stringify(y)})`,
          returnByValue: true
        });
        return { x, y };
      }
      case "wait": {
        const selector = action.selector;
        const ms = action.duration_ms ?? (selector ? 15e3 : 1e3);
        if (!selector) {
          await delay(ms);
          return { waitedMs: ms };
        }
        const deadline = Date.now() + ms;
        while (Date.now() < deadline) {
          ensureActive(signal);
          if (await evaluate(this.debuggerApi, target, visibleExpression(selector))) return { visible: true };
          await delay(Math.min(100, Math.max(1, deadline - Date.now())));
        }
        throw new BrowserError("element_not_found", `Element did not appear: ${selector}`);
      }
      case "close":
        await this.tabsApi.remove(tabId);
        return { closed: true };
    }
  }
};
async function evaluate(api, target, expression) {
  const response = await api.sendCommand(target, "Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  const result = response;
  if (result.exceptionDetails) throw new BrowserError("browser_failure", "Page evaluation failed");
  return result.result?.value;
}
async function elementPoint(api, target, selector) {
  const expression = `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!e)return null; const r=e.getBoundingClientRect(); return {x:r.left+r.width/2,y:r.top+r.height/2}; })()`;
  const point = await evaluate(api, target, expression);
  if (!point || typeof point.x !== "number" || typeof point.y !== "number") {
    throw new BrowserError("element_not_found", `No element matches ${selector}`);
  }
  return { x: point.x, y: point.y };
}
async function elementRect(api, target, selector) {
  const expression = `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!e)return null; const r=e.getBoundingClientRect(); return {x:r.left+scrollX,y:r.top+scrollY,width:r.width,height:r.height,scale:1}; })()`;
  const rect = await evaluate(api, target, expression);
  if (!rect || typeof rect.x !== "number" || typeof rect.y !== "number" || typeof rect.width !== "number" || typeof rect.height !== "number") {
    throw new BrowserError("element_not_found", `No element matches ${selector}`);
  }
  return rect;
}
async function mouse(api, target, type, point, clickCount) {
  await api.sendCommand(target, "Input.dispatchMouseEvent", { type, x: point.x, y: point.y, button: "left", clickCount });
}
function textExpression(selector) {
  return selector ? `document.querySelector(${JSON.stringify(selector)})?.textContent ?? null` : 'document.body?.innerText ?? ""';
}
function visibleExpression(selector) {
  return `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!e)return false; const r=e.getBoundingClientRect(); const s=getComputedStyle(e); return r.width>0&&r.height>0&&s.visibility!=="hidden"&&s.display!=="none"; })()`;
}
function fillExpression(selector, value) {
  return `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!(e instanceof HTMLInputElement||e instanceof HTMLTextAreaElement))return false; e.focus(); const set=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(e),"value")?.set; set?.call(e,${JSON.stringify(value)}); e.dispatchEvent(new Event("input",{bubbles:true})); e.dispatchEvent(new Event("change",{bubbles:true})); return true; })()`;
}
function findExpression(text) {
  return `(() => { const q=${JSON.stringify(text)}.toLowerCase(); return [...document.querySelectorAll("a,button,input,textarea,select,[role],[aria-label]")].filter(e => ((e.textContent||"")+" "+(e.getAttribute("aria-label")||"")).toLowerCase().includes(q)).slice(0,50).map(e => ({tag:e.tagName.toLowerCase(),text:(e.textContent||"").trim().slice(0,500),role:e.getAttribute("role"),ariaLabel:e.getAttribute("aria-label")})); })()`;
}
function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
async function withTimeout(operation, ms, controller) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => {
      controller.abort();
      reject(new BrowserError("action_timeout", `Browser action timed out after ${ms}ms`, true));
    }, ms);
  });
  try {
    return await Promise.race([operation, timeout]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}
function ensureActive(signal) {
  if (signal.aborted) throw new BrowserError("cancelled", "Browser action was cancelled");
}
async function waitForReady(api, target, signal, previousDocumentMarker) {
  while (true) {
    ensureActive(signal);
    const state = await evaluate(api, target, `({
      ready: document.readyState,
      previousDocument: ${previousDocumentMarker ? `globalThis[${JSON.stringify(previousDocumentMarker)}] === true` : "false"}
    })`);
    if (!state.previousDocument && (state.ready === "interactive" || state.ready === "complete")) return;
    await delay(25);
  }
}

// src/protocol.ts
var PROTOCOL_VERSION = 1;
function tabSharedEvent(tab) {
  return { event: "tab_shared", protocol_version: PROTOCOL_VERSION, tab };
}
function isBrowserRequest(value) {
  if (!isRecordWithKeys(value, ["protocol_version", "request_id", "run_id", "tab_id", "timeout_ms", "action"])) return false;
  return value.protocol_version === PROTOCOL_VERSION && isId(value.request_id) && isId(value.run_id) && Number.isSafeInteger(value.tab_id) && value.tab_id >= 0 && Number.isSafeInteger(value.timeout_ms) && value.timeout_ms >= 1 && value.timeout_ms <= 6e4 && isBrowserAction(value.action);
}
function isBrowserCancel(value) {
  return isRecordWithKeys(value, ["protocol_version", "type", "request_id"]) && value.protocol_version === PROTOCOL_VERSION && value.type === "browser.cancel" && isId(value.request_id);
}
function isBrowserAction(value) {
  if (!isRecord(value) || typeof value.action !== "string") return false;
  switch (value.action) {
    case "open":
      return exactStrings(value, ["action", "url"], ["url"]);
    case "snapshot":
    case "get_title":
    case "get_url":
    case "close":
      return hasExactKeys(value, ["action"]);
    case "click":
    case "hover":
    case "is_visible":
      return exactStrings(value, ["action", "selector"], ["selector"]);
    case "fill":
      return hasExactKeys(value, ["action", "selector", "value"]) && typeof value.selector === "string" && value.selector.length > 0 && typeof value.value === "string";
    case "type":
      return exactStrings(value, ["action", "text"], ["text"], ["selector"]);
    case "get_text":
      return exactStrings(value, ["action"], [], ["selector"]);
    case "screenshot":
      return hasOnlyKeys(value, ["action", "selector", "full_page"]) && optionalString(value.selector) && (value.full_page === void 0 || typeof value.full_page === "boolean");
    case "wait":
      return hasOnlyKeys(value, ["action", "duration_ms", "selector"]) && optionalString(value.selector) && (value.duration_ms === void 0 || Number.isSafeInteger(value.duration_ms) && value.duration_ms >= 0);
    case "press":
      return exactStrings(value, ["action", "key"], ["key"]);
    case "scroll":
      return hasOnlyKeys(value, ["action", "x", "y"]) && optionalInteger(value.x) && optionalInteger(value.y);
    case "find":
      return exactStrings(value, ["action", "query"], ["query"]);
    default:
      return false;
  }
}
function isControlResponse(value) {
  if (!isRecord(value) || value.protocol_version !== PROTOCOL_VERSION || !isId(value.request_id)) return false;
  switch (value.status) {
    case "ok":
      return hasExactKeys(value, ["status", "protocol_version", "request_id", "result"]);
    case "error":
      return hasExactKeys(value, ["status", "protocol_version", "request_id", "code", "message"]) && isId(value.code) && typeof value.message === "string";
    case "workflows":
      return hasExactKeys(value, ["status", "protocol_version", "request_id", "workflows"]) && Array.isArray(value.workflows);
    case "tabs":
      return hasExactKeys(value, ["status", "protocol_version", "request_id", "tabs"]) && Array.isArray(value.tabs);
    case "connection":
      return hasExactKeys(value, ["status", "protocol_version", "request_id", "connected"]) && typeof value.connected === "boolean";
    default:
      return false;
  }
}
function isRunEvent(value) {
  if (!isRecord(value) || value.protocol_version !== PROTOCOL_VERSION || !isId(value.run_id)) return false;
  switch (value.event) {
    case "started":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "tab_id"]) && Number.isSafeInteger(value.tab_id);
    case "step_started":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "node_id", "node_kind"]) && isId(value.node_id) && isId(value.node_kind);
    case "step_completed":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "node_id", "node_kind", "output"]) && isId(value.node_id) && isId(value.node_kind);
    case "awaiting_approval":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "pending_approvals"]) && Array.isArray(value.pending_approvals) && value.pending_approvals.every(isId);
    case "browser_action_started":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "request_id", "tab_id", "action"]) && isId(value.request_id) && Number.isSafeInteger(value.tab_id) && isId(value.action);
    case "browser_action_completed":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "request_id", "output"]) && isId(value.request_id);
    case "browser_action_failed":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "request_id", "code", "message"]) && isId(value.request_id) && isId(value.code) && typeof value.message === "string";
    case "completed":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "output"]);
    case "failed":
      return hasExactKeys(value, ["event", "protocol_version", "run_id", "code", "message"]) && isId(value.code) && typeof value.message === "string";
    case "cancelled":
      return hasExactKeys(value, ["event", "protocol_version", "run_id"]);
    default:
      return false;
  }
}
function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function isRecordWithKeys(value, keys) {
  return isRecord(value) && hasExactKeys(value, keys);
}
function hasExactKeys(value, keys) {
  return Object.keys(value).length === keys.length && hasOnlyKeys(value, keys) && keys.every((key) => key in value);
}
function hasOnlyKeys(value, keys) {
  return Object.keys(value).every((key) => keys.includes(key));
}
function exactStrings(value, requiredKeys, requiredStrings, optionalStrings = []) {
  if (!requiredKeys.every((key) => key in value) || !hasOnlyKeys(value, [...requiredKeys, ...optionalStrings])) return false;
  return requiredStrings.every((key) => typeof value[key] === "string" && value[key].length > 0) && optionalStrings.every((key) => optionalString(value[key]));
}
function optionalString(value) {
  return value === void 0 || typeof value === "string";
}
function optionalInteger(value) {
  return value === void 0 || Number.isSafeInteger(value);
}
function isId(value) {
  return typeof value === "string" && value.length > 0 && value.length <= 256;
}

// src/relay.ts
var CONFIG_KEY = "tinyflows.relayConfig.v1";
var RelayClient = class {
  constructor(onBrowserRequest, onState, onRunEvent, storage = chrome.storage, makeWebSocket = (url, protocols) => new WebSocket(url, protocols)) {
    this.onBrowserRequest = onBrowserRequest;
    this.onState = onState;
    this.onRunEvent = onRunEvent;
    this.storage = storage;
    this.makeWebSocket = makeWebSocket;
  }
  socket;
  retryTimer;
  heartbeatTimer;
  retries = 0;
  stopped = false;
  pending = /* @__PURE__ */ new Map();
  onBrowserCancel = () => void 0;
  async start() {
    this.stopped = false;
    const config = await this.getConfig();
    if (!config) {
      this.onState("unpaired");
      return;
    }
    this.connect(config);
  }
  setBrowserCancelHandler(handler) {
    this.onBrowserCancel = handler;
  }
  stop() {
    this.stopped = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
    this.socket?.close(1e3, "extension stopped");
    this.rejectPending("Relay disconnected");
  }
  async configure(config) {
    assertConfig(config);
    await this.storage.local.set({ [CONFIG_KEY]: config });
    this.stop();
    this.retries = 0;
    await this.start();
  }
  async getConfig() {
    const value = (await this.storage.local.get(CONFIG_KEY))[CONFIG_KEY];
    if (!isRelayConfig(value)) return void 0;
    return value;
  }
  async request(method, params, timeoutMs = 15e3) {
    if (!this.socket || this.socket.readyState !== 1) throw new Error("Relay is not connected");
    const request_id = crypto.randomUUID();
    const request = controlRequest(method, request_id, params);
    const result = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(request_id);
        reject(new Error(`${method} timed out`));
      }, timeoutMs);
      this.pending.set(request_id, { resolve, reject, timer });
    });
    this.socket.send(JSON.stringify(request));
    return result;
  }
  send(message) {
    if (this.socket?.readyState === 1) this.socket.send(JSON.stringify(message));
  }
  connect(config) {
    if (this.stopped) return;
    this.onState(this.retries ? "reconnecting" : "connecting");
    const protocols = ["tinyflows.v1", `tinyflows.auth.${config.pairingToken}`];
    const socket = this.makeWebSocket(config.url, protocols);
    this.socket = socket;
    socket.onopen = () => {
      if (socket !== this.socket) return socket.close();
      this.retries = 0;
      this.onState("connected");
      this.heartbeatTimer = setInterval(() => this.send({ protocol_version: PROTOCOL_VERSION, type: "heartbeat" }), 15e3);
    };
    socket.onmessage = (event) => {
      void this.handleMessage(String(event.data));
    };
    socket.onerror = () => this.onState("failed");
    socket.onclose = () => {
      if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
      this.rejectPending("Relay disconnected");
      if (!this.stopped && socket === this.socket) this.scheduleReconnect(config);
    };
  }
  async handleMessage(raw) {
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    if (isBrowserRequest(message)) {
      this.send(await this.onBrowserRequest(message));
    } else if (isBrowserCancel(message)) {
      this.onBrowserCancel(message.request_id);
    } else if (isControlResponse(message)) {
      this.settle(message);
    } else if (isRunEvent(message)) {
      this.onRunEvent(message);
    }
  }
  settle(response) {
    const pending = this.pending.get(response.request_id);
    if (!pending) return;
    clearTimeout(pending.timer);
    this.pending.delete(response.request_id);
    switch (response.status) {
      case "ok":
        pending.resolve(response.result);
        break;
      case "workflows":
        pending.resolve(response.workflows);
        break;
      case "tabs":
        pending.resolve(response.tabs);
        break;
      case "connection":
        pending.resolve({ connected: response.connected });
        break;
      case "error":
        pending.reject(new Error(response.message));
        break;
    }
  }
  scheduleReconnect(config) {
    this.retries += 1;
    this.onState(this.retries >= 8 ? "failed" : "reconnecting");
    const delay2 = Math.min(3e4, 500 * 2 ** Math.min(this.retries - 1, 6));
    this.retryTimer = setTimeout(() => this.connect(config), delay2);
  }
  rejectPending(message) {
    for (const item of this.pending.values()) {
      clearTimeout(item.timer);
      item.reject(new Error(message));
    }
    this.pending.clear();
  }
};
function assertConfig(value) {
  if (!isRelayConfig(value)) throw new Error("Use a loopback ws:// URL and a base64url pairing token");
}
function isRelayConfig(value) {
  if (typeof value !== "object" || value === null) return false;
  const item = value;
  if (typeof item.url !== "string" || typeof item.pairingToken !== "string" || !/^[A-Za-z0-9]{32,512}$/.test(item.pairingToken)) return false;
  try {
    const url = new URL(item.url);
    return url.protocol === "ws:" && (url.hostname === "127.0.0.1" || url.hostname === "localhost" || url.hostname === "[::1]");
  } catch {
    return false;
  }
}
function controlRequest(method, request_id, params) {
  const base = { protocol_version: PROTOCOL_VERSION, request_id };
  switch (method) {
    case "workflow.list":
      return { ...base, method };
    case "tab.list":
      return { ...base, method };
    case "connection.status":
      return { ...base, method };
    case "workflow.start":
      return { ...base, method, workflow_id: String(params.workflow_id ?? ""), tab_id: Number(params.tab_id), input: params.input ?? {} };
    case "workflow.cancel":
      return { ...base, method, run_id: String(params.run_id ?? "") };
    case "run.subscribe":
      return { ...base, method, run_id: String(params.run_id ?? "") };
  }
}

// src/tab-manager.ts
var STORAGE_KEY = "tinyflows.sharedTabs.v1";
var GROUP_TITLE = "TinyFlows shared tabs";
var TabManager = class {
  constructor(api = chrome) {
    this.api = api;
  }
  shared = /* @__PURE__ */ new Map();
  async rehydrate() {
    const saved = (await this.api.storage.local.get(STORAGE_KEY))[STORAGE_KEY];
    if (Array.isArray(saved)) {
      for (const item of saved) {
        if (isSharedTab(item)) this.shared.set(item.tabId, item);
      }
    }
    for (const tabId of [...this.shared.keys()]) {
      try {
        await this.assertShared(tabId);
        const targets = await this.api.debugger.getTargets();
        if (!targets.some((target) => target.tabId === tabId && target.attached)) {
          await this.api.debugger.attach({ tabId }, "1.3");
        }
        await this.setBadge(tabId, "connected");
      } catch {
        await this.revoke(tabId, false);
      }
    }
    return this.list();
  }
  async share(tabId) {
    const tab = await this.api.tabs.get(tabId);
    ensureSupported(tab.url);
    if (tab.windowId === void 0) throw new BrowserError("invalid_request", "Tab has no window");
    let groupId;
    const groups = await this.api.tabGroups.query({ windowId: tab.windowId, title: GROUP_TITLE });
    const existing = groups[0];
    if (existing) {
      groupId = existing.id;
      await this.api.tabs.group({ groupId, tabIds: [tabId] });
    } else {
      groupId = await this.api.tabs.group({ tabIds: [tabId] });
      await this.api.tabGroups.update(groupId, { title: GROUP_TITLE, color: "blue", collapsed: false });
    }
    try {
      await this.api.debugger.attach({ tabId }, "1.3");
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!message.includes("already attached")) throw error;
    }
    const shared = { tabId, groupId, windowId: tab.windowId, attachedAt: Date.now() };
    this.shared.set(tabId, shared);
    await this.persist();
    await this.setBadge(tabId, "connected");
    return shared;
  }
  async revoke(tabId, ungroup = true) {
    const existed = this.shared.delete(tabId);
    if (existed) await this.persist();
    try {
      await this.api.debugger.detach({ tabId });
    } catch {
    }
    if (ungroup) {
      try {
        await this.api.tabs.ungroup([tabId]);
      } catch {
      }
    }
    await this.setBadge(tabId, "idle");
  }
  async toggle(tabId) {
    if (this.shared.has(tabId)) {
      await this.revoke(tabId);
      return false;
    }
    await this.share(tabId);
    return true;
  }
  async assertShared(tabId) {
    const record = this.shared.get(tabId);
    if (!record) throw new BrowserError("tab_not_shared", "The tab was not explicitly shared");
    let tab;
    try {
      tab = await this.api.tabs.get(tabId);
    } catch {
      throw new BrowserError("tab_revoked", "The shared tab no longer exists");
    }
    ensureSupported(tab.url);
    if (tab.groupId !== record.groupId) {
      await this.revoke(tabId, false);
      throw new BrowserError("tab_revoked", "The tab was removed from the TinyFlows group");
    }
    const group = await this.api.tabGroups.get(record.groupId).catch(() => void 0);
    if (!group || group.title !== GROUP_TITLE) {
      await this.revoke(tabId, false);
      throw new BrowserError("tab_revoked", "The TinyFlows group was removed or renamed");
    }
    return record;
  }
  list() {
    return [...this.shared.values()].sort((a, b) => a.tabId - b.tabId);
  }
  has(tabId) {
    return this.shared.has(tabId);
  }
  async announcement(tabId) {
    const shared = await this.assertShared(tabId);
    const tab = await this.api.tabs.get(tabId);
    return {
      id: tabId,
      window_id: shared.windowId,
      url: tab.url,
      title: tab.title ?? ""
    };
  }
  async markAll(state) {
    await Promise.all(this.list().map(({ tabId }) => this.setBadge(tabId, state)));
  }
  async setBadge(tabId, state) {
    const visual = {
      connected: { text: "ON", color: "#16794f" },
      reconnecting: { text: "\u2026", color: "#ad6b00" },
      failed: { text: "!", color: "#b42318" },
      idle: { text: "", color: "#59636e" }
    }[state];
    try {
      await this.api.action.setBadgeBackgroundColor({ tabId, color: visual.color });
      await this.api.action.setBadgeText({ tabId, text: visual.text });
    } catch {
    }
  }
  async persist() {
    await this.api.storage.local.set({ [STORAGE_KEY]: this.list() });
  }
};
function ensureSupported(url) {
  if (!url || !/^https?:\/\//i.test(url)) {
    throw new BrowserError("unsupported_page", "Chrome does not permit automation on this page");
  }
}
function isSharedTab(value) {
  if (typeof value !== "object" || value === null) return false;
  const item = value;
  return Number.isInteger(item.tabId) && Number.isInteger(item.groupId) && Number.isInteger(item.windowId) && typeof item.attachedAt === "number";
}

// src/background.ts
var tabs = new TabManager();
var executor = new CdpExecutor();
var relayState = "unpaired";
var pendingWorkflowTabs = /* @__PURE__ */ new Set();
var activeAutomationTabs = /* @__PURE__ */ new Set();
var closingWorkflowTabs = /* @__PURE__ */ new Set();
var actions = /* @__PURE__ */ new Map();
var bootPromise;
var relay = new RelayClient(handleBrowserRequest, handleRelayState, (event) => {
  void chrome.runtime.sendMessage({ type: "run.event", event }).catch(() => void 0);
});
relay.setBrowserCancelHandler((requestId) => actions.get(requestId)?.abort());
async function handleBrowserRequest(request) {
  const controller = new AbortController();
  actions.set(request.request_id, controller);
  activeAutomationTabs.add(request.tab_id);
  if (request.action.action === "close") closingWorkflowTabs.add(request.tab_id);
  notifyRunEvent({
    event: "browser_action_started",
    protocol_version: PROTOCOL_VERSION,
    run_id: request.run_id,
    request_id: request.request_id,
    tab_id: request.tab_id,
    action: request.action.action
  });
  relay.send({ event: "action_started", protocol_version: PROTOCOL_VERSION, request_id: request.request_id, run_id: request.run_id, tab_id: request.tab_id });
  try {
    await tabs.assertShared(request.tab_id);
    const data = await executor.execute(request, controller);
    relay.send({ event: "action_completed", protocol_version: PROTOCOL_VERSION, request_id: request.request_id, result: { data } });
    notifyRunEvent({
      event: "browser_action_completed",
      protocol_version: PROTOCOL_VERSION,
      run_id: request.run_id,
      request_id: request.request_id,
      output: data
    });
    if (request.action.action === "close") {
      await tabs.revoke(request.tab_id, false);
      setTimeout(() => relay.send({ event: "tab_revoked", protocol_version: PROTOCOL_VERSION, tab_id: request.tab_id }), 0);
    }
    return { status: "ok", protocol_version: PROTOCOL_VERSION, request_id: request.request_id, result: { data } };
  } catch (cause) {
    const error = toBrowserError(cause);
    const data = { code: error.code, message: error.message };
    relay.send({ event: "action_failed", protocol_version: PROTOCOL_VERSION, request_id: request.request_id, error: data });
    notifyRunEvent({
      event: "browser_action_failed",
      protocol_version: PROTOCOL_VERSION,
      run_id: request.run_id,
      request_id: request.request_id,
      code: error.code,
      message: error.message
    });
    return { status: "error", protocol_version: PROTOCOL_VERSION, request_id: request.request_id, error: data };
  } finally {
    actions.delete(request.request_id);
    activeAutomationTabs.delete(request.tab_id);
    closingWorkflowTabs.delete(request.tab_id);
  }
}
function notifyRunEvent(event) {
  void chrome.runtime.sendMessage({ type: "run.event", event }).catch(() => void 0);
}
function handleRelayState(state) {
  relayState = state;
  if (state !== "connected") {
    for (const controller of actions.values()) controller.abort();
  }
  const badge = state === "connected" ? "connected" : state === "failed" ? "failed" : "reconnecting";
  void tabs.markAll(badge);
  if (state === "connected") void announceAllSharedTabs();
  void chrome.runtime.sendMessage({ type: "relay.state", state }).catch(() => void 0);
}
chrome.runtime.onInstalled.addListener(() => {
  void chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: false });
});
chrome.runtime.onStartup.addListener(() => {
  void bootOnce();
});
chrome.tabs.onRemoved.addListener((tabId) => {
  pendingWorkflowTabs.delete(tabId);
  if (tabs.has(tabId)) {
    void tabs.revoke(tabId, false);
    if (!closingWorkflowTabs.has(tabId)) relay.send({ event: "tab_revoked", protocol_version: PROTOCOL_VERSION, tab_id: tabId });
  }
});
chrome.tabs.onCreated.addListener((tab) => {
  if (tab.id !== void 0 && tab.openerTabId !== void 0 && activeAutomationTabs.has(tab.openerTabId)) {
    pendingWorkflowTabs.add(tab.id);
  }
});
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (pendingWorkflowTabs.has(tabId) && changeInfo.url?.startsWith("http")) {
    pendingWorkflowTabs.delete(tabId);
    void tabs.share(tabId).then(async () => {
      if (relayState === "connected") relay.send(tabSharedEvent(await tabs.announcement(tabId)));
    }).catch(() => void 0);
  }
  if (tabs.has(tabId) && changeInfo.groupId !== void 0 && !closingWorkflowTabs.has(tabId)) {
    void tabs.assertShared(tabId).catch(() => relay.send({ event: "tab_revoked", protocol_version: PROTOCOL_VERSION, tab_id: tabId }));
  }
});
chrome.debugger.onDetach.addListener((source) => {
  if (source.tabId !== void 0 && tabs.has(source.tabId)) {
    if (closingWorkflowTabs.has(source.tabId)) return;
    void tabs.revoke(source.tabId, false);
    relay.send({ event: "tab_revoked", protocol_version: PROTOCOL_VERSION, tab_id: source.tabId });
  }
});
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  void handleUiMessage(message).then(sendResponse, (error) => {
    const result = error instanceof Error ? error.message : String(error);
    sendResponse({ ok: false, error: result });
  });
  return true;
});
async function handleUiMessage(message) {
  if (!message || typeof message !== "object") throw new BrowserError("invalid_request", "Invalid UI request");
  const item = message;
  switch (item.type) {
    case "state":
      return { ok: true, relayState, tabs: tabs.list(), config: await relay.getConfig() };
    case "tab.toggle": {
      if (!Number.isInteger(item.tabId)) throw new Error("Missing tab id");
      const tabId = item.tabId;
      const shared = await tabs.toggle(tabId);
      if (shared && relayState === "connected") relay.send(tabSharedEvent(await tabs.announcement(tabId)));
      else if (!shared) relay.send({ event: "tab_revoked", protocol_version: PROTOCOL_VERSION, tab_id: tabId });
      return { ok: true, shared };
    }
    case "relay.configure":
      await relay.configure({ url: String(item.url ?? ""), pairingToken: String(item.pairingToken ?? "") });
      return { ok: true };
    case "workflow.list":
      return { ok: true, result: await relay.request("workflow.list", {}) };
    case "workflow.start": {
      const result = await relay.request("workflow.start", { workflow_id: item.workflowId, tab_id: item.tabId });
      const runId = result && typeof result === "object" ? String(result.run_id ?? "") : "";
      if (runId) await relay.request("run.subscribe", { run_id: runId });
      return { ok: true, result };
    }
    case "workflow.cancel":
      return { ok: true, result: await relay.request("workflow.cancel", { run_id: item.runId }) };
    default:
      throw new Error("Unknown UI request");
  }
}
async function announceAllSharedTabs() {
  for (const { tabId } of tabs.list()) {
    try {
      relay.send(tabSharedEvent(await tabs.announcement(tabId)));
    } catch {
    }
  }
}
async function boot() {
  await tabs.rehydrate();
  await relay.start();
}
function bootOnce() {
  bootPromise ??= boot();
  return bootPromise;
}
void bootOnce();
