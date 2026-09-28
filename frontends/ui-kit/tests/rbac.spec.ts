/**
 * @file rbac.spec.ts
 * @description RBAC 页面级 / 操作级权限矩阵的断言。
 *              这是「权限矩阵真实生效」的底层保障：矩阵错误则页面门控必然错误。
 */
import { describe, it, expect } from 'vitest';
import { canSeePage, can, firstAllowedPage, PAGES, ROLE_META, ROLES, ACTION_MATRIX } from '../src/rbac';

describe('页面级门控 canSeePage', () => {
  it('总览（概览）对四角色全可见', () => {
    for (const role of ROLES) {
      expect(canSeePage(role, 'overview')).toBe(true);
    }
  });

  it('激活码管理 ops / lic_ops / system 可见（系统管理员全权）', () => {
    expect(canSeePage('ops', 'codes')).toBe(true);
    expect(canSeePage('lic_ops', 'codes')).toBe(true);
    expect(canSeePage('risk', 'codes')).toBe(false);
    expect(canSeePage('system', 'codes')).toBe(true);
  });

  it('租户与策略 / 密钥管理 / 账号与角色仅 system 可见', () => {
    for (const page of ['tenants', 'keys', 'accounts'] as const) {
      expect(canSeePage('system', page)).toBe(true);
      expect(canSeePage('ops', page)).toBe(false);
      expect(canSeePage('lic_ops', page)).toBe(false);
      expect(canSeePage('risk', page)).toBe(false);
    }
  });

  it('换机工单仅 lic_ops 可见', () => {
    expect(canSeePage('lic_ops', 'transfers')).toBe(true);
    expect(canSeePage('ops', 'transfers')).toBe(false);
  });

  it('审计日志 risk / system 可见，ops / lic_ops 不可见', () => {
    expect(canSeePage('risk', 'audit')).toBe(true);
    expect(canSeePage('system', 'audit')).toBe(true);
    expect(canSeePage('ops', 'audit')).toBe(false);
    expect(canSeePage('lic_ops', 'audit')).toBe(false);
  });

  it('回执与异常对四角色全可见（客服查询需要）', () => {
    for (const role of ROLES) {
      expect(canSeePage(role, 'receipts')).toBe(true);
    }
  });
});

describe('操作级门控 can', () => {
  it('废弃 / 重发 lic_ops 与 system 可用（系统管理员全权）', () => {
    expect(can('lic_ops', 'code.revoke')).toBe(true);
    expect(can('lic_ops', 'code.reissue')).toBe(true);
    expect(can('ops', 'code.revoke')).toBe(false);
    expect(can('ops', 'code.reissue')).toBe(false);
    expect(can('risk', 'code.revoke')).toBe(false);
    expect(can('system', 'code.revoke')).toBe(true);
    expect(can('system', 'code.reissue')).toBe(true);
  });

  it('发放激活码 ops / lic_ops 可用，且不包含废弃权限', () => {
    expect(can('ops', 'code.issue')).toBe(true);
    expect(can('lic_ops', 'code.issue')).toBe(true);
    expect(can('ops', 'code.revoke')).toBe(false);
  });

  it('密钥轮换仅 system 可用', () => {
    expect(can('system', 'key.rotate')).toBe(true);
    expect(can('ops', 'key.rotate')).toBe(false);
    expect(can('lic_ops', 'key.rotate')).toBe(false);
    expect(can('risk', 'key.rotate')).toBe(false);
  });

  it('审计导出仅 system 可用（risk 只读不可导出）', () => {
    expect(can('system', 'audit.export')).toBe(true);
    expect(can('risk', 'audit.export')).toBe(false);
  });

  it('明文揭示权限：lic_ops / system 可用，ops / risk 不可用', () => {
    expect(can('lic_ops', 'code.reveal')).toBe(true);
    expect(can('system', 'code.reveal')).toBe(true);
    expect(can('ops', 'code.reveal')).toBe(false);
    expect(can('risk', 'code.reveal')).toBe(false);
  });
});

describe('矩阵完整性', () => {
  it('每个角色都有非空的操作集合', () => {
    for (const role of ROLES) {
      expect(ACTION_MATRIX[role].length).toBeGreaterThan(0);
    }
  });

  it('每个页面的可见角色都来自 ROLES', () => {
    for (const page of PAGES) {
      expect(page.visibleTo.length).toBeGreaterThan(0);
      for (const role of page.visibleTo) {
        expect(ROLES).toContain(role);
      }
    }
  });

  it('每个角色都能落到一个非详情页作为首页（切换角色不会白屏）', () => {
    for (const role of ROLES) {
      const page = firstAllowedPage(role);
      expect(canSeePage(role, page)).toBe(true);
    }
  });

  it('角色元信息覆盖全部角色', () => {
    for (const role of ROLES) {
      expect(ROLE_META[role].label.length).toBeGreaterThan(0);
      expect(ROLE_META[role].fullLabel.length).toBeGreaterThan(0);
    }
  });
});
