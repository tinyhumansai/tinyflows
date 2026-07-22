import { PROTOCOL_VERSION, isBrowserCancel, isBrowserRequest, isControlResponse, isRunEvent } from './protocol';
import type { BrowserRequest, BrowserResponse, ControlRequest, ControlResponse, RunEvent } from './protocol';

const CONFIG_KEY = 'tinyflows.relayConfig.v1';
const DEFAULT_URL = 'ws://127.0.0.1:32189/v1/extension';
export interface RelayConfig { url: string; pairingToken: string }
export type RelayState = 'unpaired' | 'connecting' | 'connected' | 'reconnecting' | 'failed';

type StorageApi = Pick<typeof chrome.storage, 'local'>;
type WebSocketFactory = (url: string, protocols: string[]) => WebSocket;

export class RelayClient {
  private socket?: WebSocket;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private heartbeatTimer?: ReturnType<typeof setInterval>;
  private retries = 0;
  private stopped = false;
  private pending = new Map<string, { resolve: (value: unknown) => void; reject: (reason: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  private onBrowserCancel: (requestId: string) => void = () => undefined;

  constructor(
    private readonly onBrowserRequest: (request: BrowserRequest) => Promise<BrowserResponse>,
    private readonly onState: (state: RelayState) => void,
    private readonly onRunEvent: (event: RunEvent) => void,
    private readonly storage: StorageApi = chrome.storage,
    private readonly makeWebSocket: WebSocketFactory = (url, protocols) => new WebSocket(url, protocols)
  ) {}

  async start(): Promise<void> {
    this.stopped = false;
    const config = await this.getConfig();
    if (!config) { this.onState('unpaired'); return; }
    this.connect(config);
  }

  setBrowserCancelHandler(handler: (requestId: string) => void): void {
    this.onBrowserCancel = handler;
  }

  stop(): void {
    this.stopped = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
    this.socket?.close(1000, 'extension stopped');
    this.rejectPending('Relay disconnected');
  }

  async configure(config: RelayConfig): Promise<void> {
    assertConfig(config);
    await this.storage.local.set({ [CONFIG_KEY]: config });
    this.stop();
    this.retries = 0;
    await this.start();
  }

  async getConfig(): Promise<RelayConfig | undefined> {
    const value = (await this.storage.local.get(CONFIG_KEY))[CONFIG_KEY];
    if (!isRelayConfig(value)) return undefined;
    return value;
  }

  async request(method: ControlRequest['method'], params: Record<string, unknown>, timeoutMs = 15_000): Promise<unknown> {
    if (!this.socket || this.socket.readyState !== 1) throw new Error('Relay is not connected');
    const request_id = crypto.randomUUID();
    const request = controlRequest(method, request_id, params);
    const result = new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(request_id); reject(new Error(`${method} timed out`)); }, timeoutMs);
      this.pending.set(request_id, { resolve, reject, timer });
    });
    this.socket.send(JSON.stringify(request));
    return result;
  }

  send(message: unknown): void {
    if (this.socket?.readyState === 1) this.socket.send(JSON.stringify(message));
  }

  private connect(config: RelayConfig): void {
    if (this.stopped) return;
    this.onState(this.retries ? 'reconnecting' : 'connecting');
    const protocols = ['tinyflows.v1', `tinyflows.auth.${config.pairingToken}`];
    const socket = this.makeWebSocket(config.url, protocols);
    this.socket = socket;
    socket.onopen = () => {
      if (socket !== this.socket) return socket.close();
      this.retries = 0;
      this.onState('connected');
      this.heartbeatTimer = setInterval(() => this.send({ protocol_version: PROTOCOL_VERSION, type: 'heartbeat' }), 15_000);
    };
    socket.onmessage = (event) => { void this.handleMessage(String(event.data)); };
    socket.onerror = () => this.onState('failed');
    socket.onclose = () => {
      if (this.heartbeatTimer) clearInterval(this.heartbeatTimer);
      this.rejectPending('Relay disconnected');
      if (!this.stopped && socket === this.socket) this.scheduleReconnect(config);
    };
  }

  private async handleMessage(raw: string): Promise<void> {
    let message: unknown;
    try { message = JSON.parse(raw); } catch { return; }
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

  private settle(response: ControlResponse): void {
    const pending = this.pending.get(response.request_id);
    if (!pending) return;
    clearTimeout(pending.timer); this.pending.delete(response.request_id);
    switch (response.status) {
      case 'ok': pending.resolve(response.result); break;
      case 'workflows': pending.resolve(response.workflows); break;
      case 'tabs': pending.resolve(response.tabs); break;
      case 'connection': pending.resolve({ connected: response.connected }); break;
      case 'error': pending.reject(new Error(response.message)); break;
    }
  }

  private scheduleReconnect(config: RelayConfig): void {
    this.retries += 1;
    this.onState(this.retries >= 8 ? 'failed' : 'reconnecting');
    const delay = Math.min(30_000, 500 * 2 ** Math.min(this.retries - 1, 6));
    this.retryTimer = setTimeout(() => this.connect(config), delay);
  }

  private rejectPending(message: string): void {
    for (const item of this.pending.values()) { clearTimeout(item.timer); item.reject(new Error(message)); }
    this.pending.clear();
  }
}

function assertConfig(value: RelayConfig): void {
  if (!isRelayConfig(value)) throw new Error('Use a loopback ws:// URL and a base64url pairing token');
}
function isRelayConfig(value: unknown): value is RelayConfig {
  if (typeof value !== 'object' || value === null) return false;
  const item = value as Record<string, unknown>;
  if (typeof item.url !== 'string' || typeof item.pairingToken !== 'string' || !/^[A-Za-z0-9]{32,512}$/.test(item.pairingToken)) return false;
  try {
    const url = new URL(item.url);
    return url.protocol === 'ws:' && (url.hostname === '127.0.0.1' || url.hostname === 'localhost' || url.hostname === '[::1]');
  } catch { return false; }
}
export { DEFAULT_URL };

function controlRequest(method: ControlRequest['method'], request_id: string, params: Record<string, unknown>): ControlRequest {
  const base = { protocol_version: PROTOCOL_VERSION, request_id };
  switch (method) {
    case 'workflow.list': return { ...base, method };
    case 'tab.list': return { ...base, method };
    case 'connection.status': return { ...base, method };
    case 'workflow.start': return { ...base, method, workflow_id: String(params.workflow_id ?? ''), tab_id: Number(params.tab_id), input: params.input ?? {} };
    case 'workflow.cancel': return { ...base, method, run_id: String(params.run_id ?? '') };
    case 'run.subscribe': return { ...base, method, run_id: String(params.run_id ?? '') };
  }
}
