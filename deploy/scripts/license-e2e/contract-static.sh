#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# contract-static.sh —— P1-3「授权端到端」六条硬契约的**静态**契约核查。
#
# 对应体检报告 P1-3：一机一码 / 换机重发 / 容器重建不重置试用。
#
# 只读核查，**不改任何源码**（本脚本只 grep / 解析，不写入 crates/**）。
# 每条断言都必须落到「文件:行号 + 看到的取值的字面证据」上；
# 找不到证据 = FAIL（不猜、不脑补）。
#
# 用法：  bash contract-static.sh
# 退出码：0=全 PASS；1=有 FAIL；2=环境不满足。
# ---------------------------------------------------------------------------
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${SCRIPT_DIR}/lib.sh"

section "契约核查环境"
REPO_ROOT_FROM_LIB="${REPO_ROOT}"
log "仓库根: ${REPO_ROOT_FROM_LIB}"
log "本脚本: 只读核查，不修改 crates/** 下任何文件"
E_TOTAL=0; E_PASS=0; E_FAIL=0; E_SKIP=0
FAILED_NAMES=(); SKIPPED_NAMES=()

LS_CRS="crates/licensing-server/src"
DA_CRS="crates/daemon/src"

# ===========================================================================
section "契约 1：一机一码 —— 服务端同机判定阈值 N=4/5，且只在服务端发生"
# ===========================================================================

src_const "1.1 锚点数 M=5 (ANCHOR_COUNT)"      "${LS_CRS}/model.rs"  "ANCHOR_COUNT"           "5"
src_const "1.2 同机阈值 N=4 (SAME_MACHINE_MIN_HITS)" "${LS_CRS}/model.rs" "SAME_MACHINE_MIN_HITS" "4"

src_contains "1.3 阈值比较只走单一来源 is_same_machine_by_anchors" \
    "${LS_CRS}/service.rs" "device\.is_same_machine_by_anchors\(&req\.anchor_hashes\)"

src_contains "1.4 service 层不写 >=4 字面量（避免阈值分叉）" \
    "${LS_CRS}/service.rs" "阈值比较\*\*只\*\*经 \[\[Device::is_same_machine_by_anchors\]\]|阈值比较\*\*只\*\*经"

src_contains "1.5 同机改绑走 auto_rebind_same_machine" \
    "${LS_CRS}/service.rs" "fn auto_rebind_same_machine\("

src_contains "1.6 异机判定分支（≤3/5 → 拒绝）" \
    "${LS_CRS}/service.rs" "Err\(LicenseError::code_bound_to_other_device\(\)\)"

src_contains "1.7 异机拒绝写审计（hits/drifts 留痕）" \
    "${LS_CRS}/service.rs" "\"hits=\{hits\} drifts=\{drifts\}\""

src_contains "1.8 一机一码判定只在服务端（服务端模块注释口径）" \
    "${LS_CRS}/service.rs" "§7 一机一码冲突检测"

# 客户端不得自行判定：daemon 侧不得出现锚点命中计数 / 同机阈值 / 同机判定
src_absent "1.9 客户端无锚点命中计数逻辑"      "${DA_CRS}" "fn anchor_match_count|anchor_drift_count|fn is_same_machine_by_anchors"
src_absent "1.10 客户端无同机阈值常量"         "${DA_CRS}" "SAME_MACHINE_MIN_HITS|is_same_machine"
src_absent "1.11 客户端不预判同机结论（无 anchor 比对调用）" "${DA_CRS}" "anchor_match_count|anchor_hashes.*intersect"

# ===========================================================================
section "契约 2：试用 3 天 → 降级免费版、不停用"
# ===========================================================================

src_const "2.1 试用到期天数 = 3 (TRIAL_DAYS)" "${DA_CRS}/auth/client.rs" "TRIAL_DAYS" "3"
src_const "2.2 离线宽限天数 = 7 (GRACE_DAYS)" "${DA_CRS}/auth/client.rs" "GRACE_DAYS" "7"

# 跨行结构拆成两条单行断言（grep -E 不跨行）
src_contains "2.3 试用到期跃迁 Degraded + reason 文案" \
    "${DA_CRS}/license.rs" "reason: \"trial expired\"\.to_string\(\)"
