//! 设备分组管理接口（需求 4）。
//!
//! ## 端点（写操作要求 `Authorization: Bearer` JWT + `Permission::DeviceWrite`）
//! - `GET    /api/groups`             分组列表（读，开放）——**必含默认分组**；
//! - `POST   /api/groups`             新建分组 `{id?, name, reason?}`；
//! - `PUT    /api/groups/:id`         改名 `{name, reason?}`（默认分组**不可改名**）；
//! - `DELETE /api/groups/:id`         删除 `{confirm, reason?}`（默认分组**不可删除**；
//!   非空分组删除后其下设备**回落默认分组**）。
//!
//! ## 默认分组语义（写死）
//! id 固定 [`DEFAULT_GROUP_ID`]、名称固定 [`DEFAULT_GROUP_NAME`]，**永远存在且
//! 不可删除 / 不可改名**；配置里没有对应 `[[device_groups]]` 行也在接口层合成出来
//!（无需强制写盘）。`DeviceConfig::group_id` 缺省 / 空 / 指向未知分组 → 归属默认分组。
//!
//! ## 写路径范式（对齐 writeapi）
//! 鉴权 extractor（401）→ `ensure(Permission::DeviceWrite)`（403，入审计）→ 全进程写锁
//! → 校验（结构化错误）→ [`super::pages::persist_config`]（license 配额闸门 + 写前
//! 备份 + 原子落盘 + 热生效）→ 审计。**actor 恒取 JWT `sub`**——请求体里的 `actor`
//! 字段即便出现也被忽略（不接受客户端自报身份）。
//!
//! ## 红线
//! 大数（`device_count` / `config_version`）一律**字符串**编码；错误体结构化
//!（`{error, field, reason, allowed}`），绝不靠 `msg.contains` 字符串匹配。

use std::collections::{BTreeSet, HashMap, HashSet};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{OpsAction, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::{writeapi, MgmtState};
use crate::config::{DeviceGroupConfig, GatewayConfig, DEFAULT_GROUP_ID, DEFAULT_GROUP_NAME};

/// 分组 id 最大长度。
const MAX_GROUP_ID_LEN: usize = 64;
/// 分组名最大长度。
const MAX_GROUP_NAME_LEN: usize = 128;

/// 分组视图（`GET /api/groups` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupView {
    /// 分组标识。
    pub id: String,
    /// 分组显示名。
    pub name: String,
    /// 该分组下的设备数（点位设备 ∪ 登记设备的并集）。
    pub device_count: u64,
    /// 是否默认分组（永远存在、不可删 / 不可改名）。
    pub is_default: bool,
}

/// 解析设备分组全量视图（纯函数；默认分组恒在首位）。
///
/// 设备归属：`DeviceConfig::group_id` 非空且指向已声明分组 → 该分组；
/// 否则（未声明 / 空 / 未知分组）→ 默认分组。设备集合 = 点位行 device_id ∪
/// 登记段 device_id（去重），与 `/api/devices` 同口径。
#[must_use]
pub fn resolve_groups(config: &GatewayConfig) -> Vec<GroupView> {
    let known: HashSet<&str> = config.device_groups.iter().map(|g| g.id.as_str()).collect();
    let declared: HashMap<&str, &str> = config
        .devices
        .iter()
        .filter_map(|d| {
            d.group_id
                .as_deref()
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(|g| (d.device_id.as_str(), g))
        })
        .collect();

    let mut devices: BTreeSet<&str> = BTreeSet::new();
    for point in &config.points {
        devices.insert(point.device_id.as_str());
    }
    for device in &config.devices {
        devices.insert(device.device_id.as_str());
    }

    let mut counts: HashMap<String, u64> = HashMap::new();
    for device_id in &devices {
        let effective = declared
            .get(device_id)
            .filter(|g| known.contains(**g))
            .map_or_else(|| DEFAULT_GROUP_ID.to_string(), |g| (*g).to_string());
        let entry = counts.entry(effective).or_insert(0);
        *entry = entry.saturating_add(1);
    }

    let mut views = Vec::with_capacity(config.device_groups.len() + 1);
    views.push(GroupView {
        id: DEFAULT_GROUP_ID.to_string(),
        name: DEFAULT_GROUP_NAME.to_string(),
        device_count: counts.get(DEFAULT_GROUP_ID).copied().unwrap_or(0),
        is_default: true,
    });
    for group in &config.device_groups {
        // 配置里即便误写 `default` 行也跳过：默认分组由接口层唯一合成。
        if group.id == DEFAULT_GROUP_ID {
            continue;
        }
        views.push(GroupView {
            id: group.id.clone(),
            name: group.name.clone(),
            device_count: counts.get(&group.id).copied().unwrap_or(0),
            is_default: false,
        });
    }
    views
}

