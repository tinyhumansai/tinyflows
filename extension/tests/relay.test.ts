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
function setup(config: unknown = { url: 'ws://127.0.0.1:32189/v1/extension', pairingToken: 'abcdefghijklmnop' }) {
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
    expect(sockets[0]?.url).not.toContain('abcdefghijklmnop');
    expect(sockets[0]?.protocols).toContain('tinyflows.auth.abcdefghijklmnop');
    sockets[0]?.open(); expect(states).toContain('connected');
    sockets[0]?.message({ protocol_version: 1, request_id: 'r', run_id: 'x', tab_id: 1, timeout_ms: 1000, action: { action: 'get_title' } });
    await vi.waitFor(() => expect(sockets[0]?.sent.some((item) => JSON.parse(item).status === 'ok')).toBe(true));
    relay.stop();
  });

  it('correlates control replies and forwards run events', async () => {
    const { relay, sockets, events } = setup(); await relay.start(); sockets[0]?.open();
    const pending = relay.request('workflow.list', {});
    const sent = JSON.parse(sockets[0]!.sent.at(-1)!);
    sockets[0]?.message({ protocol_version: 1, type: 'control.response', request_id: sent.request_id, ok: true, result: ['one'] });
    await expect(pending).resolves.toEqual(['one']);
    sockets[0]?.message({ protocol_version: 1, type: 'run.event', run_id: 'run', event: 'step', data: {} });
    expect(events).toHaveLength(1); relay.stop();
  });

  it('rejects non-loopback config and stays unpaired without config', async () => {
    const empty = setup(null); await empty.relay.start(); expect(empty.states).toContain('unpaired');
    const configured = setup();
    await expect(configured.relay.configure({ url: 'wss://evil.example/ws', pairingToken: 'abcdefghijklmnop' })).rejects.toThrow(/loopback/);
  });
});
