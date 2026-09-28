/**
 * @file repo.ota-apply.spec.ts
 * @module web-console/api/repo.ota-apply.spec
 * @description `repo.ops.applyUpdate` 的 wire 契约回归（网关「系统更新」执行端点）。
 *
 * 后端 `POST /api/updates/apply` 是**危险操作硬契约**：body 必须恰好是
 * `{reason, note, confirm}` 三个**彼此独立**的字段（`note` 绝不并入 `reason`），
 * 任一缺失 / trim 后空白 / 出现未知字段 → 400 `validation_failed`。
 *
 * 前端红线（本文件守护）：
 *  1. 实际发出的 body 只有三个键，`note` 没有被拼进 `reason`；
 *  2. 后端 400 时前端**不报成功**（结构化失败 + 后端原文）；
 *  3. `accepted:true, applied:false` 的文案**不含「成功」**——替换由宿主安装器
 *     在重启时完成，结果以重启后的实际版本号为准。
 */
import { describe, it, expect, vi } from 'vitest';
import { repo, type UpdateApplyOutcome } from './repo';
import { API_BASE } from './client';

/** 把绝对请求地址归一为**路径**后再断言（基址与契约无关）。 */
const pathOf = (url: string): string => (url.startsWith(API_BASE) ? url.slice(API_BASE.length) : url);

interface RecordedCall {
  url: string;
  init?: RequestInit;
}

/** 打桩 `globalThis.fetch`：记录每次调用并返回指定状态码 + JSON 响应体。 */
function stubFetch(status: number, payload: unknown): { calls: RecordedCall[]; restore: () => void } {
  const calls: RecordedCall[] = [];
  const spy = vi.spyOn(globalThis, 'fetch').mockImplementation((async (
    input: RequestInfo | URL,
    init?: RequestInit,
  ) => {
    calls.push({ url: pathOf(String(input)), init });
    return new Response(JSON.stringify(payload), {
      status,
      headers: { 'Content-Type': 'application/json' },
    });
  }) as typeof fetch);
  return { calls, restore: () => spy.mockRestore() };
}

/** 只取 POST 调用（避免把可能跟随的 GET 计入）。 */
function postsOf(calls: RecordedCall[]): RecordedCall[] {
  return calls.filter((c) => c.init?.method === 'POST');
}

