/**
 * @file repo.contract.test.ts
 * @module web-console/api/repo.contract.test
 * @description 数据层真实契约测试（**演示模式已废除**后的回归护栏）。
 *
 * 断言重点：
 *  1. `API_MODE` 恒为 `'real'`；
 *  2. 未加载 / 请求失败时读取型方法返回**诚实空态**（绝无演示数据）；
 *  3. `notices` 覆盖全部真实数据源键；
 *  4. 后端无对应端点时写操作返回**结构化失败**（`ok:false`），绝不假装成功。
 */
import { describe, it, expect, vi } from 'vitest';
import { API_MODE, repo, refresh, refreshAlerts, coerceRuleConditionValue } from './repo';
import { DEFAULT_ACTOR, PROTOCOL_OPTIONS, licenseSnapshot } from './model';

const NOTICE_KEYS = [
  'devices',
  'points',
  'forwarders',
  'audit',
  'alerts',
  'rules',
  'license',
  'groups',
  'roles',
  'permissions',
  'accounts',
] as const;

describe('repo 真实契约', () => {
  it('API_MODE 恒为 real', () => {
    expect(API_MODE).toBe('real');
  });

  it('未加载任何真实数据时，读取型方法返回诚实空态（无演示数据）', () => {
    expect(repo.allDevices()).toEqual([]);
    expect(repo.allPoints()).toEqual([]);
    expect(repo.allForwarders()).toEqual([]);
    expect(repo.allRules()).toEqual([]);
    expect(repo.allAlarms()).toEqual([]);
    expect(repo.queryAudit({ actorType: '', actor: '', action: '', entityType: '', result: '', from: '', to: '', page: 1, pageSize: 10 }).total).toBe(0);
  });

  it('getLicense 在无真实快照时返回诚实空值基线（status=stopped）', () => {
    expect(repo.getLicense().status).toBe('stopped');
    expect(licenseSnapshot.status).toBe('stopped');
  });

  it('notices 覆盖全部真实数据源键', () => {
    const notices = repo.actions.notices();
    for (const key of NOTICE_KEYS) {
      expect(notices).toHaveProperty(key);
    }
  });

  it('resolveAlarm 走 POST /api/alerts/:id/ack：四要素下发，confirm 漏传归一为告警 id（fail-closed）', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(JSON.stringify({ ok: true, alarm: { id: 'al-1' } }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      const result = await repo.resolveAlarm({
        id: 'al-1',
        state: 'resolved',
        note: '现场已复位',
        actor: DEFAULT_ACTOR,
        reason: '误报确认',
      });
      expect(result.ok).toBe(true);
    } finally {
      spy.mockRestore();
    }

    const posts = calls.filter((c) => c.init?.method === 'POST');
    expect(posts).toHaveLength(1);
    expect(posts[0].url).toBe('/api/alerts/al-1/ack');
    const body = JSON.parse(String(posts[0].init?.body)) as Record<string, unknown>;
    // AckBody 四要素：state / note / reason / confirm 各自独立；reason 与 note 不拼接。
    expect(body).toMatchObject({ state: 'resolved', note: '现场已复位', reason: '误报确认' });
    // 后端 fail-closed：confirm 必须 = 告警 id 原文；调用方漏传 → repo 归一为 id。
    expect(body['confirm']).toBe('al-1');
  });

  it('resolveAlarm 默认成功后重取清单并 bump dataVersion；refetch:false 则不跟随 GET', async () => {
    const urls: string[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (init?.method === 'POST') {
        urls.push(url);
      } else if (url.startsWith('/api/alerts')) {
        urls.push(url);
      }
      return Promise.resolve(
        new Response(JSON.stringify({ ok: true, alarm: { id: 'al-2' } }), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      // 默认：POST ack + 跟随一次 GET /api/alerts（重取 + bump → watch(dataVersion) 触发）。
      const r1 = await repo.resolveAlarm({ id: 'al-2', state: 'acking', note: '开始处置', actor: DEFAULT_ACTOR });
      expect(r1.ok).toBe(true);
      expect(urls.filter((u) => u.startsWith('/api/alerts'))).toEqual(['/api/alerts/al-2/ack', '/api/alerts']);

      // 批量场景：refetch:false → 只有 POST，不跟随清单 GET（由 refreshAlerts 统一收口）。
      urls.length = 0;
      const r2 = await repo.resolveAlarm({ id: 'al-3', state: 'resolved', note: '批量处置', actor: DEFAULT_ACTOR }, { refetch: false });
      expect(r2.ok).toBe(true);
      expect(urls).toEqual(['/api/alerts/al-3/ack']);

      // 统一重取一次：恰好 1 次 GET /api/alerts。
      await refreshAlerts();
      expect(urls.filter((u) => u === '/api/alerts')).toHaveLength(1);
    } finally {
      spy.mockRestore();
    }
  });

  it('createPoint 透传 endpoint（V1 正例键）；缺省不带上（address 别名语义不变）', async () => {
    const posts: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (init?.method === 'POST') {
        posts.push({ url, init });
      }
      const body: unknown = url.startsWith('/api/devices') ? [{ id: 'dev-1', name: '设备1', protocol: 'modbus-tcp' }] : [];
      return Promise.resolve(
        new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } }),
      );
    }) as typeof fetch);

    try {
      // 先让 realCache.devices 就绪（pointBody 需要设备协议）。
      await refresh();
      posts.length = 0;
      await repo.createPoint({
        deviceId: 'dev-1',
        name: '温度',
        pointType: 'physical',
        address: 'DB1.0',
        endpoint: '192.168.10.31:502',
        dataType: 'float32',
        byteOrder: 'AB CD',
        unit: '℃',
        deadband: 0,
        targetKey: 'temp',
        actor: 'tester',
      });
      await repo.createPoint({
        deviceId: 'dev-1',
        name: '压力',
        pointType: 'physical',
        address: 'DB1.2',
        dataType: 'float32',
        byteOrder: 'AB CD',
        unit: 'kPa',
        deadband: 0,
        targetKey: 'press',
        actor: 'tester',
      });
    } finally {
      spy.mockRestore();
    }

    const pointPosts = posts.filter((c) => c.url === '/api/points');
    expect(pointPosts).toHaveLength(2);
    const withEndpoint = JSON.parse(String(pointPosts[0].init?.body)) as Record<string, unknown>;
    expect(withEndpoint['endpoint']).toBe('192.168.10.31:502'); // V1 正例键透传
    const withoutEndpoint = JSON.parse(String(pointPosts[1].init?.body)) as Record<string, unknown>;
    expect('endpoint' in withoutEndpoint).toBe(false); // 缺省不带上
    expect(withoutEndpoint['address']).toBe('DB1.2'); // address 别名语义不变
  });

  it('常量照旧导出（页面导入面保持兼容）', () => {
    expect(PROTOCOL_OPTIONS.length).toBeGreaterThan(0);
    expect(DEFAULT_ACTOR).toBeTruthy();
  });

  it('危险写操作把 reason / confirm 原样下发到请求 body（供后端写审计）', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((
      input: RequestInfo | URL,
      init?: RequestInit,
    ) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(JSON.stringify({ error: 'not_implemented' }), {
          status: 404,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      // 后端当前 404 → 返回 ok:false；这里只断言请求体忠实携带 reason/note/confirm。
      await repo.roles.remove({
        id: 'role-x',
        actor: 'tester',
        reason: '与其它角色重复',
        note: '与 rule-03 条件完全重复，合并处理',
        confirm: 'role-x',
      });
      await repo.accounts.remove({
        account: 'bob',
        actor: 'tester',
        reason: '人员离职',
        note: '已完成工作交接',
        confirm: 'bob',
      });
      await repo.groups.remove({
        id: 'g1',
        actor: 'tester',
        reason: '分组废弃',
        note: '所属业务线已下线',
        confirm: 'g1',
      });
      await repo.accounts.resetPassword({
        account: 'bob',
        newPassword: 'p@ss',
        actor: 'tester',
        reason: '口令疑似泄露',
        note: '运维要求强制重置并通知本人',
      });
    } finally {
      spy.mockRestore();
    }

    expect(calls).toHaveLength(4);
    const bodies = calls.map((c) => JSON.parse(String(c.init?.body)) as Record<string, unknown>);
    // reason / note 两个**独立字段**，禁止拼接；confirm：角色/账号 = 全名回显。
    expect(bodies[0]).toMatchObject({
      id: 'role-x',
      reason: '与其它角色重复',
      note: '与 rule-03 条件完全重复，合并处理',
      confirm: 'role-x',
    });
    expect(bodies[1]).toMatchObject({
      account: 'bob',
      reason: '人员离职',
      note: '已完成工作交接',
      confirm: 'bob',
    });
    // ⚠️ 分组删除例外：confirm 恒被 repo 归一为**布尔 true**（后端 groups.rs 只认
    // Some(true)），调用方传入的字符串 'g1' 不会到达 wire。
    expect(bodies[2]).toMatchObject({ id: 'g1', reason: '分组废弃', note: '所属业务线已下线' });
    expect(bodies[2]['confirm']).toBe(true);
    expect(bodies[3]).toMatchObject({ reason: '口令疑似泄露', note: '运维要求强制重置并通知本人', password: 'p@ss' });
  });

  it('groups.remove 调用方漏传 confirm 时仍归一为布尔 true（DevicesPage 真机 400 回归）', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(JSON.stringify({ accepted: true }), { status: 200, headers: { 'Content-Type': 'application/json' } }),
      );
    }) as typeof fetch);

    try {
      // 页面现网调用形状（repo.ts 收口前正是这个形状 → 后端 400 confirm）。
      await repo.groups.remove({ id: 'g-1790381861533', actor: 'tester' });
    } finally {
      spy.mockRestore();
    }

    // remove 成功后会跟随一次 loadGroups()（GET /api/groups）刷新，过滤出 DELETE 本体。
    const deletes = calls.filter((c) => c.init?.method === 'DELETE');
    expect(deletes).toHaveLength(1);
    expect(deletes[0].url).toBe('/api/groups/g-1790381861533');
    const body = JSON.parse(String(deletes[0].init?.body)) as Record<string, unknown>;
    expect(body['confirm']).toBe(true); // 布尔 true，非字符串、非缺失
    expect(body['id']).toBe('g-1790381861533');
  });

  it('checkUpdates / autostartStatus：snake → camel 透传，write_supported:false 与 registered:null 诚实保留', async () => {
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL) => {
      const url = String(input);
      const body: unknown =
        url.startsWith('/api/updates/check')
          ? {
              check_supported: false,
              current_version: '0.1.0',
              update_available: false,
              available_version: null, // 未配置升级源 → null
              source: 'unconfigured',
              reason: 'no update source configured',
            }
          : {
              supported: true,
              registered: null, // 非 Windows / 未查询 → null
              command: null,
              source: 'registry-hkcu-run',
              query_error: null,
              write_supported: false, // 后端诚实声明，repo 不得丢弃
              write_reason: 'autostart registration write is not implemented yet',
            };
      return Promise.resolve(
        new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } }),
      );
    }) as typeof fetch);

    try {
      const updates = await repo.ops.checkUpdates();
      expect(updates).toEqual({
        checkSupported: false,
        currentVersion: '0.1.0',
        updateAvailable: false,
        availableVersion: '', // null → 诚实空串
        source: 'unconfigured',
        reason: 'no update source configured',
      });

      const autostart = await repo.ops.autostartStatus();
      expect(autostart.supported).toBe(true);
      expect(autostart.registered).toBeNull(); // null 原样保留
      expect(autostart.command).toBe('');
      expect(autostart.writeSupported).toBe(false); // 绝不冒充可写
      expect(autostart.writeReason).toBe('autostart registration write is not implemented yet');
    } finally {
      spy.mockRestore();
    }
  });

  it('putBackupPolicy：白名单 snake body + reason/note 独立下发，计数字符串透传', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(
          JSON.stringify({ auto_before_write: true, retention_count: '30', interval_min: '60', source: 'config' }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
      );
    }) as typeof fetch);

    try {
      const result = await repo.settings.putBackupPolicy({
        retentionCount: '30',
        intervalMin: '60',
        reason: '延长保留份数',
        note: '产线要求保留近 30 次配置备份',
      });
      expect(result.ok).toBe(true);
      expect(result.policy).toMatchObject({ retentionCount: '30', intervalMin: '60' });
    } finally {
      spy.mockRestore();
    }

    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('/api/settings/backup-policy');
    const body = JSON.parse(String(calls[0].init?.body)) as Record<string, unknown>;
    // 大数红线：retention_count / interval_min 为字符串透传，非 number。
    expect(body).toMatchObject({
      retention_count: '30',
      interval_min: '60',
      reason: '延长保留份数',
      note: '产线要求保留近 30 次配置备份',
    });
    expect(body['auto_before_write']).toBeUndefined(); // 未改动的字段不下发
  });

  it('groups.create / roles.create 同样下发 reason / note', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((
      input: RequestInfo | URL,
      init?: RequestInit,
    ) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(JSON.stringify({ error: 'not_implemented' }), {
          status: 404,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      await repo.groups.create({ name: '产线A', actor: 'tester', reason: '新产线接入', note: '产线A 三台设备并入' });
      await repo.roles.create({
        name: '值班员',
        permissions: ['device.view'],
        actor: 'tester',
        reason: '新增值班岗位',
        note: '仅只读权限',
      });
    } finally {
      spy.mockRestore();
    }

    expect(calls).toHaveLength(2);
    const bodies = calls.map((c) => JSON.parse(String(c.init?.body)) as Record<string, unknown>);
    expect(bodies[0]).toMatchObject({ name: '产线A', reason: '新产线接入', note: '产线A 三台设备并入' });
    expect(bodies[1]).toMatchObject({ name: '值班员', reason: '新增值班岗位', note: '仅只读权限' });
  });

  it('mapPoint 推送开关缺省「开」：后端不上报 push_enabled 时为 true，显式 false 仍为关', async () => {
    const DEVICE = { id: 'dev-1', name: '设备1' };
    // 需求 6：点位默认参与北向推送 → 后端缺失该字段视为「开」。
    const POINT_DEFAULT = { device_id: 'dev-1', point_id: 'p-default', name: '缺省点' };
    // 后端显式回 false → 仍为关闭（不得被缺省值覆盖）。
    const POINT_OFF = { device_id: 'dev-1', point_id: 'p-off', name: '关闭点', push_enabled: false };

    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((
      input: RequestInfo | URL,
    ) => {
      const url = String(input);
      let body: unknown = [];
      if (url.startsWith('/api/devices')) {
        body = [DEVICE];
      } else if (url.startsWith('/api/points')) {
        body = [POINT_DEFAULT, POINT_OFF];
      }
      return Promise.resolve(
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      await refresh();
    } finally {
      spy.mockRestore();
    }

    const byId = new Map(repo.allPoints().map((p) => [p.id, p]));
    expect(byId.get('p-default')?.pushEnabled).toBe(true); // 后端未上报 → 缺省开
    expect(byId.get('p-off')?.pushEnabled).toBe(false); // 显式 false → 仍为关
  });

  it('mapDevice 对齐 BE-1 新字段：status 三态 / last_sample_at 空串→— / 数值与字符串字段各就其位', async () => {
    const DEVICE_ROW = {
      id: 'dev-a',
      name: '设备A',
      protocol: 'modbus-tcp',
      status: 'online',
      last_sample_at: '', // 未采过 → 空串（不得被当成 1970）
      success_rate: 87.5, // number
      fail_streak: 0, // number
      point_count: '2', // 字符串（大数红线：不 parseInt 丢精度）
      group_id: 'default',
      poll_interval_ms: '1000', // 字符串
    };

    const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(((
      input: RequestInfo | URL,
    ) => {
      const url = String(input);
      const body: unknown = url.startsWith('/api/devices') ? [DEVICE_ROW] : [];
      return Promise.resolve(
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);

    try {
      await refresh();
    } finally {
      spy.mockRestore();
    }

    const dev = repo.allDevices().find((d) => d.id === 'dev-a');
    expect(dev).toBeTruthy();
    expect(dev?.status).toBe('online'); // 三态原样
    expect(dev?.lastSampleAt).toBe('—'); // 空串 → 诚实占位，不臆造时间
    expect(dev?.successRate).toBe(87.5); // number 直通
    expect(dev?.failStreak).toBe(0);
    expect(dev?.pointCount).toBe(2); // 字符串计数 → 小值 number（Number 转换，非 parseInt）
    expect(dev?.groupId).toBe('default'); // snake → camel
    expect(dev?.intervalMs).toBe(1000); // poll_interval_ms 字符串 → number
  });
});

describe('转发规则结构化契约（/api/rules 系列；写请求不得打告警端点）', () => {
  /** 拦 fetch 并让所有请求 404（后端写接口未落地 → 结构化失败，不假装成功）。 */
  function spyFetch(calls: { url: string; init?: RequestInit }[]) {
    return vi.spyOn(globalThis, 'fetch').mockImplementation(((input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      return Promise.resolve(
        new Response(JSON.stringify({ error: 'not_implemented' }), {
          status: 404,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    }) as typeof fetch);
  }

  it('rules.create 下发结构化 when 对象（非字符串）与 publish 动作；reason/note/confirm 三字段独立', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = spyFetch(calls);
    let result: { ok: boolean; message: string } | null = null;
    try {
      result = await repo.rules.create({
        name: '高温告警',
        forwarderId: 'fw-1',
        enabled: true,
        priority: 1,
        when: { kind: 'and', conditions: [{ kind: 'cmp', field: 'value', op: 'gt', value: 30 }] },
        actions: [{ kind: 'publish', topic: 'alarms' }],
        select: ['value'],
        reason: '高温告警需求',
        note: 'value 超过 30 时转发到 alarms 主题',
        confirm: '高温告警',
      });
    } finally {
      spy.mockRestore();
    }
    expect(result?.ok).toBe(false); // 后端未落地 → 诚实结构化失败
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('/api/rules');
    expect(calls[0].init?.method).toBe('POST');
    const body = JSON.parse(String(calls[0].init?.body)) as Record<string, unknown>;
    // when 必须是结构化对象，不是字符串
    expect(typeof body['when']).toBe('object');
    expect(body['when']).toEqual({
      kind: 'and',
      conditions: [{ kind: 'cmp', field: 'value', op: 'gt', value: 30 }],
    });
    expect(Array.isArray(body['actions'])).toBe(true);
    expect((body['actions'] as { kind: string }[])[0].kind).toBe('publish');
    expect((body['actions'] as { topic: string }[])[0].topic).toBe('alarms');
    // snake_case wire 字段
    expect(body['forwarder_id']).toBe('fw-1');
    // reason / note / confirm 三个**独立字段**（禁止拼接）
    expect(body['reason']).toBe('高温告警需求');
    expect(body['note']).toBe('value 超过 30 时转发到 alarms 主题');
    expect(body['confirm']).toBe('高温告警');
  });

  it('条件值大数红线：大整数 / 1e30 / 非数值一律字符串，安全整数与小数为 number', () => {
    expect(coerceRuleConditionValue('9007199254740993')).toBe('9007199254740993'); // 2^53+1 → 字符串
    expect(coerceRuleConditionValue('-9007199254740993')).toBe('-9007199254740993');
    expect(coerceRuleConditionValue('30')).toBe(30);
    expect(coerceRuleConditionValue('-2.5')).toBe(-2.5);
    expect(coerceRuleConditionValue('0')).toBe(0);
    expect(coerceRuleConditionValue('1e30')).toBe('1e30'); // 不硬转科学计数
    expect(coerceRuleConditionValue('0x10')).toBe('0x10');
    expect(coerceRuleConditionValue('dev-01')).toBe('dev-01');
    expect(coerceRuleConditionValue('')).toBe('');
  });

  it('条件值判型贯通到 wire：大整数下发字符串、安全整数下发 number', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = spyFetch(calls);
    try {
      await repo.rules.create({
        name: '判型贯通',
        when: {
          kind: 'and',
          conditions: [
            { kind: 'cmp', field: 'value', op: 'gt', value: coerceRuleConditionValue('30') },
            { kind: 'cmp', field: 'serial', op: 'eq', value: coerceRuleConditionValue('9007199254740993') },
          ],
        },
        actions: [{ kind: 'publish', topic: 'alarms' }],
        reason: '判型回归',
        note: '大整数按字符串、安全整数按数字下发',
        confirm: '判型贯通',
      });
    } finally {
      spy.mockRestore();
    }
    const body = JSON.parse(String(calls[0].init?.body)) as {
      when: { conditions: { value: number | string }[] };
    };
    expect(body.when.conditions[0].value).toBe(30); // 安全整数 → JSON number
    expect(body.when.conditions[1].value).toBe('9007199254740993'); // 大整数 → JSON string
  });

  it('rules.setEnabled 走 PUT /api/rules/:id（不再误用 /api/alerts/rules），reason/note/confirm 独立下发', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = spyFetch(calls);
    try {
      await repo.setRuleEnabled({ id: 'rule-1', enabled: false, reason: '停用转发规则', note: '列表开关切换', confirm: 'rule-1' });
    } finally {
      spy.mockRestore();
    }
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('/api/rules/rule-1');
    expect(calls[0].init?.method).toBe('PUT');
    const body = JSON.parse(String(calls[0].init?.body)) as Record<string, unknown>;
    expect(body['enabled']).toBe(false);
    expect(body['reason']).toBe('停用转发规则');
    expect(body['note']).toBe('列表开关切换');
    expect(body['confirm']).toBe('rule-1');
  });

  it('rules.remove 走 DELETE /api/rules/:id，body 携带 reason/note/confirm', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const spy = spyFetch(calls);
    try {
      await repo.rules.remove({ id: 'rule-9', reason: '调试清理', note: '调试期临时规则，验证完即删', confirm: 'rule-9' });
    } finally {
      spy.mockRestore();
    }
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('/api/rules/rule-9');
    expect(calls[0].init?.method).toBe('DELETE');
    const body = JSON.parse(String(calls[0].init?.body)) as Record<string, unknown>;
    expect(body['reason']).toBe('调试清理');
    expect(body['note']).toBe('调试期临时规则，验证完即删');
    expect(body['confirm']).toBe('rule-9');
  });
});
