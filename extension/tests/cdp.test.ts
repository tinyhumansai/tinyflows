import { describe, expect, it, vi } from 'vitest';
import { CdpExecutor } from '../src/cdp';
import type { BrowserAction, BrowserRequest } from '../src/protocol';

function request(action: BrowserAction, timeout_ms = 1000): BrowserRequest {
  return { protocol_version: 1, request_id: 'r', run_id: 'run', tab_id: 7, timeout_ms, action };
}
function executor(responses: unknown[] = []) {
  const sendCommand = vi.fn(async () => responses.shift() ?? {});
  const remove = vi.fn(async () => undefined);
  return { instance: new CdpExecutor({ sendCommand } as any, { remove } as any), sendCommand, remove };
}

describe('CDP action execution', () => {
  it('navigates only to HTTP pages and returns structured data', async () => {
    const { instance, sendCommand } = executor([
      { result: { value: true } }, { loaderId: 'destination' },
      { result: { value: { ready: 'complete', previousDocument: true } } },
      { result: { value: { ready: 'complete', previousDocument: false } } }
    ]);
    await expect(instance.execute(request({ action: 'open', url: 'https://example.com' }))).resolves.toEqual({ url: 'https://example.com' });
    expect(sendCommand).toHaveBeenCalledWith({ tabId: 7 }, 'Page.navigate', { url: 'https://example.com' });
    expect(sendCommand).toHaveBeenCalledTimes(4);
    await expect(instance.execute(request({ action: 'open', url: 'chrome://settings' }))).rejects.toMatchObject({ code: 'unsupported_page' });
  });

  it('surfaces navigation failures instead of reporting an open success', async () => {
    const { instance } = executor([{ result: { value: true } }, { errorText: 'net::ERR_FAILED' }]);
    await expect(instance.execute(request({ action: 'open', url: 'https://example.com' }))).rejects.toMatchObject({
      code: 'browser_failure', message: 'net::ERR_FAILED'
    });
  });

  it('executes evaluate, keyboard, mouse, screenshot, and close actions', async () => {
    const { instance, sendCommand, remove } = executor([
      { result: { value: 'A title' } }, { result: { value: { x: 10, y: 20 } } },
      {}, {}, { data: 'png' }, {}, {}
    ]);
    await expect(instance.execute(request({ action: 'get_title' }))).resolves.toBe('A title');
    await expect(instance.execute(request({ action: 'click', selector: '#go' }))).resolves.toEqual({ clicked: true });
    await expect(instance.execute(request({ action: 'screenshot' }))).resolves.toEqual({ data: 'png' });
    await expect(instance.execute(request({ action: 'press', key: 'Enter' }))).resolves.toEqual({ pressed: 'Enter' });
    await expect(instance.execute(request({ action: 'close' }))).resolves.toEqual({ closed: true });
    expect(remove).toHaveBeenCalledWith(7);
    expect((sendCommand.mock.calls as unknown[][]).some((call) => call[1] === 'Input.dispatchMouseEvent')).toBe(true);
    expect((sendCommand.mock.calls as unknown[][]).some((call) =>
      call[1] === 'Runtime.evaluate' && JSON.stringify(call[2]).includes('scrollIntoView')
    )).toBe(true);
  });

  it('fails with stable errors when an element disappears or an action times out', async () => {
    const missing = executor([{ result: { value: null } }]).instance;
    await expect(missing.execute(request({ action: 'hover', selector: '.gone' }))).rejects.toMatchObject({ code: 'element_not_found' });
    vi.useFakeTimers();
    const slow = new CdpExecutor({ sendCommand: () => new Promise(() => undefined) } as any, { remove: vi.fn() } as any);
    const pending = slow.execute(request({ action: 'snapshot' }, 100));
    const rejected = expect(pending).rejects.toMatchObject({ code: 'action_timeout', retryable: true });
    await vi.advanceTimersByTimeAsync(100);
    await rejected;
    vi.useRealTimers();
  });

  it('fills, types, scrolls, waits, and finds semantically', async () => {
    const { instance, sendCommand } = executor([
      { result: { value: true } }, { result: { value: true } }, {}, {},
      { result: { value: true } }, { result: { value: [{ tag: 'button', text: 'Buy' }] } }
    ]);
    await expect(instance.execute(request({ action: 'fill', selector: '#q', value: 'abc' }))).resolves.toEqual({ filled: true });
    await expect(instance.execute(request({ action: 'type', selector: '#q', text: 'd' }))).resolves.toEqual({ typed: true });
    await expect(instance.execute(request({ action: 'scroll', y: 20 }))).resolves.toEqual({ x: 0, y: 20 });
    await expect(instance.execute(request({ action: 'wait', selector: '#done', duration_ms: 100 }))).resolves.toEqual({ visible: true });
    await expect(instance.execute(request({ action: 'find', query: 'Buy' }))).resolves.toEqual([{ tag: 'button', text: 'Buy' }]);
    expect(sendCommand).toHaveBeenCalled();
  });

  it('reads text and visibility and clips element screenshots', async () => {
    const { instance, sendCommand } = executor([
      { result: { value: 'text' } }, { result: { value: true } },
      { result: { value: { x: 1, y: 2, width: 30, height: 40, scale: 1 } } }, { data: 'clip' },
      { cssContentSize: { x: 0, y: 0, width: 100, height: 200 } }, { data: 'full' }
    ]);
    await expect(instance.execute(request({ action: 'get_text', selector: '#x' }))).resolves.toBe('text');
    await expect(instance.execute(request({ action: 'is_visible', selector: '#x' }))).resolves.toBe(true);
    await expect(instance.execute(request({ action: 'screenshot', selector: '#x' }))).resolves.toEqual({ data: 'clip' });
    await expect(instance.execute(request({ action: 'screenshot', full_page: true }))).resolves.toEqual({ data: 'full' });
    expect(sendCommand).toHaveBeenCalledWith({ tabId: 7 }, 'Page.captureScreenshot', expect.objectContaining({ clip: expect.any(Object) }));
  });
});
