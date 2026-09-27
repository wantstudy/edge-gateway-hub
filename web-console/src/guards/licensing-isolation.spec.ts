/**
 * @file licensing-isolation.spec.ts
 * @module web-console/guards
 * @description 防回流守护：**网关侧源码不得引用授权端（licensing）RBAC 资产**。
 *
 * ── 背景 ──────────────────────────────────────────────────────────────────────
 * `ui-kit/src/rbac.ts` 同时定义了两类互不映射的权限矩阵：
 *   · **厂商/授权端**（`RBAC_SIDE = 'licensing'`）：`ROLES` / `ROLE_META` / `PAGES` /
 *     `ACTION_MATRIX` / `canSeePage` / `can` / `firstAllowedPage` 及类型
 *     `Role` / `PageId` / `Action` / `RoleMeta` / `PageMeta`；页面含「激活码管理 /
 *     租户与策略 / 换机工单 / 签名密钥管理」等**授权端专属**菜单。
 *   · **网关侧**：另有独立模型（daemon `rbac::PermissionScope` + 本应用
 *     `store/session.ts` 自有的 `ROLES` / `ROLE_META` / `Role`）。
 *
 * 用户曾报告「客户端权限矩阵里出现激活码发放这类**授权端**菜单」——根因即
 * web-console 误 import 了授权端的 `PAGES` / `ACTION_MATRIX`。本用例即为拦住此回流。
 *
 * ── 同名 id 的边界（**不得**误伤）─────────────────────────────────────────────
 * `rbac.ts` 顶部契约与 `store/session.ts` 均声明：两端存在**同名但语义不同**的 id，
 * 例如 `device.view` / `account.view` / `audit.view` / `audit.export` / `account.update`
 * 都是**网关侧合法的** 10 项权限，`store/session.ts` 正在真实消费它们。
 * 因此本黑名单**只收授权端独占**的 id，绝不包含 `device.*` / `account.*` / `audit.*`
 * 这几个双端同名族（已逐一核对为网关侧在用）。
 */
import { describe, expect, it } from 'vitest';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** 被扫描的源码根（本文件位于 `src/guards/`，向上一级即 `src/`）。 */
const SRC_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/**
 * 授权端（licensing）**独占**导出符号：网关侧一旦从 ui-kit 导入即为回流。
 *
 * 逐条来源（`ui-kit/src/rbac.ts` 的 `export` 语句 与 `ui-kit/src/index.ts:46-55`
 * 的再导出清单）：
 *   RBAC_SIDE(53) ROLES(56) Role(59) PageId(62) Action(77) RoleMeta(99)
 *   ROLE_META(110) PageMeta(138) PAGES(154) ACTION_MATRIX(255)
 *   canSeePage(313) can(329) firstAllowedPage(338)
 */
const LICENSING_ONLY_SYMBOLS: readonly string[] = [
  'RBAC_SIDE',
  'ROLES',
  'ROLE_META',
  'PAGES',
  'ACTION_MATRIX',
  'canSeePage',
  'can',
  'firstAllowedPage',
  'Role',
  'PageId',
  'Action',
  'RoleMeta',
  'PageMeta',
];

/**
 * 授权端独占**页面 id**（取自 `rbac.ts` 的 `PageId` 联合类型）。
 * 已排除 `overview` / `login` / `devices` / `audit` / `accounts` —— 这些是双端通用名，
 * 在本应用里作为**路由名**合法出现（`router.ts` / `App.vue`）。
 */
const LICENSING_ONLY_PAGE_IDS: readonly string[] = [
  'codes',
  'code-detail',
  'tenants',
  'receipts',
  'transfers',
  'keys',
  'device-detail',
];

/**
 * 授权端独占**动作 id**（取自 `rbac.ts` 的 `Action` 联合类型，命名族 `code.*` /
 * `tenant.*` / `receipt.*` / `transfer.*` / `key.*` 与 `device.mark_anomaly`）。
 * 已排除 `device.view` / `device.write` / `account.view` / `account.update` /
 * `audit.view` / `audit.export` —— 依 `rbac.ts:49-51` 与 `store/session.ts:81-83`，
 * 它们是**网关侧合法**权限（`/api/permissions` 的 10 项），不得进黑名单。
 */
