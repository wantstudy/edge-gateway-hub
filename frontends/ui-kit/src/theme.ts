/**
 * @file theme.ts
 * @module ui-kit/theme
 * @description 明暗双主题的**唯一运行时状态源**（两端共用，禁止各应用自建第二套）。
 *
 * ── 契约 ──────────────────────────────────────────────────────────────────────
 *  · 主题只影响「底色 / 文字 / 语义色」；品牌色、圆角、字体在明暗两态**同值**
 *    （见 `tokens.css` 的 `:root[data-theme='dark']` 注释）。
 *  · 视觉层开关：`<html data-theme="dark">` —— 消费方 CSS 一律用 `var(--…)`。
 *  · 组件库层开关：`<body arco-theme="dark">` —— Arco Design Vue 官方暗色令牌，
 *    由 arco.css 的 `[arco-theme='dark']` 选择器接管，**不另建第二套 token**（红线 1）。
 *  · 持久化：`localStorage['iot-daq.theme']`（写入失败如隐私模式静默降级为内存态）。
 *  · 缺省回退：`prefers-color-scheme: dark`；用户未显式选择时随系统变化实时跟随。
 *
 * 模块被 import 时即完成一次应用（早于 Vue 挂载，避免亮色闪屏），
 * 因此**不要**把 `applyTheme` 的调用放到组件内部。
 */
import { computed, ref, watch } from 'vue';

/** 主题取值。 */
export type ThemeMode = 'light' | 'dark';

/** localStorage 键名（两端共用同一套键，避免同机多应用互相重置）。 */
export const THEME_STORAGE_KEY = 'iot-daq.theme';

/** 系统偏好。 */
export const DARK_QUERY = '(prefers-color-scheme: dark)';

/** 读取 localStorage：未存 / 非法值 → null（走系统回退）。 */
function readStored(): ThemeMode | null {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    return raw === 'light' || raw === 'dark' ? raw : null;
  } catch {
    return null;
  }
}

/** 用户显式选择（null = 跟随系统）。 */
const chosen = ref<ThemeMode | null>(readStored());

/** 读取系统偏好（非浏览器环境 / matchMedia 不可用如单测 → false）。 */
function systemPrefersDark(): boolean {
  try {
    return window.matchMedia(DARK_QUERY).matches;
  } catch {
    return false;
  }
}

/**
 * 当前生效主题：`显式选择 > 系统偏好 > light`。
 *
 * 系统偏好在**每次求值时**实时读取（computed 惰性求值），因此无需再挂
 * `change` 监听：用户未显式选择时，系统主题变化会自动经 `themeMode` 传导到 DOM；
 * 一旦用户点过开关，`chosen` 非空，系统变化不再覆盖用户选择（否则开关看起来失效）。
 */
export const themeMode = computed<ThemeMode>(() => chosen.value ?? (systemPrefersDark() ? 'dark' : 'light'));

/**
 * 应用到 DOM。
 *
 * @param mode 目标主题
 */
export function applyTheme(mode: ThemeMode): void {
  try {
    if (mode === 'dark') {
      document.documentElement.setAttribute('data-theme', 'dark');
      // Arco 官方暗色令牌（arco.css 的 `[arco-theme='dark']`），非第二套 token
      document.body.setAttribute('arco-theme', 'dark');
    } else {
      document.documentElement.removeAttribute('data-theme');
      document.body.removeAttribute('arco-theme');
    }
  } catch {
    /* 非浏览器环境（单测 / SSR）：仅内存态生效 */
  }
}

/** 切换主题（写 localStorage，无显式选择时置为当前反相）。 */
export function setThemeMode(mode: ThemeMode): void {
  chosen.value = mode;
  try {
    localStorage.setItem(THEME_STORAGE_KEY, mode);
  } catch {
    /* 隐私模式：仅内存态，刷新后回退系统偏好（诚实降级，不报错） */
  }
}

/** 切换为与当前相反的主题。 */
export function toggleThemeMode(): void {
  setThemeMode(themeMode.value === 'dark' ? 'light' : 'dark');
}

/** 首次挂载：在 `<html>` / `<body>` 上打标记（早于 Vue 挂载，避免亮色闪屏）。 */
applyTheme(themeMode.value);

/** `themeMode` 变化时落到 DOM（模块 import 时已由上面的 applyTheme 先跑一次）。 */
watch(themeMode, applyTheme);
