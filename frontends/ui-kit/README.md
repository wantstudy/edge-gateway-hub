# ui-kit — 两端共用前端基础包

网关客户端（`web-console/`）与厂商总管理后台（`admin-console/`）共用的设计 token 与业务组件包。
权威设计来源：`docs/design/ui-design-system.md`（组件契约）、`docs/design/ui-admin-console.md`（管理端页面）。

约束：**Light 主题、无 i18n、目标分辨率 ≥1366×768**；危险操作一律走 `DangerConfirmModal`（原因必填 + 草稿隔离）。

---

## 1. 这个包提供什么

**设计 token（唯一真源）**

| 文件 | 用途 |
| --- | --- |
| `src/tokens.ts` | 供 TS 逻辑消费的常量（`BRAND` / `NEUTRAL` / `SEMANTIC` / `SEMANTIC_SURFACE` / `SCALE` / `Tone`） |
| `src/tokens.css` | 供 CSS 消费的 CSS 变量（与 `tokens.ts` 一一对应，改一处必须同步另一处） |

**纯逻辑模块**

| 文件 | 用途 |
| --- | --- |
| `src/rbac.ts` | 页面级 + 操作级权限矩阵（**唯一真源**），含 `canSeePage()` / `can()` / `firstAllowedPage()` |
| `src/status-map.ts` | 状态码 → `{ label, tone }` 映射，`statusView()` 派生视图 |
| `src/mask.ts` | 码掩码 / 机器码格式化 / 时间格式化等纯函数（如 `maskCode()`、`codeTail8()`） |
| `src/icons.ts` | 内联 SVG 图标（避免为图标引入重依赖） |

**组件（`src/components/`）**
`StatusTag`、`StatCard`、`PageHeader`、`EmptyState`、`MachineCodeDisplay`、`MaskedCode`、
`CodeLifecycleTimeline`、`DangerConfirmModal`、`RoleGate`、`UiField`、`UiInput`、`UiSelect`、
`UiTextarea`、`UiSwitch`、`UiRadio`、`UiTable`、`UiPager`。

所有导出统一从包入口 `src/index.ts` re-export —— **消费方只写 `import { Xxx } from '@ui-kit'`**，不要深入 `@ui-kit/src/...` 子路径。

---

## 2. 如何在应用里消费 `@ui-kit`（关键：别名要配两处）

`@ui-kit` 不是从 npm 拉取的发布包，而是**源码级共享**（消费方直接编译本包的 `.ts` / `.vue`）。
因此 **Vite 与 TypeScript 两边都要配别名** —— 只配一处会「构建通过但类型报错」或反之。**两处必须指向同一目录。**

### 2.1 Vite（编译期解析）—— `vite.config.ts`

```ts
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import { fileURLToPath, URL } from 'node:url';

export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@ui-kit': fileURLToPath(new URL('../ui-kit/src', import.meta.url)),
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
});
```

### 2.2 TypeScript（类型期解析）—— `tsconfig.json`

```jsonc
{
  "compilerOptions": {
    "paths": {
      "@ui-kit": ["../ui-kit/src/index.ts"],
      "@ui-kit/*": ["../ui-kit/src/*"],
      "@/*": ["./src/*"]
    }
  }
}
```

> 校验口诀：`@ui-kit` 在 Vite 指到 `../ui-kit/src`，在 tsconfig 指到 `../ui-kit/src` / `src/index.ts`。
> **改一个目录，两处都得改。**

### 2.3 样式与顺序

入口 `main.ts` 里**先引 token，再引第三方 UI 库**，保证组件样式能覆盖基线：

```ts
import '@ui-kit/src/tokens.css';       // 1) 设计 token（CSS 变量）
import '@arco-design/web-vue/dist/arco.css'; // 2) 第三方基线（如用到）
import './styles/global.css';          // 3) 应用自有样式
```

---

## 3. 重要边界：本包**不强制依赖 Arco Design Vue**

设计基线是 Arco，但为保持 ui-kit **可独立测试、可被任意宿主复用**，本包**不把 Arco 作为运行时依赖**：

- 组件优先用**自研实现 + 原生元素**（`UiInput` / `UiSelect` / `UiTable` … 都是原生封装），
  只在极少数处需要 Arco 的**类型**（如 `rbac.ts` 里 `IconXxx` 的类型来自 `@arco-design/web-vue/es/icon`）。
- Arco 仅作为 **devDependency** 出现在 `ui-kit/package.json` 中，供类型引用。
- **测试**里对 Arco 图标做了 **stub**（见 `tests/stubs/arco-icon.ts`），并在 `vitest.config.ts`
  用别名把 `@arco-design/web-vue/es/icon` 指到该 stub —— 这样即便宿主环境没有 Arco，
  单测也能跑；同时避免为跑测试而引入整包 Arco。

**给宿主的含义**：应用可以自由选用 Arco（或不选用），ui-kit 都能正常工作。

---

## 4. 开发与测试

```bash
cd frontends/ui-kit
npm install
npm run test        # Vitest（jsdom），覆盖 rbac / mask / status-map / DangerConfirmModal
npm run test:watch  # 监听模式
```

测试说明：
- 环境 jsdom；`DangerConfirmModal` 用了 `<Teleport to="body">`，断言需查询 `document.body`
  （各用例在 `beforeEach` 里清空 `document.body`）。
- 新增纯函数（mask / status / rbac）请一并补单测，保持「矩阵即真源」的可回归性。

---

## 5. 约定（Contributing）

- 组件用 Vue 3 `<script setup lang="ts">`；泛型组件（如 `UiTable`）用 `generic="T extends object"`。
- 面向消费者的 public API 只从 `src/index.ts` 导出；内部组件不深链。
- 危险操作**不得**绕过 `DangerConfirmModal` 的四要素（影响清单 / 原因必填 / 对象二次校验 / 双人复核）与草稿隔离。
- 权限判定统一走 `rbac.ts`，**调用方不要另行硬编码角色**（路由守卫 / 菜单 / `RoleGate` 共用同一矩阵，保证不漂移）。