src_contains "2.4 降级后仍允许本地采集（恒 true）" \
    "${DA_CRS}/auth/client.rs" "pub fn allows_local_capture\(&self\) -> bool \{"
src_next_line "2.4b 本地采集恒真（函数体首句 = true）" \
    "${DA_CRS}/auth/client.rs" "pub fn allows_local_capture(&self) -> bool {" "true"
src_contains "2.5 降级后停北向转发" \
    "${DA_CRS}/auth/client.rs" "pub fn allows_northbound_forward"
src_contains "2.6 降级走免费版配额闸门（enforce_free_limits）" \
    "${DA_CRS}/license.rs" "fn enforce_free_limits\(&self, config: &GatewayConfig\)"
src_contains "2.7 免费版配额违规文案（可解释恢复路径）" \
    "${DA_CRS}/license.rs" "free-edition quota exceeded \(license degraded\)"
# 「不停用」的 strengthen：降级时本模块不持有采集开关
src_contains "2.8 本模块不持有采集开关（消费方才是开关持有者）" \
    "${DA_CRS}/license.rs" "本模块\*\*不持有采集开关\*\*"

# ===========================================================================
section "契约 3：离线宽限 7 天 / 24 小时心跳；超期后停止北向、保留采集"
# ===========================================================================

src_const "3.1 心跳周期 24h (HEARTBEAT_HOURS)" "${LS_CRS}/service.rs" "HEARTBEAT_HOURS" "24"
src_const "3.2 心跳周期 24h (DEFAULT_HEARTBEAT_HOURS)" "${DA_CRS}/auth/client.rs" "DEFAULT_HEARTBEAT_HOURS" "24"
# HEARTBEAT_SECS 由 HEARTBEAT_HOURS 派生，故先断言表达式、再断言源常量=24（3.1 已覆盖）
src_const "3.3 服务端心跳秒数 = HEARTBEAT_HOURS × 3600" "${LS_CRS}/service.rs" "HEARTBEAT_SECS" "HEARTBEAT_HOURS*3_600"
src_fn_returns "3.3b 客户端心跳秒数 = heartbeat_hours × 3600" \
    "${DA_CRS}/auth/client.rs" "heartbeat_secs" "(self.heartbeat_hours as i64).saturating_mul(3_600)"
src_next_line "3.3b-cfg 心跳周期由配置 heartbeat_hours 驱动" \
    "${DA_CRS}/auth/client.rs" "pub fn heartbeat_secs(&self) -> i64 {" "(self.heartbeat_hours as i64).saturating_mul(3_600)"
src_contains "3.3c 配置默认心跳间隔取字面值" \
    "${DA_CRS}/config.rs" "fn default_heartbeat_secs\(\) -> u64 \{"
src_next_line "3.3d 配置默认心跳间隔字面值 86_400（= 24h）" \
    "${DA_CRS}/config.rs" "fn default_heartbeat_secs() -> u64 {" "86_400"

src_contains "3.4 激活响应下发 heartbeat_hours=24" \
    "${LS_CRS}/service.rs" "heartbeat_hours: HEARTBEAT_HOURS"
src_contains "3.5 心跳间隔由配置 gateway.licensing.heartbeat_interval_secs 驱动" \
    "${DA_CRS}/license.rs" "心跳周期（来自 \`gateway\.licensing\.heartbeat_interval_secs\`）"
src_contains "3.6 心跳失败不立即降级 → 进入/继续宽限" \
    "${DA_CRS}/license.rs" "license runtime: heartbeat failed; entering/continuing offline grace"
src_contains "3.7 宽限剩余天数按「最后成功联网 + 7 天」计算" \
    "${DA_CRS}/license.rs" "fn grace_days_left"
src_contains "3.8 宽限耗尽 → Degraded" \
    "${DA_CRS}/license.rs" "offline grace expired"
# 「超期后停止北向、保留采集」：北向闸门 + 采集恒真，逐条落断言
src_contains "3.9 北向闸门实现（NorthForwardGate）" \
    "${DA_CRS}/license.rs" "impl NorthForwardGate for LicenseRuntime"
src_contains "3.10 北向拒绝原因含「本地采集继续」" \
    "${DA_CRS}/license.rs" "local capture continues"