describe('repo.ops.applyUpdate（POST /api/updates/apply 三字段硬契约）', () => {
  it('请求体恰好是 {reason, note, confirm} 三键，note 未并入 reason；supported:false 时原文展示 reason', async () => {
    const { calls, restore } = stubFetch(200, {
      supported: false,
      accepted: false,
      applied: false,
      current_version: '1.0.0',
      target_version: null,
      source: 'unconfigured',
      source_configured: false,
      reason: '尚未配置升级源，无法执行更新。',
    });

    let outcome: UpdateApplyOutcome;
    try {
      outcome = await repo.ops.applyUpdate({
        reason: '安全漏洞修复',
        note: '现场存在 CVE-2026-0001，需尽快修复',
        confirm: 'gw-local-dev',
      });
    } finally {
      restore();
    }

    const posts = postsOf(calls);
    expect(posts).toHaveLength(1);
    expect(posts[0].url).toBe('/api/updates/apply');
    const body = JSON.parse(String(posts[0].init?.body)) as Record<string, unknown>;

    // 恰好三个键，顺序无关。
    expect(Object.keys(body).sort()).toEqual(['confirm', 'note', 'reason']);
    expect(body['reason']).toBe('安全漏洞修复');
    expect(body['note']).toBe('现场存在 CVE-2026-0001，需尽快修复');
    expect(body['confirm']).toBe('gw-local-dev');
    // note 绝不被拼进 reason。
    expect(String(body['reason'])).not.toContain('CVE-2026-0001');

    // supported:false → 结构化失败 + 后端 reason 原文，绝不说「成功」。
    expect(outcome.accepted).toBe(false);
    expect(outcome.applied).toBe(false);
    expect(outcome.result?.supported).toBe(false);
    expect(outcome.result?.currentVersion).toBe('1.0.0');
    expect(outcome.result?.targetVersion).toBe('');
    expect(outcome.message).toContain('尚未配置升级源，无法执行更新。');
    expect(outcome.message).not.toContain('成功');
  });

  it('后端 400 validation_failed → 前端不报成功，透传后端 message 原文，result 为 null', async () => {
    const { calls, restore } = stubFetch(400, {
      error: 'validation_failed',
      message:
        'field "note" must not be blank (dangerous-op contract: `reason`, `note` and `confirm` are three independent non-empty fields)',
    });

    let outcome: UpdateApplyOutcome;
    try {
      outcome = await repo.ops.applyUpdate({ reason: '功能升级', note: '   ', confirm: 'gw-local-dev' });
    } finally {
      restore();
    }

    expect(postsOf(calls)).toHaveLength(1);
    expect(outcome.accepted).toBe(false);
    expect(outcome.applied).toBe(false);
    expect(outcome.result).toBeNull();
    expect(outcome.message).toContain('400');
    expect(outcome.message).toContain('field "note" must not be blank');
    expect(outcome.message).not.toContain('成功');
  });

  it('accepted:true, applied:false → 文案如实说明「重启后由宿主安装器替换」，不含「成功」', async () => {
    const { calls, restore } = stubFetch(200, {
      supported: true,
      accepted: true,
      applied: false,
      current_version: '1.0.0',
      target_version: '1.1.0',
      source: 'https://updates.example.com',
      source_configured: true,
      reason: '',
    });

    let outcome: UpdateApplyOutcome;
    try {
      outcome = await repo.ops.applyUpdate({
        reason: '缺陷修复',
        note: '修复现场采集抖动问题，已通过试产线验证',
        confirm: 'gw-local-dev',
      });
    } finally {
      restore();
    }

    expect(postsOf(calls)).toHaveLength(1);
    expect(outcome.accepted).toBe(true);
    expect(outcome.applied).toBe(false);
    // snake_case → camelCase 透传。
    expect(outcome.result?.sourceConfigured).toBe(true);
    expect(outcome.result?.targetVersion).toBe('1.1.0');
    // 如实文案：重启后由宿主安装器替换；绝不出现「成功」。
    expect(outcome.message).toContain('重启');
    expect(outcome.message).toContain('1.1.0');
    expect(outcome.message).not.toContain('成功');
  });

  it('accepted:false（supported:true）→ 原文展示后端 reason，不自行编造', async () => {
    const { restore } = stubFetch(200, {
      supported: true,
      accepted: false,
      applied: false,
      current_version: '1.0.0',
      target_version: null,
      source: 'https://updates.example.com',
      source_configured: true,
      reason: '当前已是最新版本，无可用更新包。',
    });

    let outcome: UpdateApplyOutcome;
    try {
      outcome = await repo.ops.applyUpdate({ reason: '功能升级', note: '例行升级到最新版本', confirm: 'gw-local-dev' });
    } finally {
      restore();
    }

    expect(outcome.accepted).toBe(false);
    expect(outcome.message).toContain('当前已是最新版本，无可用更新包。');
    expect(outcome.message).not.toContain('成功');
  });

  it('403 权限不足 → 结构化失败，不报成功；404 → 接口未上线，诚实说明', async () => {
    const forbidden = stubFetch(403, { error: 'forbidden', message: 'permission denied' });
    let denied: UpdateApplyOutcome;
    try {
      denied = await repo.ops.applyUpdate({ reason: '功能升级', note: '例行升级到最新版本', confirm: 'gw-local-dev' });
    } finally {
      forbidden.restore();
    }
    expect(denied.accepted).toBe(false);
    expect(denied.message).toContain('403');
    expect(denied.message).not.toContain('成功');

    const missing = stubFetch(404, { error: 'not_found' });
    let notDeployed: UpdateApplyOutcome;
    try {
      notDeployed = await repo.ops.applyUpdate({ reason: '功能升级', note: '例行升级到最新版本', confirm: 'gw-local-dev' });
    } finally {
      missing.restore();
    }
    expect(notDeployed.accepted).toBe(false);
    expect(notDeployed.message).toContain('未上线');
    expect(notDeployed.message).not.toContain('成功');
  });
});
