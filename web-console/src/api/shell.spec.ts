import { describe, it, expect } from 'vitest';
import {
  shellAvailable,
  supervisorStatus,
  setSupervisor,
  describeSupervisorSupport,
} from './shell';

// 浏览器 / Node 环境下 `window.__TAURI_INTERNALS__` 不存在 → shellAvailable 必须为 false，
// 守护能力诚实降级（绝不伪造）。
describe('shell (桌面壳桥)', () => {
  it('浏览器环境下 shellAvailable 为 false', () => {
    expect(shellAvailable).toBe(false);
  });

  it('supervisorStatus 在非桌面端返回 null（不抛异常、不伪造）', async () => {
    expect(await supervisorStatus()).toBeNull();
  });

  it('setSupervisor 在非桌面端返回 ok:false + 真实原因 + supported:false 占位', async () => {
    const res = await setSupervisor({ crash_restart: true });
    expect(res.ok).toBe(false);
    expect(res.state.supported).toBe(false);
    expect(res.message).toContain('浏览器');
  });

  it('describeSupervisorSupport 对 null 返回「不可得」', () => {
    expect(describeSupervisorSupport(null)).toBe('不可得');
  });

  it('describeSupervisorSupport 对 supported:false 透传 reason', () => {
    expect(
      describeSupervisorSupport({
        supported: false,
        running: false,
        restarts: 0,
        crash_restart: false,
        watchdog: false,
        boot_failure_guard: false,
        consecutive_failures: 0,
        last_exit: null,
        reason: '桌面端守护不可用',
      }),
    ).toContain('桌面端守护不可用');
  });
});