/// 分组 id 合法性：非空、长度上限、字符集 `[A-Za-z0-9_-]`、且不得为默认分组 id。
fn validate_group_id(id: &str) -> Result<(), (&'static str, String)> {
    if id.is_empty() {
        return Err(("id", "group id must not be empty".to_string()));
    }
    if id.len() > MAX_GROUP_ID_LEN {
        return Err((
            "id",
            format!("group id must be at most {MAX_GROUP_ID_LEN} characters"),
        ));
    }
    if id == DEFAULT_GROUP_ID {
        return Err((
            "id",
            format!("group id {DEFAULT_GROUP_ID:?} is reserved for the default group"),
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err((
            "id",
            "group id may only contain ASCII letters, digits, '_' and '-'".to_string(),
        ));
    }
    Ok(())
}

/// 分组名合法性：trim 后非空、长度上限。
fn validate_group_name(name: &str) -> Result<(), (&'static str, String)> {
    if name.trim().is_empty() {
        return Err(("name", "group name must not be empty".to_string()));
    }
    if name.chars().count() > MAX_GROUP_NAME_LEN {
        return Err((
            "name",
            format!("group name must be at most {MAX_GROUP_NAME_LEN} characters"),
        ));
    }
    Ok(())
}

/// 生成唯一分组 id（`g-<毫秒时间戳>`；碰撞时追加序号）。
fn generate_group_id(config: &GatewayConfig) -> String {
    let base = format!("g-{}", super::health::now_ms());
    if !config.device_groups.iter().any(|g| g.id == base) && base != DEFAULT_GROUP_ID {
        return base;
    }
    for n in 2u32..1_000 {
        let candidate = format!("{base}-{n}");
        if !config.device_groups.iter().any(|g| g.id == candidate) {
            return candidate;
        }
    }
    // 极端兜底：仍碰撞则用进程内计数尾缀（不可达路径，不 panic）。
    format!("{base}-{}", std::process::id())
}

/// 400 + 结构化校验错误（字段 + 原因 + 允许值）。
fn group_validation_error(field: &'static str, reason: &str, allowed: &str) -> Response {
    writeapi::validation_error(field, reason, allowed)
}

// ---- 请求体（`actor` 字段即便出现也被忽略：actor 恒取 JWT sub） ----

#[derive(Debug, Default, Deserialize)]
struct GroupCreateBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GroupUpdateBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GroupDeleteBody {
    #[serde(default)]
    confirm: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

/// 审计原因附注（`reason?` 非空时追加）。
fn reason_suffix(reason: &Option<String>) -> String {
    match reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => format!("; reason={r:?}"),
        None => String::new(),
    }
}

/// 写操作受理响应（`device_count` / `config_version` 字符串编码）。
fn group_accepted(view: &GroupView, version: u64) -> Response {
    Json(json!({
        "accepted": true,
        "id": view.id,
        "name": view.name,
        "device_count": view.device_count.to_string(),
        "is_default": view.is_default,
        "config_version": version.to_string(),
    }))
    .into_response()
}

// ---- 处理器 ----

/// `GET /api/groups` → 分组列表（读，开放；**必含默认分组**）。
pub async fn groups_list(State(state): State<MgmtState>) -> Response {
    let rows: Vec<Value> = resolve_groups(&state.config())
        .into_iter()
        .map(|view| {
            json!({
                "id": view.id,
                "name": view.name,
                // 大数红线：device_count 为计数，字符串编码。
                "device_count": view.device_count.to_string(),
                "is_default": view.is_default,
            })
        })
        .collect();
    Json(Value::Array(rows)).into_response()
}

/// `POST /api/groups` → 新建分组（`device.write`）。
///
/// body：`{id?, name, reason?}`（`id` 缺省由服务端生成；显式 `id` 须唯一 /
/// 非空 / 仅 `[A-Za-z0-9_-]`）。重复 id（含保留的 `default`）→ 400。
pub async fn group_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceCreate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: GroupCreateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::DeviceCreate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("create group: malformed json body: {err}"),
            );
            return group_validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {id?, name, reason?}",
            );
        }
    };
    if let Err((field, reason)) = validate_group_name(&req.name) {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("create group: invalid name: {reason}"),
        );
        return group_validation_error(field, &reason, "non-empty group name");
    }

    let _guard = writeapi::write_guard();
    let mut config = (*state.config()).clone();
    let id = match req.id.as_deref().map(str::trim) {
        Some(explicit) if !explicit.is_empty() => {
            if let Err((field, reason)) = validate_group_id(explicit) {
                writeapi::audit(
                    &state,
                    &actor,
                    OpsAction::DeviceCreate,
                    true,
                    OUTCOME_BAD_REQUEST,
                    &format!("create group: invalid id: {reason}"),
                );
                return group_validation_error(
                    field,
                    &reason,
                    "unique id matching [A-Za-z0-9_-]{1,64}, not \"default\"",
                );
            }
            explicit.to_string()
        }
        _ => generate_group_id(&config),
    };
    if config.device_groups.iter().any(|g| g.id == id) {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("create group: duplicate id {id:?}"),
        );
        return group_validation_error(
            "id",
            &format!("group {id:?} already exists"),
            "unique group id",
        );
    }
    let name = req.name.trim().to_string();
    config.device_groups.push(DeviceGroupConfig {
        id: id.clone(),
        name: name.clone(),
    });
    let detail = format!(
        "create device group {id:?} (name={name:?}){}",
        reason_suffix(&req.reason)
    );
    match super::pages::persist_config(&state, config, &actor, OpsAction::DeviceCreate, &detail) {
        Ok(version) => {
            let view = GroupView {
                id,
                name,
                device_count: 0,
                is_default: false,
            };
            group_accepted(&view, version)
        }
        Err(resp) => resp,
    }
}

