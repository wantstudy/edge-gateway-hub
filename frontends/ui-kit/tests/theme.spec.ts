/**
 * @file theme.spec.ts
 * @description 明暗双主题运行时（`ui-kit/src/theme.ts`）的单测。
 *
 * 这些断言保护的是「用户点了开关就必须真的变」这条底线：
 * 主题一旦错位，全站会陷入暗底暗字或白块，属于 P0 级观感缺陷。
 *
 * 本模块在 import 时即读取 localStorage / matchMedia 并缓存 computed，
 * 因此每个用例都走 `vi.resetModules()` + 动态 import，确保环境在求值前就绪。
 */
import { describe, it, expect, beforeEach, vi } from 'vitest';
import { nextTick } from 'vue';
import type { ThemeMode } from '../src/theme';

/** 清干净 localStorage / DOM 标记，避免用例间互相污染。 */
function resetDom(): void {
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
  document.body.removeAttribute('arco-theme');
}

/**
 * 在新模块实例下装载主题运行时。
 *
 * @param systemDark 系统是否偏好暗色
 * @param stored     localStorage 中已有的显式选择（未传表示未选择）
 */
async function loadTheme(
  systemDark: boolean,
  stored?: ThemeMode,
): Promise<typeof import('../src/theme')> {
  if (stored) {
    localStorage.setItem('iot-daq.theme', stored);
  }
  vi.resetModules();
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches: query.includes('prefers-color-scheme: dark') ? systemDark : false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }));
  return (await import('../src/theme')) as typeof import('../src/theme');
}

/** 读取当前生效主题标记。 */
function currentFlag(): ThemeMode | null {
  return document.documentElement.getAttribute('data-theme') as ThemeMode | null;
}

describe('themeMode（主题取值优先级）', () => {
  beforeEach(() => {
    resetDom();
    vi.unstubAllGlobals();
  });

  it('回退系统偏好：系统暗色 → dark', async () => {
    const { themeMode } = await loadTheme(true);
    expect(themeMode.value).toBe('dark');
  });

  it('回退系统偏好：系统亮色 → light', async () => {
    const { themeMode } = await loadTheme(false);
    expect(themeMode.value).toBe('light');
  });

  it('localStorage 显式选择优先于系统偏好（切过开关后系统变化不得把页面改回去）', async () => {
    const { themeMode } = await loadTheme(true, 'light');
    expect(themeMode.value).toBe('light');
  });

  it('localStorage 中的非法值按「未选择」处理，回退系统偏好', async () => {
    localStorage.setItem('iot-daq.theme', 'neon');
    vi.resetModules();
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }));
    const { themeMode } = await import('../src/theme');
    expect(themeMode.value).toBe('light');
  });
});

describe('applyTheme / setThemeMode（DOM 落地）', () => {
  beforeEach(() => {
    resetDom();
    vi.unstubAllGlobals();
  });

  it('dark → <html data-theme=dark> 且 <body arco-theme=dark>（Arco 官方令牌，非第二套 token）', async () => {
    const { setThemeMode } = await loadTheme(false);
    setThemeMode('dark');
    await nextTick();
    expect(currentFlag()).toBe('dark');
    expect(document.body.getAttribute('arco-theme')).toBe('dark');
  });

  it('light → 同时摘掉两边的主题标记（干净回亮色，不留残留）', async () => {
    const { setThemeMode } = await loadTheme(false);
    setThemeMode('dark');
    await nextTick();
    setThemeMode('light');
    await nextTick();
    expect(currentFlag()).toBeNull();
    expect(document.body.hasAttribute('arco-theme')).toBe(false);
  });

  it('localStorage 显式选择写入键名 iot-daq.theme，供刷新后恢复', async () => {
    const { setThemeMode, THEME_STORAGE_KEY } = await loadTheme(false);
    setThemeMode('dark');
    await nextTick();
    expect(THEME_STORAGE_KEY).toBe('iot-daq.theme');
    expect(localStorage.getItem('iot-daq.theme')).toBe('dark');
  });

  it('toggleThemeMode 在两态间互斥翻转', async () => {
    const { setThemeMode, toggleThemeMode, themeMode } = await loadTheme(false);
    setThemeMode('light');
    await nextTick();
    toggleThemeMode();
    await nextTick();
    expect(themeMode.value).toBe('dark');
    toggleThemeMode();
    await nextTick();
    expect(themeMode.value).toBe('light');
  });
});