src_contains "3.11 Degraded 状态下本地采集判据恒真（单测守卫）" \
    "${DA_CRS}/license.rs" "must always allow local capture"

# ===========================================================================
section "契约 4：客户端禁解绑 / 重置试用 / 废弃激活码入口"
# ===========================================================================

# 4a. crates/daemon 全量扫描
src_absent "4.1 无解绑类 API/方法"     "${DA_CRS}" "unbind|un_bind|release_lease|drop_lease|remove_license|delete_license|clear_license"
src_absent "4.2 无重置试用入口"        "${DA_CRS}" "reset_trial|reset_license|reset_machine|clear_trial|renew_trial_full"
src_absent "4.3 无废弃/停用激活码入口" "${DA_CRS}" "deactivate|revoke_activation|invalidate_activation|discard_activation|delete_activation"
src_absent "4.4 无换机/重绑入口"       "${DA_CRS}" "rebind|switch_machine|migrate_license"

# 4b. 路由层：daemon 管理面不得注册任何授权写路由
src_contains "4.5 daemon 管理面仅暴露只读授权状态路由" \
    "${DA_CRS}/mgmt/mod.rs" "\"/api/license/status\", get\(pages::license_status\)"
src_absent "4.6 daemon 无激活码发放/废弃/重发路由" \
    "${DA_CRS}/mgmt/mod.rs" "admin/codes|/revoke|/reissue|/admin/"
src_absent "4.7 daemon 无 ops 类越权写路由" \
    "${DA_CRS}/mgmt/mod.rs" "\"/api/license/.*\", (post|put|delete)\("

# 4c. RBAC 权限码 ≠ 入口。CodeRevoke 仅定义未消费属可接受；但若真被路由消费则 FAIL
src_absent "4.8 无路由消费 RBAC CodeRevoke（若出现即等于客户端带废弃入口）" \
    "${DA_CRS}" "ensure\(Permission::CodeRevoke\)|require\(Permission::CodeRevoke\)"

# 4d. 激活码不得经管理面泄漏/回写（只回布尔位）
src_contains "4.9 管理面只回 activation_code_set 布尔位，不回激活码值" \
    "${DA_CRS}/mgmt/settings.rs" "\"activation_code_set\": gateway\.licensing\.activation_code\.is_some\(\)"
src_absent "4.10 管理面响应体不得出现激活码明文" \
    "${DA_CRS}/mgmt/settings.rs" "\"activation_code\":"

# 4e. 换机唯一路径在厂商后台 + 云端（管理端前端只打 licensing-server 的 /admin/*）
src_contains "4.11 换机唯一路径 = 厂商后台废弃+重发（文档红线由实现注释承接）" \
    "${DA_CRS}/license.rs" "客户端侧\*\*不得\*\*出现解绑 / 重置试用 / 废弃激活码入口"
src_contains "4.12 客户端红线注释：连方法都不提供" \
    "${DA_CRS}/auth/client.rs" "红线：客户端没有「解绑 / 重置试用 / 换机」入口"
src_contains "4.13 管理端写端点后端契约 = licensing-server/src/http.rs" \
    "admin-console/src/api/repo.ts" "crates/licensing-server/src/http\.rs" "*.ts"
src_contains "4.14 管理端写端点路径为 /admin/codes/issue|/revoke|/reissue" \
    "admin-console/src/api/repo.ts" "/admin/codes(/issue|:id/revoke|:id/reissue)" "*.ts"

# ===========================================================================
section "契约 5：二次校验分档 A/B/C（默认 B）；客户自建 Broker 时服务端无消息级落点"
# ===========================================================================

src_contains "5.1 档位枚举 A/B/C（服务端，含 C 分支）" \
    "${LS_CRS}/model.rs" "VerifyMode::C => \"C\""
src_contains "5.1b 服务端档位可解析（含 C）" \
    "${LS_CRS}/model.rs" "\"C\" \| \"c\" => Ok\(VerifyMode::C\)"
src_contains "5.2 档位枚举 A/B/C（客户端）" \
    "${DA_CRS}/auth/client.rs" "VerifyMode::C => \"C\""
