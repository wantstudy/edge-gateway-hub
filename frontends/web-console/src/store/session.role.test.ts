/**
 * @file session.role.test.ts
 * @module web-console/store/session.role.test
 * @description UI 角色派生规则单测（`deriveRoleFromPerms` / `parseJwtPerms` /
 * `resolveRoleFromToken`）。
 *
 * 背景：带有效 token 刷新页面（F5）后 `loggedIn === true` 但 `role` 停在
 * `'viewer'`，会让 `RoleGate` 把写操作按钮替换成「无权操作」。修复方案是**启动时
 * 从 token 的 `perms` claim 派生 UI 角色**，本文件即该派生规则的回归护栏。
 *
 * ⚠️ 派生只影响按钮可见性，**不是授权判定**（判定在网关 Rust 侧）。
 */
import { describe, it, expect } from 'vitest';
import { deriveRoleFromPerms, parseJwtPerms, resolveRoleFromToken } from './session';

/** 构造一个（未签名/签名无意义的）JWT，仅用于解析测试。 */
function makeToken(payload: unknown): string {
  const seg = (value: unknown): string => Buffer.from(JSON.stringify(value)).toString('base64url');
  return `${seg({ alg: 'HS256', typ: 'JWT' })}.${seg(payload)}.signature`;
}

describe('deriveRoleFromPerms —— 权限集 → UI 角色', () => {
  it('backendRole 为 system 时直通 admin（系统管理员不被降权）', () => {
    expect(deriveRoleFromPerms([], 'system')).toBe('admin');
    expect(deriveRoleFromPerms(['device.view'], 'system')).toBe('admin');
  });

  it('含 account.update → admin', () => {
    expect(deriveRoleFromPerms(['account.view', 'account.update'])).toBe('admin');
  });

  it('同时含 ops.restart 与 device.write → admin', () => {
    expect(deriveRoleFromPerms(['ops.restart', 'device.write'])).toBe('admin');
  });

  it('仅含 ops.restart（无 device.write）→ operator（不误升为 admin）', () => {
    expect(deriveRoleFromPerms(['ops.restart'])).toBe('operator');
  });

  it('含 device.write 或 point.write → engineer', () => {
    expect(deriveRoleFromPerms(['device.write'])).toBe('engineer');
    expect(deriveRoleFromPerms(['point.write', 'device.view'])).toBe('engineer');
  });

  it('含 ops.collectors / ops.restart → operator', () => {
    expect(deriveRoleFromPerms(['ops.collectors'])).toBe('operator');
    expect(deriveRoleFromPerms(['ops.logs_read', 'ops.restart'])).toBe('operator');
  });

  it('只有 *.view / 空集 → viewer', () => {
    expect(deriveRoleFromPerms(['device.view', 'audit.view', 'account.view'])).toBe('viewer');
    expect(deriveRoleFromPerms([], undefined)).toBe('viewer');
  });

  it('精确成员判断：只读权限不会被模糊匹配误判为写权限', () => {
    // `ops.logs_read` 含 "read" 而非写；`audit.export` 是导出（只读语义）
    expect(deriveRoleFromPerms(['ops.logs_read'])).toBe('viewer');
    expect(deriveRoleFromPerms(['audit.export'])).toBe('viewer');
    // 非法/未知 id 一律不提升
    expect(deriveRoleFromPerms(['device', 'write', 'device.writing'])).toBe('viewer');
  });

  it('厂商侧角色名不再影响派生（两端角色模型不映射）', () => {
    // 即便 backendRole 是厂商侧运营档，只要权限集是只读，仍是 viewer
    expect(deriveRoleFromPerms(['device.view'], 'ops')).toBe('viewer');
    expect(deriveRoleFromPerms(['device.view'], 'lic_ops')).toBe('viewer');
    expect(deriveRoleFromPerms(['device.view'], 'risk')).toBe('viewer');
  });
});

describe('parseJwtPerms —— 只读解析 JWT payload 的 perms', () => {
  it('解析出权限 id 数组', () => {
    const token = makeToken({ sub: 'root', role: 'system', perms: ['device.write', 'point.write'] });
    expect(parseJwtPerms(token)).toEqual(['device.write', 'point.write']);
  });

  it('无 perms claim → null（走兜底映射）', () => {
    expect(parseJwtPerms(makeToken({ sub: 'root', role: 'system' }))).toBeNull();
  });

  it('非法 token / 空串 → null', () => {
    expect(parseJwtPerms('')).toBeNull();
    expect(parseJwtPerms('not-a-jwt')).toBeNull();
    expect(parseJwtPerms('aaa.!!!not-base64!!!.ccc')).toBeNull();
  });

  it('过滤非字符串项', () => {
    const token = makeToken({ perms: ['device.write', 42, null, 'point.write'] });
    expect(parseJwtPerms(token)).toEqual(['device.write', 'point.write']);
  });
});

describe('resolveRoleFromToken —— 首选 perms 派生、兜底角色映射', () => {
  it('有 perms：按权限集派生（与 backendRole 无关）', () => {
    const token = makeToken({ perms: ['device.write'] });
    expect(resolveRoleFromToken(token, 'ops')).toBe('engineer');
  });

  it('admin 档：account.update', () => {
    const token = makeToken({ perms: ['account.update'] });
    expect(resolveRoleFromToken(token, 'root')).toBe('admin');
  });

  it('无 perms：回落 mapBackendRole（system → admin）', () => {
    const token = makeToken({ sub: 'root', role: 'system' });
    expect(resolveRoleFromToken(token, 'system')).toBe('admin');
  });

  it('无 perms 且非 system：回落 viewer', () => {
    const token = makeToken({ sub: 'x', role: 'ops' });
    expect(resolveRoleFromToken(token, 'ops')).toBe('viewer');
  });

  it('token 非法：回落 mapBackendRole', () => {
    expect(resolveRoleFromToken('broken.token.value', 'system')).toBe('admin');
    expect(resolveRoleFromToken('', 'ops')).toBe('viewer');
  });

  it('纯 view 权限集 → viewer（只读场景，backendRole 非 system）', () => {
    const token = makeToken({ perms: ['device.view', 'audit.view', 'account.view'] });
    expect(resolveRoleFromToken(token, 'ops')).toBe('viewer');
  });
});
