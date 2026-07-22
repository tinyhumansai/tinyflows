import { describe, expect, it, vi } from 'vitest';
import { GROUP_TITLE, TabManager } from '../src/tab-manager';

function api(tabOverrides: Record<string, unknown> = {}) {
  let stored: Record<string, unknown> = {};
  const tab = { id: 5, windowId: 1, groupId: -1, url: 'https://example.com', ...tabOverrides };
  const mock = {
    tabs: {
      get: vi.fn(async () => tab), group: vi.fn(async () => { tab.groupId = 9; return 9; }),
      ungroup: vi.fn(async () => { tab.groupId = -1; })
    },
    tabGroups: {
      query: vi.fn(async () => []), update: vi.fn(async () => ({ id: 9 })),
      get: vi.fn(async () => ({ id: 9, title: GROUP_TITLE }))
    },
    debugger: { attach: vi.fn(async () => undefined), detach: vi.fn(async () => undefined), getTargets: vi.fn(async () => []) },
    storage: { local: { get: vi.fn(async () => stored), set: vi.fn(async (value) => { stored = value; }) } },
    action: { setBadgeText: vi.fn(async () => undefined), setBadgeBackgroundColor: vi.fn(async () => undefined) }
  };
  return { mock, tab, setStored: (value: Record<string, unknown>) => { stored = value; } };
}

describe('explicit tab sharing', () => {
  it('groups and attaches a tab, then revokes it explicitly', async () => {
    const { mock, tab } = api(); const manager = new TabManager(mock as any);
    await expect(manager.share(5)).resolves.toMatchObject({ tabId: 5, groupId: 9 });
    expect(mock.tabGroups.update).toHaveBeenCalledWith(9, expect.objectContaining({ title: GROUP_TITLE }));
    expect(mock.debugger.attach).toHaveBeenCalledWith({ tabId: 5 }, '1.3');
    expect(manager.has(5)).toBe(true);
    await manager.revoke(5);
    expect(tab.groupId).toBe(-1); expect(manager.has(5)).toBe(false);
  });

  it('fails closed for unshared, regrouped, and restricted tabs', async () => {
    const fixture = api(); const manager = new TabManager(fixture.mock as any);
    await expect(manager.assertShared(5)).rejects.toMatchObject({ code: 'tab_not_shared' });
    await manager.share(5); fixture.tab.groupId = 22;
    await expect(manager.assertShared(5)).rejects.toMatchObject({ code: 'tab_revoked' });
    const restricted = api({ url: 'chrome://settings' });
    await expect(new TabManager(restricted.mock as any).share(5)).rejects.toMatchObject({ code: 'unsupported_page' });
  });

  it('rehydrates only valid saved tabs and restores debugger sessions', async () => {
    const fixture = api({ groupId: 9 });
    fixture.setStored({ 'tinyflows.sharedTabs.v1': [{ tabId: 5, groupId: 9, windowId: 1, attachedAt: 1 }, { bad: true }] });
    const manager = new TabManager(fixture.mock as any);
    await expect(manager.rehydrate()).resolves.toHaveLength(1);
    expect(fixture.mock.debugger.attach).toHaveBeenCalledWith({ tabId: 5 }, '1.3');
  });

  it('reuses the named group, toggles sharing, and updates all badges', async () => {
    const fixture = api(); fixture.mock.tabGroups.query.mockResolvedValue([{ id: 12 }] as any);
    const manager = new TabManager(fixture.mock as any);
    await expect(manager.toggle(5)).resolves.toBe(true);
    expect(fixture.mock.tabs.group).toHaveBeenCalledWith({ groupId: 12, tabIds: [5] });
    await manager.markAll('reconnecting');
    await expect(manager.toggle(5)).resolves.toBe(false);
  });

  it('revokes a saved tab when its group was renamed', async () => {
    const fixture = api({ groupId: 9 }); fixture.setStored({ 'tinyflows.sharedTabs.v1': [{ tabId: 5, groupId: 9, windowId: 1, attachedAt: 1 }] });
    fixture.mock.tabGroups.get.mockResolvedValue({ id: 9, title: 'Other' } as any);
    const manager = new TabManager(fixture.mock as any); await manager.rehydrate();
    expect(manager.list()).toEqual([]);
  });
});