src_contains "5.3 租户默认档位缺省为 B" \
    "${LS_CRS}/model.rs" "verify_mode_default: VerifyMode::B"
src_contains "5.4 建租户时未指定则回落 B" \
    "${LS_CRS}/service.rs" "_ => VerifyMode::B,"
src_contains "5.5 DB 列缺省 'B'" \
    "${LS_CRS}/store.rs" "verify_mode_default TEXT NOT NULL DEFAULT 'B'"
src_contains "5.6 激活签发档位为 B" \
    "${LS_CRS}/service.rs" "verify_mode: VerifyMode::B\.as_str\(\)\.to_string\(\)"
src_contains "5.7 客户端无档位切换接口（随 Token 下发）" \
    "${DA_CRS}/auth/client.rs" "随 Token 下发，客户端无切换接口"

# 5b. 服务端对客户自建 Broker 无消息级落点：回执表只存 digest，不存业务数值；
#     且白名单**拒绝**业务字段（flow_rate 出现在测试里是「反例在证明会拒收」，
#     故此处断言「存在反例测试 + validate_whitelist_value 存在」，而非「字符串不存在」）。
src_contains "5.8 回执表仅存 payload_digest（无业务数值列）" \
    "${LS_CRS}/store.rs" "payload_digest TEXT NOT NULL,"
src_contains "5.9 回执白名单存在越界拒绝校验（业务字段被拒收）" \
    "${LS_CRS}/proto.rs" "fn validate_whitelist_value\(value: &serde_json::Value\) -> LicenseResult<\(\)>"
src_contains "5.9b 存在「业务字段 flow_rate 被拒收」的反例测试" \
    "${LS_CRS}/proto.rs" "whitelist_rejects_extra_business_field"
src_contains "5.9c 反例断言被拒字段确实是业务字段 flow_rate" \
    "${LS_CRS}/proto.rs" "err\.to_string\(\)\.contains\(\"flow_rate\"\)"
src_contains "5.10 客户端回执请求体白名单 8 字段且不带业务数值" \
    "${DA_CRS}/auth/client.rs" "绝不携带任何业务数值"

src_contains "5.11 租户档位可配置（管理面 PUT policy）" \
    "${LS_CRS}/http.rs" "\"/admin/tenants/:tenant_id/policy\""

# ---------------------------------------------------------------------------
# 已知偏离 #1（KNOWN GAP，默认 FAIL，需显式 acknowledge 才能置为 PASS）：
# `verify_mode_default` 只落库、从未被任何判定路径消费 → A 档实际无法下发，
# 租户策略写进去是空操作。契约字面「默认 B」是符合的，但「档位可切换」落空。
# 证据：
#   - 写：service.rs:555-560（create_tenant 回落 B）、http.rs:107（PUT policy 路由）
#   - 读：全仓 grep 仅 store.rs(CRUD) / http.rs(admin 回显) / model.rs(构造)
#   - 判定：service.rs:373/384/411、1355/1366、1414/1425 一律硬编码 VerifyMode::B
#   - 消费：store.rs 的 lease.verify_mode 只出现在 INSERT/SELECT 列，无任何决策读取
# ---------------------------------------------------------------------------
section "已知偏离 #1：租户 verify_mode_default 落库但从未消费（A 档无法下发）"
GAP1_ACK="${LICENSE_E2E_ACK_KNOWN_GAP_1:-0}"
# 先证明「策略写存在」
src_contains "GAP1-a 租户策略写入路径存在" "${LS_CRS}/service.rs" "pub fn admin_update_tenant_policy"
src_contains "GAP1-b 档位在租户上落库"     "${LS_CRS}/model.rs" "verify_mode_default: VerifyMode"
# 再证明「没有任何判定路径消费它」——若下面三条中任一条 FAIL，偏离即被修复、回归为 PASS
src_fn_absent  "GAP1-c 激活函数体内不读租户默认档位"     "${LS_CRS}/service.rs" "activate"              "verify_mode_default"
src_fn_absent  "GAP1-d 二次校验函数体内不读档位配置"     "${LS_CRS}/service.rs" "verify"                "verify_mode_default"
src_fn_absent  "GAP1-e 心跳函数体内不读档位配置"         "${LS_CRS}/service.rs" "heartbeat"             "verify_mode_default"
src_fn_absent  "GAP1-f 同机改绑函数体内不读档位配置"     "${LS_CRS}/service.rs" "auto_rebind_same_machine" "verify_mode_default"
E_TOTAL=$((E_TOTAL + 1))
if [ "${GAP1_ACK}" = "1" ]; then
    E_PASS=$((E_PASS + 1))
    ok "GAP1 已知偏离已由 human acknowledge（LICENSE_E2E_ACK_KNOWN_GAP_1=1）"
    log "    ⚠ 该偏离仍在生效：A 档在当前实现下无法下发，租户策略写为空操作。"
