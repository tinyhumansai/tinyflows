import { describe, expect, it } from 'vitest';
import { BrowserError, toBrowserError } from '../src/errors';

describe('browser errors', () => {
  it('preserves stable errors and classifies Chrome failures', () => {
    const stable = new BrowserError('cancelled', 'cancelled');
    expect(toBrowserError(stable)).toBe(stable);
    expect(toBrowserError(new Error('No tab with id: 7'))).toMatchObject({ code: 'tab_revoked' });
    expect(toBrowserError(new Error('debugger failed'))).toMatchObject({ code: 'browser_failure', message: 'debugger failed' });
    expect(toBrowserError('bad')).toMatchObject({ code: 'browser_failure', message: 'bad' });
  });
});
