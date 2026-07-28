import type { BrowserErrorCode } from './protocol';

export class BrowserError extends Error {
  constructor(
    public readonly code: BrowserErrorCode,
    message: string,
    public readonly retryable = false
  ) {
    super(message);
    this.name = 'BrowserError';
  }
}

type TabsApi = Pick<typeof chrome.tabs, 'get'>;

export async function toBrowserError(
  error: unknown,
  tabId?: number,
  tabsApi?: TabsApi
): Promise<BrowserError> {
  if (error instanceof BrowserError) return error;
  const message = error instanceof Error ? error.message : String(error);
  if (tabId !== undefined) {
    try { await (tabsApi ?? chrome.tabs).get(tabId); }
    catch { return new BrowserError('tab_revoked', 'The shared tab no longer exists'); }
  }
  return new BrowserError('browser_failure', message || 'Unknown browser error');
}
