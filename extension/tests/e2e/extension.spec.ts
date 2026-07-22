import { test as base, chromium, expect } from '@playwright/test';
import type { BrowserContext } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

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
