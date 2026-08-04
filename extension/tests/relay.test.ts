import { describe, expect, it, vi } from 'vitest';
import { RelayClient } from '../src/relay';
import type { BrowserRequest, BrowserResponse } from '../src/protocol';

class FakeSocket {
  readyState = 0; sent: string[] = []; protocols: string[];
  onopen: ((event?: unknown) => void) | null = null; onclose: (() => void) | null = null;
  onerror: (() => void) | null = null; onmessage: ((event: {data:string}) => void) | null = null;
  constructor(public url: string, protocols: string[]) { this.protocols = protocols; }
  send(value: string) { this.sent.push(value); }
  close() { this.readyState = 3; }
  open() { this.readyState = 1; this.onopen?.(); }
  message(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
}
const TOKEN = '0123456789abcdef0123456789abcdef';
function setup(config: unknown = { url: 'ws://127.0.0.1:32189/v1/extension', pairingToken: TOKEN }) {
  const sockets: FakeSocket[] = []; const states: string[] = []; const events: unknown[] = [];
  let stored: Record<string, unknown> = { 'tinyflows.relayConfig.v1': config };
  const storage = { local: { get: vi.fn(async () => stored), set: vi.fn(async (value) => { stored = value; }) } };
  const browser = vi.fn(async (request: BrowserRequest): Promise<BrowserResponse> =>
    ({ status: 'ok', protocol_version: 1, request_id: request.request_id, result: { data: 'ok' } }));
  const relay = new RelayClient(browser, (state) => states.push(state), (event) => events.push(event), storage as any,
    (url, protocols) => { const socket = new FakeSocket(url, protocols); sockets.push(socket); return socket as any; });
  return { relay, sockets, states, events };
}

describe('authenticated relay', () => {
  it('puts the secret in a websocket subprotocol and handles browser requests', async () => {
    const { relay, sockets, states } = setup(); await relay.start();
    expect(sockets[0]?.url).not.toContain(TOKEN);
    expect(sockets[0]?.protocols).toContain(`tinyflows.auth.${TOKEN}`);
    sockets[0]?.open(); expect(states).toContain('connected');
    sockets[0]?.message({ protocol_version: 1, request_id: 'r', run_id: 'x', tab_id: 1, timeout_ms: 1000, action: { action: 'get_title' } });
    await vi.waitFor(() => expect(sockets[0]?.sent.some((item) => JSON.parse(item).status === 'ok')).toBe(true));
    relay.send({ event: 'tab_shared', protocol_version: 1, tab: { id: 1, window_id: 2, url: 'https://example.com', title: 'Example' } });
    expect(JSON.parse(sockets[0]!.sent.at(-1)!)).toMatchObject({ event: 'tab_shared', tab: { id: 1 } });
    relay.stop();
  });

  it('correlates control replies and forwards run events', async () => {
    const { relay, sockets, events } = setup(); await relay.start(); sockets[0]?.open();
    const pending = relay.request('workflow.list', {});
    const sent = JSON.parse(sockets[0]!.sent.at(-1)!);
    expect(sent).toEqual({ protocol_version: 1, request_id: sent.request_id, method: 'workflow.list' });
    sockets[0]?.message({ protocol_version: 1, status: 'workflows', request_id: sent.request_id, workflows: [{ id: 'one', name: 'One' }] });
    await expect(pending).resolves.toEqual([{ id: 'one', name: 'One' }]);
    sockets[0]?.message({ event: 'step_started', protocol_version: 1, run_id: 'run', node_id: 'one', node_kind: 'browser' });
    expect(events).toHaveLength(1); relay.stop();
  });

  it('forwards correlated browser cancellation', async () => {
    const { relay, sockets } = setup();
    const cancelled: string[] = [];
    relay.setBrowserCancelHandler((requestId) => cancelled.push(requestId));
    await relay.start(); sockets[0]?.open();
    sockets[0]?.message({ protocol_version: 1, type: 'browser.cancel', request_id: 'run:7' });
    expect(cancelled).toEqual(['run:7']); relay.stop();
  });

  it('rejects non-loopback config and stays unpaired without config', async () => {
    const empty = setup(null); await empty.relay.start(); expect(empty.states).toContain('unpaired');
    const configured = setup();
    await expect(configured.relay.configure({ url: 'wss://evil.example/ws', pairingToken: TOKEN })).rejects.toThrow(/loopback/);
    await expect(configured.relay.configure({ url: 'ws://127.0.0.1:32189/ws', pairingToken: `${TOKEN.slice(0, 30)}-_` })).resolves.toBeUndefined();
    configured.relay.stop();
  });

  it('ignores close and error events from a replaced socket', async () => {
    const { relay, sockets, states } = setup();
    await relay.start(); sockets[0]?.open();
    await relay.configure({ url: 'ws://127.0.0.1:32190/v1/extension', pairingToken: TOKEN });
    sockets[1]?.open();
    sockets[0]?.onerror?.(); sockets[0]?.onclose?.();
    expect(states.at(-1)).toBe('connected');
    expect(sockets).toHaveLength(2);
    relay.stop();
  });
});
