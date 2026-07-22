import { test as base, chromium, expect } from '@playwright/test';
import type { BrowserContext } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { WebSocketServer, type WebSocket } from 'ws';

const test = base.extend<{ context: BrowserContext; extensionId: string; profile: string }>({
  profile: async ({}, use) => {
    const profile = await mkdtemp(join(tmpdir(), 'tinyflows-extension-'));
    await use(profile); await rm(profile, { recursive: true, force: true });
  },
  context: async ({ profile }, use) => {
    const path = resolve('dist');
    const context = await chromium.launchPersistentContext(profile, {
      channel: 'chromium', headless: true,
      args: [`--disable-extensions-except=${path}`, `--load-extension=${path}`]
    });
    await use(context); await context.close();
  },
  extensionId: async ({ context }, use) => {
    let worker = context.serviceWorkers()[0];
    worker ??= await context.waitForEvent('serviceworker');
    await use(new URL(worker.url()).host);
  }
});

test('loads local MV3 pages and keeps ordinary tabs private by default', async ({ context, extensionId }) => {
  const page = await context.newPage();
  await page.goto(`chrome-extension://${extensionId}/sidepanel.html`);
  await expect(page.locator('h1')).toHaveText('TinyFlows');

  const state = await page.evaluate(async () => chrome.runtime.sendMessage({ type: 'state' }));
  expect(state.tabs).toEqual([]);
});

test('explicitly shares and revokes an ordinary tab through the TinyFlows group', async ({ context, extensionId }) => {
  await context.route('http://tinyflows.test/', (route) => route.fulfill({ contentType: 'text/html', body: '<title>Fixture</title><button>Buy</button>' }));
  const target = await context.newPage();
  await target.goto('http://tinyflows.test/');
  const worker = context.serviceWorkers()[0]!;
  const tabId = await worker.evaluate(async () => {
    const tabs = await chrome.tabs.query({ title: 'Fixture' }); return tabs[0]!.id!;
  });
  const control = await context.newPage(); await control.goto(`chrome-extension://${extensionId}/popup.html`);
  const shared = await control.evaluate(async (id) => chrome.runtime.sendMessage({ type: 'tab.toggle', tabId: id }), tabId);
  expect(shared).toMatchObject({ ok: true, shared: true });
  const state = await control.evaluate(async () => chrome.runtime.sendMessage({ type: 'state' }));
  expect(state.tabs).toEqual([expect.objectContaining({ tabId })]);
  const revoked = await control.evaluate(async (id) => chrome.runtime.sendMessage({ type: 'tab.toggle', tabId: id }), tabId);
  expect(revoked).toMatchObject({ ok: true, shared: false });
});

test('executes a deterministic signed-in shopping journey over the relay', async ({ context, extensionId }) => {
  let extensionSocket: WebSocket | undefined;
  const messages: unknown[] = [];
  // Locate the test relay's ephemeral port without exposing a credential in the URL.
  const relayServer = new WebSocketServer({ port: 0, host: '127.0.0.1', handleProtocols(protocols) {
    return protocols.has('tinyflows.v1') ? 'tinyflows.v1' : false;
  }});
  try {
  await new Promise<void>((resolveListening) => relayServer.once('listening', resolveListening));
  const relayConnected = new Promise<void>((resolveConnected) => {
    relayServer.on('connection', (socket) => {
      extensionSocket = socket;
      socket.on('message', (data) => messages.push(JSON.parse(data.toString())));
      resolveConnected();
    });
  });
  const address = relayServer.address();
  if (!address || typeof address === 'string') throw new Error('missing relay port');

  await context.route('http://shop.tinyflows.test/', (route) => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><title>TinyFlows Shop</title>
      <form id="login"><input id="email"><button id="sign-in">Sign in</button></form>
      <main id="shop" hidden><input id="search"><button id="result"><span class="product-name">Trail Boot</span></button><p id="details" hidden>Trail Boot · $89</p></main>
      <script>
        login.addEventListener('submit', event => { event.preventDefault(); login.hidden=true; shop.hidden=false; });
        result.addEventListener('click', () => { details.hidden=false; });
      </script>`
  }));
  const target = await context.newPage();
  await target.goto('http://shop.tinyflows.test/');
  const worker = context.serviceWorkers()[0]!;
  const tabId = await worker.evaluate(async () => {
    const tabs = await chrome.tabs.query({ title: 'TinyFlows Shop' }); return tabs[0]!.id!;
  });
  const control = await context.newPage();
  await control.goto(`chrome-extension://${extensionId}/popup.html`);
  await control.evaluate(async ({ port }) => chrome.runtime.sendMessage({
    type: 'relay.configure',
    url: `ws://127.0.0.1:${port}`,
    pairingToken: '0123456789abcdef0123456789abcdef'
  }), { port: address.port });
  await relayConnected;
  await control.evaluate(async (id) => chrome.runtime.sendMessage({ type: 'tab.toggle', tabId: id }), tabId);
  await expect.poll(() => messages.some((message) => (message as {event?:string}).event === 'tab_shared')).toBe(true);

  let sequence = 0;
  async function action(value: Record<string, unknown>): Promise<unknown> {
    const requestId = `journey:${++sequence}`;
    extensionSocket!.send(JSON.stringify({
      protocol_version: 1, request_id: requestId, run_id: 'journey', tab_id: tabId,
      timeout_ms: 5000, action: value
    }));
    await expect.poll(() => messages.find((message) =>
      (message as {status?:string;request_id?:string}).status &&
      (message as {request_id?:string}).request_id === requestId
    )).toBeTruthy();
    const response = messages.find((message) =>
      (message as {status?:string;request_id?:string}).status &&
      (message as {request_id?:string}).request_id === requestId
    ) as {status:string;result?:{data:unknown};error?:unknown};
    expect(response.status, JSON.stringify(response.error)).toBe('ok');
    return response.result?.data;
  }

  await action({ action: 'fill', selector: '#email', value: 'person@example.com' });
  await action({ action: 'click', selector: '#sign-in' });
  expect(await action({ action: 'is_visible', selector: '#shop' })).toBe(true);
  await action({ action: 'fill', selector: '#search', value: 'Trail Boot' });
  expect(await action({ action: 'get_text', selector: '.product-name' })).toBe('Trail Boot');
  await action({ action: 'click', selector: '#result' });
  expect(await action({ action: 'get_text', selector: '#details' })).toContain('$89');
  const closeRequestId = `journey:${sequence + 1}`;
  await action({ action: 'close' });
  await expect.poll(() => messages.some((message) =>
    (message as {event?:string;tab_id?:number}).event === 'tab_revoked' &&
    (message as {tab_id?:number}).tab_id === tabId
  )).toBe(true);
  const closeResponseIndex = messages.findIndex((message) =>
    (message as {status?:string;request_id?:string}).status === 'ok' &&
    (message as {request_id?:string}).request_id === closeRequestId
  );
  const revokeIndex = messages.findIndex((message) =>
    (message as {event?:string;tab_id?:number}).event === 'tab_revoked' &&
    (message as {tab_id?:number}).tab_id === tabId
  );
  expect(closeResponseIndex).toBeGreaterThanOrEqual(0);
  expect(revokeIndex).toBeGreaterThan(closeResponseIndex);

  } finally {
    extensionSocket?.close();
    await new Promise<void>((resolveClosed) => relayServer.close(() => resolveClosed()));
  }
});