/// `PUT /api/groups/:id` → 分组改名（`device.write`）。
///
/// 默认分组**不允许改名**（400 并给出可解释原因）；分组不存在 → 404。
pub async fn group_update(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let id = id.trim().to_string();
    let req: GroupUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::DeviceUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("update group {id:?}: malformed json body: {err}"),
            );
            return group_validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name, reason?}",
            );
        }
    };
    if let Err((field, reason)) = validate_group_name(&req.name) {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("update group {id:?}: invalid name: {reason}"),
        );
        return group_validation_error(field, &reason, "non-empty group name");
    }
    if id == DEFAULT_GROUP_ID {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            "update group: default group cannot be renamed",
        );
        return group_validation_error(
            "id",
            &format!("the default group {DEFAULT_GROUP_ID:?} cannot be renamed"),
            "default group is fixed; rename one of the user-defined groups instead",
        );
    }

    let _guard = writeapi::write_guard();
    let mut config = (*state.config()).clone();
    let Some(entry) = config.device_groups.iter_mut().find(|g| g.id == id) else {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("update group: unknown group {id:?}"),
        );
        return writeapi::not_found(&format!("group {id:?} not found"));
    };
    let name = req.name.trim().to_string();
    entry.name = name.clone();
    let device_count = count_devices_in_group(&config, &id);
    let detail = format!(
        "update device group {id:?} (name={name:?}){}",
        reason_suffix(&req.reason)
    );
    match super::pages::persist_config(&state, config, &actor, OpsAction::DeviceUpdate, &detail) {
        Ok(version) => group_accepted(
            &GroupView {
                id,
                name,
                device_count,
                is_default: false,
            },
            version,
        ),
        Err(resp) => resp,
    }
}

/// `DELETE /api/groups/:id` → 删除分组（`device.write`）。
///
/// 默认分组**拒删**（400）；须 `confirm=true`；非空分组删除后其下设备
/// **回落默认分组**（登记段 `group_id` 清空）。
pub async fn group_delete(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceDelete,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let id = id.trim().to_string();
    let req: GroupDeleteBody = if body.is_empty() {
        GroupDeleteBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(err) => {
                writeapi::audit(
                    &state,
                    &actor,
                    OpsAction::DeviceDelete,
                    true,
                    OUTCOME_BAD_REQUEST,
                    &format!("delete group {id:?}: malformed json body: {err}"),
                );
                return group_validation_error(
                    "body",
                    &format!("malformed JSON: {err}"),
                    "object {confirm, reason?}",
                );
            }
        }
    };
    if id == DEFAULT_GROUP_ID {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceDelete,
            true,
            OUTCOME_BAD_REQUEST,
            "delete group: default group cannot be deleted",
        );
        return group_validation_error(
            "id",
            &format!("the default group {DEFAULT_GROUP_ID:?} cannot be deleted"),
            "default group always exists; delete one of the user-defined groups instead",
        );
    }
    if req.confirm != Some(true) {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("delete group {id:?}: confirmation missing"),
        );
        return group_validation_error(
            "confirm",
            "deleting a group requires explicit confirmation",
            "true (boolean); passing confirm=false is refused",
        );
    }

    let _guard = writeapi::write_guard();
    let mut config = (*state.config()).clone();
    let Some(idx) = config.device_groups.iter().position(|g| g.id == id) else {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::DeviceDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("delete group: unknown group {id:?}"),
        );
        return writeapi::not_found(&format!("group {id:?} not found"));
    };
    let removed = config.device_groups.remove(idx);
    // 其下设备回落默认分组（清空登记段 group_id）。
    let mut rehomed = 0u64;
    for device in &mut config.devices {
        if device.group_id.as_deref().map(str::trim) == Some(id.as_str()) {
            device.group_id = None;
            rehomed = rehomed.saturating_add(1);
        }
    }
    let detail = format!(
        "delete device group {:?} (name={:?}; {rehomed} device registration(s) fell back to {DEFAULT_GROUP_ID}){}",
        removed.id,
        removed.name,
        reason_suffix(&req.reason)
    );
    // 落盘前重新用剩余配置解析默认分组计数（含回落设备）。
    let default_view = resolve_groups(&config)
        .into_iter()
        .find(|v| v.is_default)
        .unwrap_or(GroupView {
            id: DEFAULT_GROUP_ID.to_string(),
            name: DEFAULT_GROUP_NAME.to_string(),
            device_count: 0,
            is_default: true,
        });
    match super::pages::persist_config(&state, config, &actor, OpsAction::DeviceDelete, &detail) {
        Ok(version) => group_accepted(&default_view, version),
        Err(resp) => resp,
    }
}