const LICENSING_ONLY_ACTION_IDS: readonly string[] = [
  'code.view',
  'code.issue',
  'code.revoke',
  'code.reissue',
  'code.reveal',
  'tenant.view',
  'tenant.policy_update',
  'receipt.view',
  'receipt.mark',
  'transfer.view',
  'transfer.process',
  'key.view',
  'key.rotate',
  'device.mark_anomaly',
];

/** 具名导入声明（含 `type` 前缀与 `default, { … }` 混合形态）。 */
const NAMED_IMPORT_RE =
  /import\s+(?:type\s+)?(?:[A-Za-z_$][\w$]*\s*,\s*)?\{([\s\S]*?)\}\s*from\s*['"]([^'"]+)['"]/g;

/** 判断模块说明符是否指向 ui-kit（`@ui-kit`、`@ui-kit/xxx` 或相对路径含 `ui-kit`）。 */
function isUiKitSpecifier(spec: string): boolean {
  return /^@ui-kit(\/|$)/.test(spec) || spec.includes('ui-kit');
}

/** 第 `index` 个字符所在行号（1 起）。 */
function lineOf(src: string, index: number): number {
  let line = 1;
  for (let i = 0; i < index && i < src.length; i++) {
    if (src[i] === '\n') line++;
  }
  return line;
}

/**
 * 剥离 `//` 行注释与块注释（替换为等长空白，**保留换行**以维持行号），
 * 同时保留字符串/模板字面量内容——这样「注释里提到某个 id」不会误报，
 * 而字符串里的 id 仍能被检出。
 */
function stripComments(src: string): string {
  const out = src.split('');
  type State = 'code' | 'line' | 'block' | 'sq' | 'dq' | 'tpl';
  let state: State = 'code';
  for (let i = 0; i < src.length; i++) {
    const c = src[i];
    const next = src[i + 1];
    if (state === 'code') {
      if (c === '/' && next === '/') {
        state = 'line';
        out[i] = ' ';
        continue;
      }
      if (c === '/' && next === '*') {
        state = 'block';
        out[i] = ' ';
        continue;
      }
      if (c === "'") state = 'sq';
      else if (c === '"') state = 'dq';
      else if (c === '`') state = 'tpl';
      continue;
    }
    if (state === 'line') {
      if (c === '\n') {
        state = 'code';
      } else {
        out[i] = ' ';
      }
      continue;
    }
    if (state === 'block') {
      if (c === '*' && next === '/') {
        out[i] = ' ';
        out[i + 1] = ' ';
        i++;
        state = 'code';
      } else if (c !== '\n') {
        out[i] = ' ';
      }
      continue;
    }
    // 字符串状态：仅处理转义，保持内容原样
    if (c === '\\') {
      i++;
      continue;
    }
    if (
      (state === 'sq' && c === "'") ||
      (state === 'dq' && c === '"') ||
      (state === 'tpl' && c === '`')
    ) {
      state = 'code';
    }
  }
  return out.join('');
}

/** 检出「从 ui-kit 导入授权端专属符号」的违规（返回 `文件:行 符号`）。 */
function findSymbolImportViolations(src: string, file: string): string[] {
  const found: string[] = [];
  NAMED_IMPORT_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = NAMED_IMPORT_RE.exec(src)) !== null) {
    const [, names, spec] = m;
    if (!isUiKitSpecifier(spec)) continue;
    const line = lineOf(src, m.index);
    for (const raw of names.split(',')) {
      const name = raw.trim().replace(/^type\s+/, '').trim();
      if (name && LICENSING_ONLY_SYMBOLS.includes(name)) {
        found.push(`${file}:${line} import { ${name} } from '${spec}'`);
      }
    }
  }
  return found;
}

/** 检出授权端独占页面/动作 id 的字符串字面量（返回 `文件:行 id`）。 */
function findIdLiteralViolations(src: string, file: string): string[] {
  const scanned = stripComments(src);
  const found: string[] = [];
  for (const id of [...LICENSING_ONLY_PAGE_IDS, ...LICENSING_ONLY_ACTION_IDS]) {
    for (const quote of ["'", '"', '`']) {
      const needle = `${quote}${id}${quote}`;
      let idx = scanned.indexOf(needle);
      while (idx !== -1) {
        found.push(`${file}:${lineOf(scanned, idx)} ${needle}`);
        idx = scanned.indexOf(needle, idx + 1);
      }
    }
  }
  return found;
}

