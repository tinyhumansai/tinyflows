import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { isBrowserAction, isBrowserCancel, isBrowserRequest, isControlResponse, isRunEvent, tabSharedEvent } from '../src/protocol';

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
    expect(isBrowserRequest({ ...base, timeout_ms: 0 })).toBe(false);
    expect(isBrowserRequest({ ...base, timeout_ms: 60_001 })).toBe(false);
    expect(isBrowserRequest({ ...base, action: { action: 'click' } })).toBe(false);
    expect(isBrowserAction({ action: 'snapshot', surprise: true })).toBe(false);
    expect(isBrowserAction({ action: 'unknown' })).toBe(false);
    expect(isBrowserAction(null)).toBe(false);
  });

  it('validates control responses and run events separately', () => {
    expect(isControlResponse({ protocol_version: 1, status: 'workflows', request_id: 'r', workflows: [] })).toBe(true);
    expect(isControlResponse({ protocol_version: 1, status: 'ok', request_id: '', result: null })).toBe(false);
    expect(isRunEvent({ event: 'step_started', protocol_version: 1, run_id: 'r', node_id: 'n', node_kind: 'tool_call' })).toBe(true);
    expect(isRunEvent({ event: 'awaiting_approval', protocol_version: 1, run_id: 'r', pending_approvals: ['gate'] })).toBe(true);
    expect(isRunEvent({ event: 'browser_action_started', protocol_version: 1, run_id: 'r', request_id: 'q', tab_id: 1, action: 'click' })).toBe(true);
    expect(isRunEvent({ event: 'cancelled', protocol_version: 1, run_id: 'r', extra: 1 })).toBe(false);
  });

  it('builds the strict canonical shared-tab announcement', () => {
    expect(tabSharedEvent({ id: 9, window_id: 2, url: 'https://example.com', title: 'Example' })).toEqual({
      event: 'tab_shared', protocol_version: 1,
      tab: { id: 9, window_id: 2, url: 'https://example.com', title: 'Example' }
    });
  });

  it('accepts the same canonical repository fixture as Rust', () => {
    const fixtureUrl = new URL('../../protocol/fixtures/browser-request.v1.json', import.meta.url);
    const fixture: unknown = JSON.parse(readFileSync(fixtureUrl, 'utf8'));
    expect(isBrowserRequest(fixture)).toBe(true);
    const cancelUrl = new URL('../../protocol/fixtures/browser-cancel.v1.json', import.meta.url);
    expect(isBrowserCancel(JSON.parse(readFileSync(cancelUrl, 'utf8')))).toBe(true);
  });
});