/// 统计某分组下的设备数（用于改名响应回显）。
fn count_devices_in_group(config: &GatewayConfig, group_id: &str) -> u64 {
    resolve_groups(config)
        .into_iter()
        .find(|v| v.id == group_id)
        .map_or(0, |v| v.device_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认分组永远存在（配置无 `[[device_groups]]` 段时亦然），且在首位。
    #[test]
    fn default_group_always_present() {
        let config = GatewayConfig::parse("").expect("empty config");
        let views = resolve_groups(&config);
        assert_eq!(views.len(), 1, "only the default group");
        assert_eq!(views[0].id, DEFAULT_GROUP_ID);
        assert_eq!(views[0].name, DEFAULT_GROUP_NAME);
        assert!(views[0].is_default);
        assert_eq!(views[0].device_count, 0);
    }

    /// 设备归属：声明分组 → 该分组；未声明 / 空 / 未知分组 → 默认分组。
    #[test]
    fn device_group_membership_and_fallback() {
        let config = GatewayConfig::parse(
            r#"
[[device_groups]]
id = "line-a"
name = "A 线"

[[points]]
device_id = "dev-in-a"
point_id = "p1"
protocol = "modbus-tcp"
address = "10.0.0.1:502"

[[points]]
device_id = "dev-unknown-group"
point_id = "p2"
protocol = "modbus-tcp"
address = "10.0.0.2:502"

[[devices]]
device_id = "dev-in-a"
group_id = "line-a"

[[devices]]
device_id = "dev-unknown-group"
group_id = "no-such-group"

[[devices]]
device_id = "dev-bare"
protocol = "modbus-tcp"
"#,
        )
        .expect("parse");
        let views = resolve_groups(&config);
        assert_eq!(views.len(), 2, "default + line-a");
        assert_eq!(views[0].id, DEFAULT_GROUP_ID);
        assert_eq!(
            views[0].device_count, 2,
            "unknown-group + bare fall back to default"
        );
        assert_eq!(views[1].id, "line-a");
        assert_eq!(views[1].device_count, 1);
        assert!(!views[1].is_default);
    }

    /// 配置里误写 `default` 行不产生重复分组（接口层唯一合成默认分组）。
    #[test]
    fn configured_default_row_is_deduplicated() {
        let config =
            GatewayConfig::parse("[[device_groups]]\nid = \"default\"\nname = \"我以为的默认\"\n")
                .expect("parse");
        let views = resolve_groups(&config);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, DEFAULT_GROUP_ID);
        assert_eq!(views[0].name, DEFAULT_GROUP_NAME, "name is fixed");
    }

    /// id / name 校验：保留字、非法字符、超长、空值。
    #[test]
    fn group_id_and_name_validation() {
        assert!(validate_group_id("line-a").is_ok());
        assert!(validate_group_id("Line_1").is_ok());
        assert!(validate_group_id("").is_err(), "empty rejected");
        assert!(validate_group_id("default").is_err(), "reserved");
        assert!(validate_group_id("has space").is_err(), "charset");
        assert!(validate_group_id("含中文").is_err(), "charset");
        assert!(validate_group_id(&"a".repeat(MAX_GROUP_ID_LEN + 1)).is_err());

        assert!(validate_group_name("A 线").is_ok());
        assert!(validate_group_name("   ").is_err(), "blank rejected");
        assert!(validate_group_name(&"名".repeat(MAX_GROUP_NAME_LEN + 1)).is_err());
    }

    /// 生成的 id 唯一且合法。
    #[test]
    fn generated_group_id_is_unique_and_valid() {
        let mut config = GatewayConfig::parse("").expect("empty");
        let first = generate_group_id(&config);
        assert!(validate_group_id(&first).is_ok());
        config.device_groups.push(DeviceGroupConfig {
            id: first.clone(),
            name: "x".to_string(),
        });
        let second = generate_group_id(&config);
        assert_ne!(first, second, "collision resolved");
        assert!(validate_group_id(&second).is_ok());
    }
}
