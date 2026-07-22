import { describe, expect, it } from 'vitest';
import { BrowserError, toBrowserError } from '../src/errors';

describe('browser errors', () => {
  it('preserves stable errors and classifies Chrome failures', async () => {
    const stable = new BrowserError('cancelled', 'cancelled');
    await expect(toBrowserError(stable)).resolves.toBe(stable);
    const missing = { get: async () => { throw new Error('wording may change'); } };
    await expect(toBrowserError(new Error('debugger failed'), 7, missing)).resolves.toMatchObject({ code: 'tab_revoked' });
    const existing = { get: async () => ({ id: 7 }) as chrome.tabs.Tab };
    await expect(toBrowserError(new Error('No tab with id: 7'), 7, existing)).resolves.toMatchObject({
      code: 'browser_failure', message: 'No tab with id: 7'
    });
    await expect(toBrowserError('bad')).resolves.toMatchObject({ code: 'browser_failure', message: 'bad' });
  });
});