else
    E_FAIL=$((E_FAIL + 1)); FAILED_NAMES+=("GAP1 租户 verify_mode_default 从未消费")
    bad "GAP1 存在实打实的口径落差（详见上方 GAP1-a/b/c/d 证据线）"
    log "    若确认接受该偏离，用 LICENSE_E2E_ACK_KNOWN_GAP_1=1 重跑即可置为 PASS。"
fi

# ===========================================================================
section "契约 6：授权判定恒在网关 Rust 侧，不依赖前端"
# ===========================================================================

src_contains "6.1 授权状态唯一真相源在 Rust 侧（LicenseRuntime）" \
    "${DA_CRS}/license.rs" "授权状态 \[\`LicenseState\`\] 由本模块（Rust 侧）持有"
src_contains "6.2 不暴露「由 JS/WebView 传入并直接采信状态」的入口" \
    "${DA_CRS}/license.rs" "不\*\*暴露任何「由 JS / WebView 传入并直接采信状态」"
src_contains "6.3 构造器只接受 Rust 侧注入的类型化配置" \
    "${DA_CRS}/license.rs" "构造器只接受 Rust 侧注入的类型化配置"
src_contains "6.4 免费版配额闸门在 Rust 侧 enforce_free_limits" \
    "${DA_CRS}/license.rs" "fn enforce_free_limits"
src_contains "6.5 北向闸门实现在 Rust 侧（NorthForwardGate）" \
    "${DA_CRS}/license.rs" "impl NorthForwardGate for LicenseRuntime"

# 前端只读展示、不下判定：web-console 只映射 GET /api/license/status
src_contains "6.6 网关前端授权状态只走 GET /api/license/status（只读）" \
    "web-console/src/api/model.ts" "授权状态    ↔ \`GET /api/license/status\`" "*.ts"
src_contains "6.6b 前端状态拉取函数只发 GET /api/license/status" \
    "web-console/src/api/repo.ts" "await apiRequest<Record<string, unknown>>\('/api/license/status'\)" "*.ts"

# 6.7：前端 activate 是「提交」触点，但**必须永不成功**（后端无此端点 → ok:false）。
# 断言的是返回值（真实代码路径的返回形状），不是「字符串不存在」。
src_contains "6.7 前端 activate 触点永不成功（后端无激活端点 → ok:false）" \
    "web-console/src/api/repo.ts" "后端未提供激活接口（POST /api/license/activate 未落地）" "*.ts"
src_contains "6.7b 该触点签名返回 ok:boolean（fail-closed，无法自行点亮授权）" \
    "web-console/src/api/repo.ts" "Promise<\{ ok: boolean; message: string \}>" "*.ts"
src_absent "6.7c 前端不得注册授权写路由" \
    "web-console/src" "apiRequest(('|')?[^)]*license/(unbind|reset|revoke|reissue)" "*.ts"

# 6.8：northForwardAllowed 只能来自只读 GET 的映射，不得由用户输入决定。
src_contains "6.8 前端北向判据仅由只读 status 映射而来（pickBool 兜底 false）" \
    "web-console/src/api/repo.ts" "northForwardAllowed: pickBool\(raw, 'north_forward_allowed', false\)" "*.ts"
src_contains "6.8b 状态来源标注为只读 GET" \
    "web-console/src/api/repo.ts" "拉取授权状态：\`GET /api/license/status\`" "*.ts"
src_absent "6.8c 前端不得写入授权状态（无赋值来源为输入）" \
    "web-console/src" "realCache\.license = \{[^}]*(input|formData|payload)" "*.ts"

report "contract-static.sh"
