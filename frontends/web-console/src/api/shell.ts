/**
 * @file web-console/src/api/shell.ts
 * @module web-console/api/shell
 * @description 桌面壳桥（Tauri）。把「守护进程」真实能力封成纯函数，浏览器端诚实降级。
 *
 * 红线：桌面端能力（崩溃重启 / 看门狗 / 启动失败保护）来自 Tauri `invoke`，
 * **绝不在浏览器端伪造成功**。非桌面端 / invoke 失败一律返回 `ok:false` + 真实原因，
 * 由调用方回滚开关、展示真实错误信息。
 */

/** 守护进程真实状态（与 fe-shell 在 Tauri 壳里实现的契约一致）。 */
export interface SupervisorStatus {
  supported: boolean;
  running: boolean;
  restarts: number;
  crash_restart: boolean;
  watchdog: boolean;
  boot_failure_guard: boolean;
  consecutive_failures: number;
  /** 上次退出原因；null = 无记录（界面展示 `—`）。 */
  last_exit: string | null;
  reason: string;
}

/** 守护进程写补丁（仅传要改的字段）。 */
export interface SupervisorPatch {
  crash_restart?: boolean;
  watchdog?: boolean;
  boot_failure_guard?: boolean;
}

/** 写操作结果（含操作后的真实状态，供调用方回写 UI）。 */
export interface SupervisorResult {
  ok: boolean;
  message: string;
  state: SupervisorStatus;
}

/**
 * 当前是否桌面壳（Tauri）环境。
 *
 * 仅以 Tauri 注入的全局对象判定——浏览器（dev / web 部署）为 false，
 * 此时守护能力不可得，开关应禁用而非伪造。
 */
export const shellAvailable: boolean =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** 取真实状态失败时的占位（明确声明桌面端守护不可用，绝不假装 supported）。 */
function unsupportedState(reason: string): SupervisorStatus {
  return {
    supported: false,
    running: false,
    restarts: 0,
    crash_restart: false,
    watchdog: false,
    boot_failure_guard: false,
    consecutive_failures: 0,
    last_exit: null,
    reason,
  };
}

/** tauri invoke 命令名（fe-shell 在壳内实现）。 */
const CMD_STATUS = 'supervisor_status';
const CMD_SET = 'supervisor_set';

/** 动态加载 tauri core（避免在非桌面环境硬依赖）。 */
async function loadInvoke(): Promise<(cmd: string, args?: Record<string, unknown>) => Promise<unknown>> {
  const mod = await import('@tauri-apps/api/core');
  return mod.invoke;
}

/**
 * 读取守护真实状态；非桌面端返回 `null`（调用方据此禁用开关）。
 *
 * invoke 失败时**不抛异常给上层**、也不伪造——返回尽力取到的值或占位，
 * `reason` 写明真实原因，让 UI 呈现诚实空态。
 */
export async function supervisorStatus(): Promise<SupervisorStatus | null> {
  if (!shellAvailable) {
    return null;
  }
  try {
    const invoke = await loadInvoke();
    const raw = (await invoke(CMD_STATUS)) as Partial<SupervisorStatus> | null;
    if (!raw) {
      return unsupportedState('桌面端守护状态不可得');
    }
    return {
      supported: raw.supported ?? false,
      running: raw.running ?? false,
      restarts: raw.restarts ?? 0,
      crash_restart: raw.crash_restart ?? false,
      watchdog: raw.watchdog ?? false,
      boot_failure_guard: raw.boot_failure_guard ?? false,
      consecutive_failures: raw.consecutive_failures ?? 0,
      last_exit: raw.last_exit ?? null,
      reason: raw.reason ?? '',
    };
  } catch (cause) {
    const msg = cause instanceof Error ? cause.message : String(cause);
    return unsupportedState(`读取守护状态失败：${msg}`);
  }
}

/**
 * 写守护补丁；非桌面端 / invoke 失败一律 `ok:false` + 真实原因 + 占位状态。
 *
 * 绝不伪造成功：返回对象里即使 `state.supported` 为 false 也如实写明原因，
 * 由调用方回滚已拨动的开关。
 */
export async function setSupervisor(patch: SupervisorPatch): Promise<SupervisorResult> {
  if (!shellAvailable) {
    return {
      ok: false,
      message: '该能力随桌面端（IoT-DAQ Gateway）提供，当前为浏览器访问，状态不可得。',
      state: unsupportedState('桌面端守护不可用'),
    };
  }
  try {
    const invoke = await loadInvoke();
    const raw = (await invoke(CMD_SET, patch as Record<string, unknown>)) as
      | Partial<SupervisorStatus>
      | null;
    if (!raw) {
      return {
        ok: false,
        message: '桌面端守护未返回有效状态。',
        state: unsupportedState('守护未返回有效状态'),
      };
    }
    return {
      ok: raw.supported === true,
      message: raw.reason ?? (raw.supported === true ? '守护配置已更新。' : '守护配置未生效。'),
      state: {
        supported: raw.supported ?? false,
        running: raw.running ?? false,
        restarts: raw.restarts ?? 0,
        crash_restart: raw.crash_restart ?? false,
        watchdog: raw.watchdog ?? false,
        boot_failure_guard: raw.boot_failure_guard ?? false,
        consecutive_failures: raw.consecutive_failures ?? 0,
        last_exit: raw.last_exit ?? null,
        reason: raw.reason ?? '',
      },
    };
  } catch (cause) {
    const msg = cause instanceof Error ? cause.message : String(cause);
    return {
      ok: false,
      message: `守护配置未变更：${msg}`,
      state: unsupportedState(`写入守护失败：${msg}`),
    };
  }
}

/** 纯函数：把状态映射成人类可读的「守护支持」短标签（不改变真值）。 */
export function describeSupervisorSupport(status: SupervisorStatus | null): string {
  if (!status) {
    return '不可得';
  }
  if (!status.supported) {
    return status.reason ? `不支持：${status.reason}` : '不支持';
  }
  return status.running ? '支持 · 运行中' : '支持 · 未运行';
}
