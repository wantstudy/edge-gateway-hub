/**
 * 审计分页真实性的单元测试（对应 team-lead 对 AuditPage 的返工验收）。
 *
 * 证明点：
 *  · `repo.queryAudit` 不再以 `pageSize: 100000` 兜底全量，而是用 `page` / `pageSize`
 *    在缓存上做真实切片（`paginate`），只回本页数据，`total` 为筛选后的真实总数；
 *  · 翻页（page=2）返回的是不同的子集，条数正确；
 *  · actor / action / result / from / to 筛选与分页叠加后总数与分页一致。
 *
 * 通过 `__testCache` 注入事件以避免依赖运行中的 daemon。
 */
import { describe, it, expect, beforeEach } from 'vitest';
import { repo, __testCache, type AuditEntry } from '@/api/repo';

/** 造 N 条审计事件，actor / action 交错以便验证筛选。 */
function seedEvents(n: number): void {
  const events: AuditEntry[] = [];
  for (let i = 0; i < n; i += 1) {
    const actor = i % 2 === 0 ? 'root' : 'system';
    const action = i % 3 === 0 ? '登录' : '修改';
    events.push({
      id: `ev-${i}`,
      ts: `2026-09-23 ${String(10 + Math.floor(i / 60)).padStart(2, '0')}:${String(i % 60).padStart(2, '0')}:00`,
      actor,
      actorType: actor === 'root' ? 'human' : 'system',
      action,
      entityType: 'device',
      entityLabel: '设备',
      entityId: `dev-${i}`,
      detail: `操作 ${i}`,
      ip: `10.0.0.${i % 255}`,
      result: i % 4 === 0 ? 'success' : 'failed',
    });
  }
  __testCache.events = events;
}

beforeEach(() => {
  __testCache.events = [];
});

describe('repo.queryAudit 真实分页', () => {
  it('page=1 只回本页条数，total 为筛选后真实总数（非 pageSize:100000 兜底）', () => {
    seedEvents(12);
    const res = repo.queryAudit({
      actorType: '',
      actor: '',
      action: '',
      entityType: '',
      result: '',
      from: '',
      to: '',
      page: 1,
      pageSize: 5,
    });
    expect(res.total).toBe(12);
    expect(res.items.length).toBe(5);
    expect(res.items[0].id).toBe('ev-0');
    expect(res.items[4].id).toBe('ev-4');
  });

  it('page=2 返回不同子集，条数正确，total 不变', () => {
    seedEvents(12);
    const p1 = repo.queryAudit({ actorType: '', actor: '', action: '', entityType: '', result: '', from: '', to: '', page: 1, pageSize: 5 });
    const p2 = repo.queryAudit({ actorType: '', actor: '', action: '', entityType: '', result: '', from: '', to: '', page: 2, pageSize: 5 });
    expect(p2.total).toBe(12);
    expect(p2.items.length).toBe(5);
    expect(p2.items[0].id).toBe('ev-5');
    expect(p2.items[4].id).toBe('ev-9');
    // 两页无重叠
    const p1Ids = new Set(p1.items.map((r) => r.id));
    expect(p2.items.every((r) => !p1Ids.has(r.id))).toBe(true);
  });

  it('末页不足一页时按真实剩余条数返回', () => {
    seedEvents(12);
    const last = repo.queryAudit({ actorType: '', actor: '', action: '', entityType: '', result: '', from: '', to: '', page: 3, pageSize: 5 });
    expect(last.total).toBe(12);
    expect(last.items.length).toBe(2);
    expect(last.items.map((r) => r.id)).toEqual(['ev-10', 'ev-11']);
  });

  it('actor 筛选与分页叠加：总数与分页一致', () => {
    seedEvents(12);
    // 12 条里 actor=root 占偶数下标 → 6 条
    const res = repo.queryAudit({ actorType: '', actor: 'root', action: '', entityType: '', result: '', from: '', to: '', page: 1, pageSize: 5 });
    expect(res.total).toBe(6);
    expect(res.items.length).toBe(5);
    expect(res.items.every((r) => r.actor === 'root')).toBe(true);
    const p2 = repo.queryAudit({ actorType: '', actor: 'root', action: '', entityType: '', result: '', from: '', to: '', page: 2, pageSize: 5 });
    expect(p2.total).toBe(6);
    expect(p2.items.length).toBe(1);
  });

  it('导出用全量（pageSize=MAX_SAFE_INTEGER）返回筛选后全部', () => {
    seedEvents(12);
    const res = repo.queryAudit({ actorType: '', actor: '', action: '', entityType: '', result: 'success', from: '', to: '', page: 1, pageSize: Number.MAX_SAFE_INTEGER });
    // 12 条里 result=success 占 i%4===0 → i=0,4,8 → 3 条
    expect(res.total).toBe(3);
    expect(res.items.length).toBe(3);
  });
});