/** 递归收集待扫描源码（`*.ts` / `*.vue`，跳过测试文件自身）。 */
function listSourceFiles(root: string): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules') continue;
      out.push(...listSourceFiles(full));
      continue;
    }
    if (!/\.(ts|vue)$/.test(entry.name)) continue;
    if (/\.(spec|test)\.ts$/.test(entry.name)) continue;
    out.push(full);
  }
  return out;
}

const FILES = listSourceFiles(SRC_ROOT);

describe('授权端隔离（licensing isolation）', () => {
  it('扫描面非空且覆盖应用入口（防扫描器失效）', () => {
    console.log(
      `[licensing-isolation] 扫描 ${FILES.length} 个源文件；黑名单 = ` +
        `${LICENSING_ONLY_SYMBOLS.length} 符号 + ${LICENSING_ONLY_PAGE_IDS.length} 页面 id + ` +
        `${LICENSING_ONLY_ACTION_IDS.length} 动作 id`,
    );
    expect(FILES.length).toBeGreaterThan(20);
    expect(FILES.some((f) => f.endsWith(`${path.sep}App.vue`))).toBe(true);
    expect(FILES.some((f) => f.endsWith(`${path.sep}main.ts`))).toBe(true);
  });

  it('web-console 源码未从 ui-kit 导入授权端专属符号', () => {
    const violations = FILES.flatMap((f) =>
      findSymbolImportViolations(fs.readFileSync(f, 'utf8'), f),
    );
    expect(violations, `发现授权端符号回流:\n${violations.join('\n')}`).toEqual([]);
  });

  it('web-console 源码不含授权端专属页面/动作 id 字面量', () => {
    const violations = FILES.flatMap((f) =>
      findIdLiteralViolations(fs.readFileSync(f, 'utf8'), f),
    );
    expect(violations, `发现授权端 id 回流:\n${violations.join('\n')}`).toEqual([]);
  });

  // ---------------------------------------------------------------------------
  // 对照实验：证明匹配器真的会命中（非恒真），且不误伤网关侧同名 id
  // ---------------------------------------------------------------------------

  it('对照实验（阳性）：合成样本必须被判违规', () => {
    const synthetic = [
      "import { EmptyState, PAGES, ACTION_MATRIX, RBAC_SIDE } from '@ui-kit';",
      "import { canSeePage } from '@ui-kit/rbac';",
      "const page = 'receipts';",
      'const act = "transfer.process";',
      'const chained = `key.rotate`;',
    ].join('\n');

    const symbols = findSymbolImportViolations(synthetic, '<synthetic>');
    const ids = findIdLiteralViolations(synthetic, '<synthetic>');

    expect(symbols.length).toBeGreaterThanOrEqual(4);
    for (const name of ['PAGES', 'ACTION_MATRIX', 'RBAC_SIDE', 'canSeePage']) {
      expect(symbols.join('\n'), `未命中 ${name}`).toContain(name);
    }
    for (const id of ['receipts', 'transfer.process', 'key.rotate']) {
      expect(ids.join('\n'), `未命中 ${id}`).toContain(id);
    }
    // 失败信息必须可定位到行
    expect(symbols[0]).toMatch(/^<synthetic>:\d+ /);
    expect(ids[0]).toMatch(/^<synthetic>:\d+ /);
  });

  it('对照实验（阴性）：合法样本与注释零误报', () => {
    const benign = [
      "import { EmptyState, RoleGate, UiTable } from '@ui-kit';",
      "import { themeMode } from '@ui-kit/theme';",
      "import '@ui-kit/tokens.css';",
      "const perm = 'device.view';", // 网关侧同名权限，不得进黑名单
      "const perm2 = 'account.update';",
      "const perm3 = 'audit.export';",
      "const role = 'system';",
      "// 文档注释里提到 receipt.view / codes 也不算回流",
      '/* transfer.process 亦然 */',
    ].join('\n');

    expect(findSymbolImportViolations(benign, '<synthetic-benign>')).toEqual([]);
    expect(findIdLiteralViolations(benign, '<synthetic-benign>')).toEqual([]);
  });
});
