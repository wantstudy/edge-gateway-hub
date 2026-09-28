/**
 * @file status-map.spec.ts
 * @description 状态字典断言：保证「固定色 + 固定文案」，未知枚举不被静默丢弃。
 */
import { describe, it, expect } from 'vitest';
import { STATUS_MAP, statusView } from '../src/status-map';

describe('statusView', () => {
  it('四个激活码状态文案固定', () => {
    expect(statusView('issued')).toEqual({ label: '已发放', tone: 'info' });
    expect(statusView('bound')).toEqual({ label: '已绑定', tone: 'ok' });
    expect(statusView('revoked')).toEqual({ label: '已废弃', tone: 'danger' });
    expect(statusView('reissued')).toEqual({ label: '已重发', tone: 'warn' });
  });

  it('试用 / 宽限 / 降级统一用 warn，不与故障红混用', () => {
    for (const key of ['trial', 'trial_ending', 'grace', 'degraded']) {
      expect(statusView(key).tone).toBe('warn');
    }
  });

  it('空值与未知值降级为 unknown 且不抛错', () => {
    expect(statusView(null)).toEqual({ label: '—', tone: 'unknown' });
    expect(statusView('')).toEqual({ label: '—', tone: 'unknown' });
    expect(statusView('某种新状态')).toEqual({ label: '某种新状态', tone: 'unknown' });
  });

  it('每个状态都配置了文案与色调', () => {
    for (const [key, view] of Object.entries(STATUS_MAP)) {
      expect(view.label.length, `状态 ${key} 缺文案`).toBeGreaterThan(0);
      expect(['ok', 'warn', 'danger', 'info', 'unknown']).toContain(view.tone);
    }
  });
});
