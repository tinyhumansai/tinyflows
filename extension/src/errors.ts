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

export function toBrowserError(error: unknown): BrowserError {
  if (error instanceof BrowserError) return error;
  const message = error instanceof Error ? error.message : String(error);
  if (message.toLowerCase().includes('no tab with id')) {
    return new BrowserError('tab_revoked', 'The shared tab no longer exists');
  }
  return new BrowserError('browser_failure', message || 'Unknown browser error');
}
