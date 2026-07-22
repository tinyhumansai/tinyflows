import { describe, expect, it } from 'vitest';
import { isBrowserAction, isBrowserRequest, isControlResponse, isRunEvent } from '../src/protocol';

const base = {
  protocol_version: 1, request_id: 'req-1', run_id: 'run-1', tab_id: 4, timeout_ms: 1000,
  action: { action: 'click', selector: '#save' }
};

describe('browser protocol validation', () => {
  it('accepts the canonical request and every action shape', () => {
    expect(isBrowserRequest(base)).toBe(true);
    const actions = [
      { action: 'open', url: 'https://example.com' }, { action: 'snapshot' },
      { action: 'fill', selector: '#q', value: 'tiny' }, { action: 'type', text: 'hi', selector: '#q' },
      { action: 'get_text' }, { action: 'get_title' }, { action: 'get_url' },
      { action: 'screenshot', full_page: true }, { action: 'wait', duration_ms: 20, selector: '#x' },
      { action: 'press', key: 'Enter' }, { action: 'hover', selector: '#x' },
      { action: 'scroll', x: 0, y: 2 }, { action: 'is_visible', selector: '#x' },
      { action: 'close' }, { action: 'find', query: 'Buy' }
    ];
    for (const action of actions) expect(isBrowserAction(action), JSON.stringify(action)).toBe(true);
  });

  it('rejects wrong versions, unknown fields, invalid bounds, and malformed actions', () => {
    expect(isBrowserRequest({ ...base, protocol_version: 2 })).toBe(false);
    expect(isBrowserRequest({ ...base, extra: true })).toBe(false);
    expect(isBrowserRequest({ ...base, timeout_ms: 1 })).toBe(false);
    expect(isBrowserRequest({ ...base, action: { action: 'click' } })).toBe(false);
    expect(isBrowserAction({ action: 'snapshot', surprise: true })).toBe(false);
    expect(isBrowserAction({ action: 'unknown' })).toBe(false);
    expect(isBrowserAction(null)).toBe(false);
  });

  it('validates control responses and run events separately', () => {
    expect(isControlResponse({ protocol_version: 1, type: 'control.response', request_id: 'r', ok: true, result: [] })).toBe(true);
    expect(isControlResponse({ protocol_version: 1, type: 'control.response', request_id: '', ok: true })).toBe(false);
    expect(isRunEvent({ protocol_version: 1, type: 'run.event', run_id: 'r', event: 'step', data: {} })).toBe(true);
    expect(isRunEvent({ protocol_version: 1, type: 'run.event', run_id: 'r', event: 'step', data: {}, extra: 1 })).toBe(false);
  });
});
