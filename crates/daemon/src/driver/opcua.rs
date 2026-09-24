//! OPC UA 客户端驱动（plan task 10）。
//!
//! 基于 `async-opcua-client 0.19`（MPL-2.0，纯 Rust 栈）适配 [`Driver`] trait：
//! - 角色：南向**客户端**，连接 PLC 的 OPC UA server（匿名认证起步，用户名/密码
//!   留配置字段；安全策略起步仅支持 `None`，签名/加密策略留待后续 wave）。
//! - 地址映射：[`PointAddress`] 的 `db` = 命名空间索引（ns），`start` = 数值
//!   节点标识（`i=<start>`）；`area` / `bit` / `bit_index` 须为默认值，`count`
//!   须为 1（OPC UA 一次读取一个节点的 Value 属性），否则
//!   [`DaemonError::ProtocolError`]（错误码 `ERR_PROTOCOL` = 1000）。
//! - 值编码：节点值 ↔ 原始字节采用**小端**编码（Boolean 1 字节；SByte/Byte 1；
//!   Int16/UInt16 2；Int32/UInt32/Float 4；Int64/UInt64/Double 8）。读取时按
//!   服务器返回的 Variant 标量类型编码；写入时先读目标节点当前值以确定类型，
//!   再按该类型解码原始字节（长度不符 → [`DaemonError::ProtocolError`]），
//!   保证写入与节点声明的 DataType 一致。
//! - 节点浏览：[`OpcuaDriver::browse`] 返回指定节点（默认 ObjectsFolder）正向
//!   层级引用下的节点摘要（NodeId / BrowseName / DisplayName）。
//! - 错误映射：Endpoint URL / 安全策略非法、节点级服务状态码失败、值编解码
//!   失败 → [`DaemonError::ProtocolError`]（1000）；拨号失败 / GetEndpoints 失败 /
//!   会话建立失败 / 请求级传输错误与超时 → [`DaemonError::NetworkError`]（6000）。
//! - 断线重连：请求遇连接丢失（传输错误 / 超时）时按 [`Reconnector`] 指数退避
//!   等待，重连一次并重试该请求；重试仍失败则返回错误。`connect()` 成功会重置
//!   退避，恢复重连不重置（保持退避推进状态）——与 modbus 驱动语义一致。

use std::time::Duration;

use async_trait::async_trait;
use opcua_client::{ClientBuilder, IdentityToken, Password, Session};
use opcua_types::{
    AttributeId, BrowseDescription, BrowseDescriptionResultMask, BrowseDirection, DataValue,
    EndpointDescription, MessageSecurityMode, NodeClassMask, NodeId, ReadValueId, ReferenceTypeId,
    StatusCode, TimestampsToReturn, UserTokenPolicy, Variant, VariantScalarTypeId, WriteValue,
};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::driver::{Driver, PointSample, ReadPoint, Reconnector, WritePoint};
use crate::error::{DaemonError, DaemonResult};

/// ObjectsFolder 的数值节点标识（Browse 默认根节点，ns=0;i=85）。
const OBJECTS_FOLDER_ID: u32 = 85;
/// Browse 单节点最大返回引用数（树摘要起步，不做 browse_next 续传）。
const BROWSE_MAX_REFERENCES: u32 = 1000;

/// OPC UA 驱动配置。
#[derive(Debug, Clone)]
pub struct OpcuaConfig {
    /// 服务器 Endpoint URL（`opc.tcp://host:port[/path]`）。
    pub endpoint_url: String,
    /// 安全策略 URI 短名（起步仅支持 `"None"`；Basic128Rsa15 等留待后续）。
    pub security_policy: String,
    /// 用户名（`None` = 匿名；与 `password` 必须成对出现）。
    pub username: Option<String>,
    /// 密码（与 `username` 成对出现）。
    pub password: Option<String>,
    /// 客户端 PKI 目录（证书/私钥存放处；`None` = 默认 `./pki`）。
    pub pki_dir: Option<std::path::PathBuf>,
    /// 单次请求超时（超时按连接丢失处理并触发重连）。
    pub request_timeout: Duration,
    /// 断线重连退避（`connect()` 成功重置；恢复重连不重置）。
    pub reconnector: Reconnector,
}

impl Default for OpcuaConfig {
    fn default() -> Self {
        Self {
            endpoint_url: "opc.tcp://127.0.0.1:4840".to_string(),
            security_policy: "None".to_string(),
            username: None,
            password: None,
            pki_dir: None,
            request_timeout: Duration::from_secs(5),
            reconnector: Reconnector::default(),
        }
    }
}

/// 节点浏览摘要（节点树的一个条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcuaNodeSummary {
    /// 节点 ID 字符串（`ns=2;i=1001` 形式，与 OPC UA 标准字符串编码一致）。
    pub node_id: String,
    /// 浏览名（BrowseName，命名空间内唯一）。
    pub browse_name: String,
    /// 显示名（DisplayName，界面展示用）。
    pub display_name: String,
}

/// OPC UA 驱动。
///
/// 会话（`Arc<Session>`）与事件循环句柄成对持有；请求失败（传输错误 / 超时）
/// 时先丢弃旧会话再按退避重连。事件循环已禁用其内部自动重连——重连策略完全
/// 由本驱动按 [`Reconnector`] 语义控制。
pub struct OpcuaDriver {
    config: OpcuaConfig,
    session: Option<std::sync::Arc<Session>>,
    event_loop: Option<JoinHandle<StatusCode>>,
    reconnector: Reconnector,
}

/// 单次 OPC UA 服务调用（`run_request` 内部执行；`Clone` 供重试复用）。
#[derive(Debug, Clone)]
enum OpcuaOp {
    /// Read 服务：读取节点 Value 属性。
    Read(Vec<ReadValueId>),
    /// Write 服务：写入节点 Value 属性。
    Write(Vec<WriteValue>),
    /// Browse 服务：浏览节点层级引用。
    Browse(Vec<BrowseDescription>),
}

/// 请求结果。
#[derive(Debug)]
enum OpcuaOutcome {
    /// Read 返回的 DataValue（与请求等长同序）。
    Read(Vec<DataValue>),
    /// Write 返回的逐节点状态码。
    Written(Vec<StatusCode>),
    /// Browse 返回的逐节点结果。
    Browsed(Vec<opcua_types::BrowseResult>),
}

/// 请求级错误：服务调用失败（传输 / 会话 / 请求级状态码）。
///
/// OPC UA 的服务调用失败一律按「连接丢失」处理——节点级语义错误走
/// `DataValue.status`（Read）或逐节点写状态码（Write），在结果解码阶段判定为
/// [`DaemonError::ProtocolError`]，不经过本类型。因此本类型只有一种语义：
/// 触发退避重连并重试一次。
struct RequestError(String);

impl RequestError {
    /// 终态映射：连接丢失 → [`DaemonError::NetworkError`]。
    fn finalize(self) -> DaemonError {
        DaemonError::NetworkError(self.0)
    }
}

/// Variant 标量 → 小端原始字节（驱动支持的类型集合，其余返回 `None`）。
fn variant_to_bytes(value: &Variant) -> Option<Vec<u8>> {
    let bytes = match value {
        Variant::Boolean(b) => vec![u8::from(*b)],
        Variant::SByte(v) => v.to_le_bytes().to_vec(),
        Variant::Byte(v) => vec![*v],
        Variant::Int16(v) => v.to_le_bytes().to_vec(),
        Variant::UInt16(v) => v.to_le_bytes().to_vec(),
        Variant::Int32(v) => v.to_le_bytes().to_vec(),
        Variant::UInt32(v) => v.to_le_bytes().to_vec(),
        Variant::Int64(v) => v.to_le_bytes().to_vec(),
        Variant::UInt64(v) => v.to_le_bytes().to_vec(),
        Variant::Float(v) => v.to_le_bytes().to_vec(),
        Variant::Double(v) => v.to_le_bytes().to_vec(),
        _ => return None,
    };
    Some(bytes)
}

/// 按目标 Variant 标量类型解码小端原始字节（长度不符 / 类型不支持返回 `None`）。
fn bytes_to_variant(bytes: &[u8], ty: VariantScalarTypeId) -> Option<Variant> {
    let take = |n: usize| -> Option<&[u8]> {
        if bytes.len() == n {
            Some(&bytes[..n])
        } else {
            None
        }
    };
    let variant = match ty {
        VariantScalarTypeId::Boolean => Variant::Boolean(match bytes {
            [0] => false,
            [1] => true,
            _ => return None,
        }),
        VariantScalarTypeId::SByte => Variant::SByte(i8::from_le_bytes(take(1)?.try_into().ok()?)),
        VariantScalarTypeId::Byte => Variant::Byte(*take(1)?.first()?),
        VariantScalarTypeId::Int16 => Variant::Int16(i16::from_le_bytes(take(2)?.try_into().ok()?)),
        VariantScalarTypeId::UInt16 => {
            Variant::UInt16(u16::from_le_bytes(take(2)?.try_into().ok()?))
        }
        VariantScalarTypeId::Int32 => Variant::Int32(i32::from_le_bytes(take(4)?.try_into().ok()?)),
        VariantScalarTypeId::UInt32 => {
            Variant::UInt32(u32::from_le_bytes(take(4)?.try_into().ok()?))
        }
        VariantScalarTypeId::Int64 => Variant::Int64(i64::from_le_bytes(take(8)?.try_into().ok()?)),
        VariantScalarTypeId::UInt64 => {
            Variant::UInt64(u64::from_le_bytes(take(8)?.try_into().ok()?))
        }
        VariantScalarTypeId::Float => Variant::Float(f32::from_le_bytes(take(4)?.try_into().ok()?)),
        VariantScalarTypeId::Double => {
            Variant::Double(f64::from_le_bytes(take(8)?.try_into().ok()?))
        }
        _ => return None,
    };
    Some(variant)
}

/// 校验 OPC UA 点位地址：`db` = 命名空间，`start` = 数值节点标识。
fn validate_node_address(p: &ReadPoint) -> DaemonResult<NodeId> {
    let addr = &p.address;
    if addr.area.is_some() || addr.bit || addr.bit_index != 0 {
        return Err(DaemonError::ProtocolError(format!(
            "opc ua point address must use ns(start) form only, got {addr:?}"
        )));
    }
    if p.count != 1 {
        return Err(DaemonError::ProtocolError(format!(
            "opc ua read count must be 1 (one Value attribute per node), got {}",
            p.count
        )));
    }
    // `PointAddress.db` 为 `u32`，而 OPC UA 的命名空间索引是 `u16`。绝不可 `as u16`
    // 静默截断（`65537 → 1` 会悄悄读写**另一个命名空间的同号节点**），必须显式拒绝。
    let ns = u16::try_from(addr.db).map_err(|_| {
        DaemonError::ProtocolError(format!("opc ua namespace index {} exceeds u16", addr.db))
    })?;
    Ok(NodeId::new(ns, addr.start))
}

impl OpcuaDriver {
    /// 创建驱动（`reconnector` 从配置克隆为工作状态）。
    pub fn new(config: OpcuaConfig) -> Self {
        let reconnector = config.reconnector.clone();
        Self {
            config,
            session: None,
            event_loop: None,
            reconnector,
        }
    }

    /// 当前重连退避状态（只读；测试断言用）。
    pub fn reconnector(&self) -> &Reconnector {
        &self.reconnector
    }

    /// 丢弃当前会话与事件循环（尽力而为，不做优雅关闭）。
    fn teardown(&mut self) {
        if let Some(handle) = self.event_loop.take() {
            handle.abort();
        }
        self.session = None;
    }

    /// 建立会话（GetEndpoints → CreateSession → ActivateSession → 等待连接）。
    async fn open(&mut self) -> DaemonResult<()> {
        // Endpoint 配置校验（无网络即可判定）。
        let url = self.config.endpoint_url.trim().to_string();
        if !url.starts_with("opc.tcp://") || url.len() <= "opc.tcp://".len() {
            return Err(DaemonError::ProtocolError(format!(
                "invalid opc ua endpoint url {:?} (expected opc.tcp://host:port)",
                self.config.endpoint_url
            )));
        }
        let policy = self.config.security_policy.trim();
        if !policy.eq_ignore_ascii_case("None") {
            return Err(DaemonError::ProtocolError(format!(
                "opc ua security policy {policy:?} not supported yet (only \"None\")"
            )));
        }
        let identity = match (&self.config.username, &self.config.password) {
            (None, None) => IdentityToken::Anonymous,
            (Some(user), Some(pass)) => {
                IdentityToken::new_user_name(user.clone(), Password::new(pass.clone()))
            }
            _ => {
                return Err(DaemonError::ProtocolError(
                    "opc ua username and password must both be set".to_string(),
                ))
            }
        };

        let mut builder = ClientBuilder::new()
            .application_name("iot-daq-daemon")
            .application_uri("urn:iot-daq:daemon")
            .create_sample_keypair(false)
            // 重连策略完全由驱动侧 Reconnector 控制，事件循环不自动重试。
            .session_retry_limit(0);
        if let Some(dir) = &self.config.pki_dir {
            builder = builder.pki_dir(dir.clone());
        }
        let mut client = builder.client().map_err(|errors| {
            DaemonError::ProtocolError(format!("opc ua client config invalid: {errors:?}"))
        })?;

        let endpoint: EndpointDescription = (
            url.as_str(),
            policy,
            MessageSecurityMode::None,
            UserTokenPolicy::anonymous(),
        )
            .into();

        // GetEndpoints + 会话建立整体限时（GetEndpoints 库内无超时，防止挂死）。
        let (session, event_loop) = timeout(
            self.config.request_timeout,
            client.connect_to_matching_endpoint(endpoint, identity),
        )
        .await
        .map_err(|_| {
            DaemonError::NetworkError(format!("opc ua connect {url} timed out (get endpoints)"))
        })?
        .map_err(|e| DaemonError::NetworkError(format!("opc ua connect {url}: {e}")))?;
        let handle = event_loop.spawn();

        let connected = timeout(self.config.request_timeout, session.wait_for_connection()).await;
        match connected {
            Ok(true) => {
                // 会话已激活：关闭事件循环的自动重连，退避策略归零由 connect() 处理。
                session.disable_reconnects();
                self.session = Some(session);
                self.event_loop = Some(handle);
                Ok(())
            }
            Ok(false) => {
                handle.abort();
                Err(DaemonError::NetworkError(format!(
                    "opc ua session failed to connect {url} (event loop ended)"
                )))
            }
            Err(_) => {
                handle.abort();
                Err(DaemonError::NetworkError(format!(
                    "opc ua session connect {url} timed out"
                )))
            }
        }
    }

    /// 确保已连接（惰性连接：未连接时先 `connect()`）。
    async fn ensure_connected(&mut self) -> DaemonResult<()> {
        if self.session.is_none() {
            self.connect().await?;
        }
        Ok(())
    }

    /// 在会话上执行一次服务调用（无 IO 超时；超时由调用方 `timeout` 包装）。
    async fn execute(session: &Session, op: &OpcuaOp) -> Result<OpcuaOutcome, RequestError> {
        match op {
            OpcuaOp::Read(nodes) => {
                // 客户端层 Err 视为连接丢失（传输 / 会话问题），交由重连路径处理；
                // 节点级错误通过 DataValue.status 判定，属终端协议错误。
                let values = session
                    .read(nodes, TimestampsToReturn::Both, 0.0)
                    .await
                    .map_err(|e| RequestError(e.to_string()))?;
                Ok(OpcuaOutcome::Read(values))
            }
            OpcuaOp::Write(nodes) => {
                let results = session
                    .write(nodes)
                    .await
                    .map_err(|e| RequestError(e.to_string()))?;
                Ok(OpcuaOutcome::Written(results))
            }
            OpcuaOp::Browse(descriptions) => {
                let results = session
                    .browse(descriptions, BROWSE_MAX_REFERENCES, None)
                    .await
                    .map_err(|e| RequestError(e.to_string()))?;
                Ok(OpcuaOutcome::Browsed(results))
            }
        }
    }

    /// 处理请求失败：终端错误直接返回；连接丢失走退避 → 重连 → 重试一次。
    async fn recover(&mut self, op: OpcuaOp) -> DaemonResult<OpcuaOutcome> {
        // 1. 丢弃旧会话，按退避等待。
        self.teardown();
        let delay = self.reconnector.next_delay();
        tokio::time::sleep(delay).await;
        // 2. 重连。
        if let Err(e) = self.open().await {
            self.reconnector.next_delay();
            return Err(e);
        }
        // 3. 重试一次。
        let timeout_dur = self.config.request_timeout;
        let result = match &self.session {
            Some(session) => timeout(timeout_dur, Self::execute(session, &op)).await,
            None => {
                self.reconnector.next_delay();
                return Err(DaemonError::NetworkError(
                    "opc ua reconnect produced no session".to_string(),
                ));
            }
        };
        match result {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(e)) => {
                self.reconnector.next_delay();
                Err(e.finalize())
            }
            Err(_) => {
                self.reconnector.next_delay();
                Err(DaemonError::ProtocolError(
                    "opc ua request timeout after reconnect".to_string(),
                ))
            }
        }
    }

    /// 在已连接会话上执行请求；连接丢失时按退避重连并重试一次。
    async fn run_request(&mut self, op: OpcuaOp) -> DaemonResult<OpcuaOutcome> {
        self.ensure_connected().await?;
        let timeout_dur = self.config.request_timeout;
        let result = match &self.session {
            Some(session) => timeout(timeout_dur, Self::execute(session, &op)).await,
            None => {
                return Err(DaemonError::ProtocolError(
                    "opc ua driver not connected".to_string(),
                ))
            }
        };
        match result {
            Ok(Ok(outcome)) => Ok(outcome),
            // 首次失败的明细被重试结果覆盖（重试结果才是最终状态），故不保留。
            Ok(Err(_first)) => self.recover(op).await,
            Err(_) => self.recover(op).await,
        }
    }

    /// 检查 Read 逐节点结果并编码为小端原始字节。
    fn decode_read_values(
        points: &[ReadPoint],
        values: Vec<DataValue>,
    ) -> DaemonResult<Vec<PointSample>> {
        if values.len() != points.len() {
            return Err(DaemonError::ProtocolError(format!(
                "opc ua read returned {} values for {} points",
                values.len(),
                points.len()
            )));
        }
        let mut samples = Vec::with_capacity(points.len());
        for (p, dv) in points.iter().zip(values) {
            if let Some(status) = dv.status {
                if !status.is_good() {
                    return Err(DaemonError::ProtocolError(format!(
                        "opc ua read node ns={};i={} failed: {status}",
                        p.address.db, p.address.start
                    )));
                }
            }
            let variant = dv.value.ok_or_else(|| {
                DaemonError::ProtocolError(format!(
                    "opc ua read node ns={};i={} returned no value",
                    p.address.db, p.address.start
                ))
            })?;
            let bytes = variant_to_bytes(&variant).ok_or_else(|| {
                DaemonError::ProtocolError(format!(
                    "opc ua node ns={};i={} value type {variant:?} not supported for byte mapping",
                    p.address.db, p.address.start
                ))
            })?;
            samples.push(PointSample {
                address: p.address.clone(),
                value: bytes,
            });
        }
        Ok(samples)
    }

    /// 读取目标节点当前值以确定写入类型，并将原始字节解码为同类型 Variant。
    async fn decode_write_value(&mut self, p: &WritePoint) -> DaemonResult<WriteValue> {
        let node = validate_node_address(&ReadPoint {
            address: p.address.clone(),
            count: 1,
        })?;
        let read = ReadValueId::new_value(node.clone());
        let outcome = self
            .run_request(OpcuaOp::Read(vec![read]))
            .await
            .map_err(|e| {
                DaemonError::ProtocolError(format!(
                    "opc ua write: cannot determine type of ns={};i={}: {e}",
                    p.address.db, p.address.start
                ))
            })?;
        let OpcuaOutcome::Read(values) = outcome else {
            return Err(DaemonError::ProtocolError(
                "opc ua write: unexpected read outcome".to_string(),
            ));
        };
        let current = values.into_iter().next().ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "opc ua write: no current value for ns={};i={}",
                p.address.db, p.address.start
            ))
        })?;
        if let Some(status) = current.status {
            if !status.is_good() {
                return Err(DaemonError::ProtocolError(format!(
                    "opc ua write: read current value of ns={};i={} failed: {status}",
                    p.address.db, p.address.start
                )));
            }
        }
        let variant = current.value.ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "opc ua write: node ns={};i={} has no current value to infer type",
                p.address.db, p.address.start
            ))
        })?;
        let ty = variant.scalar_type_id().ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "opc ua write: node ns={};i={} value {variant:?} is not a scalar",
                p.address.db, p.address.start
            ))
        })?;
        let new_value = bytes_to_variant(&p.value, ty).ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "opc ua write: {} bytes do not match node type {ty:?} of ns={};i={}",
                p.value.len(),
                p.address.db,
                p.address.start
            ))
        })?;
        Ok(WriteValue {
            node_id: node,
            attribute_id: AttributeId::Value as u32,
            index_range: Default::default(),
            value: DataValue::new_now(new_value),
        })
    }
}

#[async_trait]
impl Driver for OpcuaDriver {
    async fn connect(&mut self) -> DaemonResult<()> {
        match self.open().await {
            Ok(()) => {
                self.reconnector.reset();
                Ok(())
            }
            Err(e) => {
                self.reconnector.next_delay();
                Err(e)
            }
        }
    }

    async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>> {
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let mut nodes = Vec::with_capacity(points.len());
        for p in points {
            nodes.push(ReadValueId::new_value(validate_node_address(p)?));
        }
        match self.run_request(OpcuaOp::Read(nodes)).await? {
            OpcuaOutcome::Read(values) => Self::decode_read_values(points, values),
            _ => Err(DaemonError::ProtocolError(
                "opc ua read got unexpected outcome".to_string(),
            )),
        }
    }

    async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut nodes = Vec::with_capacity(points.len());
        for p in points {
            nodes.push(self.decode_write_value(p).await?);
        }
        match self.run_request(OpcuaOp::Write(nodes)).await? {
            OpcuaOutcome::Written(results) => {
                for (i, status) in results.iter().enumerate() {
                    if !status.is_good() {
                        return Err(DaemonError::ProtocolError(format!(
                            "opc ua write node {i} failed: {status}"
                        )));
                    }
                }
                Ok(())
            }
            _ => Err(DaemonError::ProtocolError(
                "opc ua write got unexpected outcome".to_string(),
            )),
        }
    }

    async fn disconnect(&mut self) -> DaemonResult<()> {
        if let Some(session) = self.session.take() {
            let handle = self.event_loop.take();
            let dur = self.config.request_timeout;
            // 优雅关闭会话；超时 / 失败均不视为错误（通道即将释放）。
            let _ = timeout(dur, session.disconnect()).await;
            if let Some(handle) = handle {
                handle.abort();
            }
        }
        Ok(())
    }
}

impl OpcuaDriver {
    /// 浏览节点树摘要：返回 `parent` 正向层级引用下的节点列表。
    ///
    /// `parent` 为 `None` 时从 ObjectsFolder（`ns=0;i=85`）开始；
    /// 入参形式与点位地址一致（`(命名空间, 数值节点标识)`）。
    /// 树摘要起步不做 browse_next 续传，单节点引用数以
    /// [`BROWSE_MAX_REFERENCES`] 为上限。
    pub async fn browse(
        &mut self,
        parent: Option<(u16, u32)>,
    ) -> DaemonResult<Vec<OpcuaNodeSummary>> {
        let (ns, id) = parent.unwrap_or((0, OBJECTS_FOLDER_ID));
        let description = BrowseDescription {
            node_id: NodeId::new(ns, id),
            browse_direction: BrowseDirection::Forward,
            reference_type_id: NodeId::from(ReferenceTypeId::HierarchicalReferences),
            include_subtypes: true,
            node_class_mask: (NodeClassMask::OBJECT
                | NodeClassMask::VARIABLE
                | NodeClassMask::OBJECT_TYPE)
                .bits(),
            result_mask: BrowseDescriptionResultMask::all().bits(),
        };
        match self.run_request(OpcuaOp::Browse(vec![description])).await? {
            OpcuaOutcome::Browsed(results) => {
                let mut summary = Vec::new();
                for result in results {
                    if !result.status_code.is_good() {
                        return Err(DaemonError::ProtocolError(format!(
                            "opc ua browse ns={ns};i={id} failed: {}",
                            result.status_code
                        )));
                    }
                    for r in result.references.unwrap_or_default() {
                        summary.push(OpcuaNodeSummary {
                            node_id: r.node_id.node_id.to_string(),
                            browse_name: r.browse_name.name.to_string(),
                            display_name: r.display_name.text.to_string(),
                        });
                    }
                }
                Ok(summary)
            }
            _ => Err(DaemonError::ProtocolError(
                "opc ua browse got unexpected outcome".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;

    use async_trait::async_trait;
    use opcua_core::sync::RwLock;
    use opcua_server::node_manager::memory::{InMemoryNodeManagerBuilder, InMemoryNodeManagerImpl};
    use opcua_server::{
        address_space::{write_node_value, AccessLevel, AddressSpace, Variable},
        diagnostics::NamespaceMetadata,
        node_manager::{RequestContext, ServerContext, WriteNode},
        ServerBuilder, ServerHandle,
    };
    use opcua_types::{AttributeId, NodeClass, ObjectId, QualifiedName, StatusCode};

    use crate::driver::PointAddress;
    use crate::error::{ERR_NETWORK, ERR_PROTOCOL};

    /// 测试命名空间 URI 与索引（0 = OPC UA 标准库，1 = 服务器本地，2 = 测试）。
    const TEST_NS: u16 = 2;
    const TEST_NS_URI: &str = "urn:iot-daq:test";

    /// 测试节点数值标识（ns=2 命名空间内）。
    const DEMO_FOLDER_ID: u32 = 1000;
    const DEMO_INT32_ID: u32 = 1001;
    const DEMO_BOOL_ID: u32 = 1002;
    const DEMO_DOUBLE_ID: u32 = 1003;

    /// 空实现的内存节点管理器（地址空间在 builder 闭包中填充）。
    ///
    /// 只覆写 `write`：默认实现恒返回 `BadServiceUnsupported`，测试便无法覆盖
    /// 写入链路。此处镜像 `SimpleNodeManagerImpl` 的写入语义（校验 → 写回节点
    /// 层次 → 逐节点置状态码），但不引入其回调与采样器机制。
    struct TestNodesImpl;

    #[async_trait]
    impl InMemoryNodeManagerImpl for TestNodesImpl {
        async fn init(&self, _address_space: &mut AddressSpace, _context: ServerContext) {}

        fn name(&self) -> &str {
            "iot-daq-test"
        }

        fn namespaces(&self) -> Vec<NamespaceMetadata> {
            vec![NamespaceMetadata {
                namespace_uri: TEST_NS_URI.to_string(),
                namespace_index: TEST_NS,
                ..Default::default()
            }]
        }

        /// Write 服务：逐节点校验可写性并写回地址空间。
        ///
        /// 与 `SimpleNodeManagerImpl` 一致：校验失败 / 非变量节点 / 非 Value 属性
        /// 一律置对应错误码，绝不中断整批（剩余节点仍需各自落状态码）。
        async fn write(
            &self,
            context: &RequestContext,
            address_space: &RwLock<AddressSpace>,
            nodes_to_write: &mut [&mut WriteNode],
        ) -> Result<(), StatusCode> {
            let mut space = address_space.write();
            let type_tree = context.type_tree.read();
            for node in nodes_to_write {
                let target = match space.validate_node_write(context, node.value(), &*type_tree) {
                    Ok(v) => v,
                    Err(e) => {
                        node.set_status(e);
                        continue;
                    }
                };
                if target.node_class() != NodeClass::Variable
                    || node.value().attribute_id != AttributeId::Value
                {
                    node.set_status(StatusCode::BadNotWritable);
                    continue;
                }
                match write_node_value(target, node.value()) {
                    Ok(()) => node.set_status(StatusCode::Good),
                    Err(e) => node.set_status(e),
                }
            }
            Ok(())
        }
    }

    /// 把测试变量置为可写。
    ///
    /// `access_level` 与 `user_access_level` 都要置位：服务端 `is_writable` 只
    /// 校验 `user_access_level`，漏置则会拿到 `BadUserAccessDenied`。
    fn writable(mut variable: Variable) -> Variable {
        variable.set_access_level(variable.access_level() | AccessLevel::CURRENT_WRITE);
        variable.set_user_access_level(variable.user_access_level() | AccessLevel::CURRENT_WRITE);
        variable
    }

    /// 同步测试骨架：独立多线程 runtime 运行异步体，`shutdown_timeout` 兜底销毁。
    ///
    /// async-opcua 的服务器任务长驻监听，若依赖 `#[tokio::test]` 的隐式 runtime
    /// 销毁，测试结束后进程可能被残留 socket 任务拖住无法退出（Windows 实测）；
    /// 显式 `shutdown_timeout` 保证进程确定性退出。panic 经 catch_unwind 捕获后
    /// 在 shutdown 完成时回传，保持测试失败语义。
    fn run_with_runtime<F, Fut>(body: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(body())));
        rt.shutdown_timeout(Duration::from_secs(3));
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// 启动内嵌示例 OPC UA 服务器（匿名 / SecurityPolicy None），
    /// 返回 (endpoint url, server handle)。
    ///
    /// 注意：端口经 std listener 试绑定后释放，由服务器自行 `run()` 绑定
    /// （Windows 上 `from_std` 注入的 listener 实测会导致连接处理任务无响应）。
    async fn spawn_test_server() -> (String, ServerHandle) {
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
            probe.local_addr().expect("local addr").port()
        };

        let manager =
            InMemoryNodeManagerBuilder::new(|context: ServerContext, space: &mut AddressSpace| {
                // 类型树与地址空间都要登记测试命名空间。
                {
                    let mut type_tree = context.type_tree.write();
                    type_tree.namespaces_mut().add_namespace(TEST_NS_URI);
                }
                space.add_namespace(TEST_NS_URI, TEST_NS);

                let folder = NodeId::new(TEST_NS, DEMO_FOLDER_ID);
                assert!(space.add_folder(
                    &folder,
                    QualifiedName::new(TEST_NS, "Demo"),
                    "Demo",
                    &NodeId::from(ObjectId::ObjectsFolder),
                ));
                // 写入用例覆盖 Int32 与 Bool → 变量须显式可写（`Variable` 默认
                // 只读，`validate_node_write` 会返回 BadUserAccessDenied）。
                // Double 保持只读，保留一个只读节点供后续用例。
                space.add_variables(
                    vec![
                        writable(Variable::new(
                            &NodeId::new(TEST_NS, DEMO_INT32_ID),
                            QualifiedName::new(TEST_NS, "DemoInt32"),
                            "DemoInt32",
                            42i32,
                        )),
                        writable(Variable::new(
                            &NodeId::new(TEST_NS, DEMO_BOOL_ID),
                            QualifiedName::new(TEST_NS, "DemoBool"),
                            "DemoBool",
                            true,
                        )),
                        Variable::new(
                            &NodeId::new(TEST_NS, DEMO_DOUBLE_ID),
                            QualifiedName::new(TEST_NS, "DemoDouble"),
                            "DemoDouble",
                            1.5f64,
                        ),
                    ],
                    &folder,
                );
                TestNodesImpl
            });

        let (server, handle) = ServerBuilder::new_anonymous("iot-daq test server")
            .host("127.0.0.1")
            .port(port)
            .with_node_manager(manager)
            .build()
            .expect("build test server");

        tokio::spawn(async move {
            let _ = server.run().await;
        });

        // 轮询等待监听就绪（最多 5s），避免 GetEndpoints 先于 accept 循环启动。
        let url = format!("opc.tcp://127.0.0.1:{port}/");
        for _ in 0..250 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return (url, handle);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("test server did not start listening within 5s: {url}");
    }

    /// 测试用驱动配置：临时 PKI 目录 + 短超时 + 毫秒级退避。
    fn test_config(url: &str) -> OpcuaConfig {
        let pki = tempfile::tempdir().expect("tempdir");
        let path = pki.path().to_path_buf();
        // TempDir 析构即删除目录，这里保活到测试结束：泄漏一次可接受（测试进程短生命周期）。
        std::mem::forget(pki);
        OpcuaConfig {
            endpoint_url: url.to_string(),
            pki_dir: Some(path),
            request_timeout: Duration::from_secs(5),
            reconnector: Reconnector::new(Duration::from_millis(1), Duration::from_millis(2), 2),
            ..Default::default()
        }
    }

    /// 构造点位地址（db=ns，start=数值节点标识）。
    fn node_point(ns: u16, id: u32) -> ReadPoint {
        ReadPoint {
            address: PointAddress {
                db: u32::from(ns),
                area: None,
                start: id,
                bit: false,
                bit_index: 0,
            },
            count: 1,
        }
    }

    fn write_point(ns: u16, id: u32, value: Vec<u8>) -> WritePoint {
        WritePoint {
            address: node_point(ns, id).address,
            value,
        }
    }

    // ---- happy path ----

    /// QA Happy: 连接内嵌示例服务器，读取 Int32 变量 → 42 的小端 4 字节。
    #[test]
    fn opcua_connect_and_read_int32() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");
            assert_eq!(
                driver.reconnector().clone().next_delay(),
                Duration::from_millis(1),
                "connect success resets backoff"
            );

            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("read");
            assert_eq!(samples.len(), 1);
            assert_eq!(
                samples[0].address,
                node_point(TEST_NS, DEMO_INT32_ID).address
            );
            assert_eq!(samples[0].value, 42i32.to_le_bytes(), "42 LE");

            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Happy: 写值后读回一致（Int32 与 Boolean 两种类型）。
    #[test]
    fn opcua_write_then_read_back() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");

            driver
                .write(&[write_point(
                    TEST_NS,
                    DEMO_INT32_ID,
                    100i32.to_le_bytes().to_vec(),
                )])
                .await
                .expect("write int32");
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("read back int32");
            assert_eq!(samples[0].value, 100i32.to_le_bytes());

            driver
                .write(&[write_point(TEST_NS, DEMO_BOOL_ID, vec![0x00])])
                .await
                .expect("write bool");
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_BOOL_ID)])
                .await
                .expect("read back bool");
            assert_eq!(samples[0].value, vec![0x00], "false = single 0 byte");

            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Happy: 批量读多个节点 → 等长采样、按下标对应。
    #[test]
    fn opcua_batch_read_multiple_nodes() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");

            let points = [
                node_point(TEST_NS, DEMO_INT32_ID),
                node_point(TEST_NS, DEMO_DOUBLE_ID),
                node_point(TEST_NS, DEMO_BOOL_ID),
            ];
            let samples = driver.read(&points).await.expect("batch read");
            assert_eq!(samples.len(), 3, "1:1 with request");
            assert_eq!(samples[0].value, 42i32.to_le_bytes());
            assert_eq!(samples[1].value, 1.5f64.to_le_bytes());
            assert_eq!(samples[2].value, vec![0x01]);

            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Happy: 浏览 ObjectsFolder 能发现测试文件夹，浏览文件夹能列出 3 个变量。
    #[test]
    fn opcua_browse_folder_summary() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");

            let root = driver.browse(None).await.expect("browse root");
            assert!(
                root.iter().any(|n| n.browse_name == "Demo"),
                "root must contain Demo folder: {root:?}"
            );

            let children = driver
                .browse(Some((TEST_NS, DEMO_FOLDER_ID)))
                .await
                .expect("browse demo folder");
            let mut names: Vec<&str> = children.iter().map(|n| n.browse_name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["DemoBool", "DemoDouble", "DemoInt32"],
                "folder children summary"
            );
            assert!(
                children.iter().all(|n| n.node_id.starts_with("ns=2;i=")),
                "node_id uses standard string encoding: {children:?}"
            );

            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Happy: 未连接即 read（惰性连接）→ 自动建立会话并成功。
    #[test]
    fn opcua_lazy_connect_on_read() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            // 不调用 connect()，直接读。
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("lazy read");
            assert_eq!(samples[0].value, 42i32.to_le_bytes());
            driver.disconnect().await.expect("disconnect");
        });
    }

    // ---- error path ----

    /// QA Error: 读不存在的节点 → 服务器返回节点级 Bad 状态码 → ProtocolError(1000)。
    #[test]
    fn opcua_read_unknown_node_is_protocol_error() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");

            let err = driver
                .read(&[node_point(TEST_NS, 9999)])
                .await
                .expect_err("unknown node must fail");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
            assert_eq!(err.error_code(), ERR_PROTOCOL);
            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Error: 写入字节长度与节点类型不符 → ProtocolError(1000)。
    #[test]
    fn opcua_write_bad_length_is_protocol_error() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect");

            // Int32 节点写入 3 字节 → 解码失败。
            let err = driver
                .write(&[write_point(TEST_NS, DEMO_INT32_ID, vec![0x01, 0x02, 0x03])])
                .await
                .expect_err("bad length must fail");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
            assert_eq!(err.error_code(), ERR_PROTOCOL);
            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Error: 无效 Endpoint URL（非 opc.tcp）→ ProtocolError(1000)，不触网。
    #[test]
    fn opcua_invalid_endpoint_url_is_protocol_error() {
        run_with_runtime(|| async {
            let mut driver = OpcuaDriver::new(test_config("http://127.0.0.1:4840"));
            let err = driver.connect().await.expect_err("must reject url");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
            assert_eq!(err.error_code(), ERR_PROTOCOL);
            assert!(err.to_string().contains("http://"), "message keeps url");
        });
    }

    /// QA Error: 连接不可达端口 → GetEndpoints 失败 → NetworkError(6000)，退避推进。
    #[test]
    fn opcua_unreachable_server_is_network_error() {
        run_with_runtime(|| async {
            // 使用保留端口（无监听），GetEndpoints 立即失败。
            let mut driver = OpcuaDriver::new(test_config("opc.tcp://127.0.0.1:9/"));
            let err = driver.connect().await.expect_err("must fail");
            assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
            assert_eq!(err.error_code(), ERR_NETWORK);
            assert_eq!(
                driver.reconnector().clone().next_delay(),
                Duration::from_millis(2),
                "connect failure advances backoff"
            );
        });
    }

    /// QA Error: 本地点位校验——bit 访问 / count != 1 / area 非法 → ProtocolError(1000)，不触网。
    #[test]
    fn opcua_rejects_bad_point_addresses() {
        run_with_runtime(|| async {
            let mut driver = OpcuaDriver::new(test_config("opc.tcp://127.0.0.1:9/"));

            let mut bad_bit = node_point(TEST_NS, DEMO_INT32_ID);
            bad_bit.address.bit = true;
            let err = driver.read(&[bad_bit]).await.expect_err("bit must fail");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
            assert_eq!(err.error_code(), ERR_PROTOCOL);

            let mut bad_count = node_point(TEST_NS, DEMO_INT32_ID);
            bad_count.count = 2;
            let err = driver
                .read(&[bad_count])
                .await
                .expect_err("count must fail");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");

            let mut bad_area = node_point(TEST_NS, DEMO_INT32_ID);
            bad_area.address.area = Some('D');
            let err = driver.read(&[bad_area]).await.expect_err("area must fail");
            assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        });
    }

    // ---- trait 对象与生命周期 ----

    /// QA Happy: Driver trait 对象路径（Box<dyn Driver>）端到端跑通。
    #[test]
    fn opcua_driver_trait_object_end_to_end() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver: Box<dyn Driver> = Box::new(OpcuaDriver::new(test_config(&url)));
            driver.connect().await.expect("connect");
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("read via dyn");
            assert_eq!(samples[0].value, 42i32.to_le_bytes());
            driver
                .write(&[write_point(
                    TEST_NS,
                    DEMO_INT32_ID,
                    7i32.to_le_bytes().to_vec(),
                )])
                .await
                .expect("write via dyn");
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("read back via dyn");
            assert_eq!(samples[0].value, 7i32.to_le_bytes());
            driver.disconnect().await.expect("disconnect");
        });
    }

    /// QA Happy: disconnect 后可再次 connect 并正常读（会话生命周期闭环）。
    #[test]
    fn opcua_reconnect_after_disconnect() {
        run_with_runtime(|| async {
            let (url, _handle) = spawn_test_server().await;
            let mut driver = OpcuaDriver::new(test_config(&url));
            driver.connect().await.expect("connect 1");
            driver.disconnect().await.expect("disconnect");

            driver.connect().await.expect("connect 2");
            let samples = driver
                .read(&[node_point(TEST_NS, DEMO_INT32_ID)])
                .await
                .expect("read after reconnect");
            assert_eq!(samples[0].value, 42i32.to_le_bytes());
            driver.disconnect().await.expect("disconnect");
        });
    }

    // ================= QA 独立验收（task 10 边界 / 反例） =================

    /// QA 边界：Variant ↔ 小端字节 的极值往返（含 `i64::MIN` / `u64::MAX` / 负零），
    /// 未支持类型返回 `None` 而非 panic。
    #[test]
    fn qa_variant_byte_mapping_roundtrip_and_unsupported() {
        let cases: Vec<(Variant, Vec<u8>)> = vec![
            (Variant::Boolean(false), vec![0u8]),
            (Variant::Boolean(true), vec![1u8]),
            (Variant::SByte(i8::MIN), vec![0x80u8]),
            (Variant::Byte(u8::MAX), vec![0xFFu8]),
            (Variant::Int16(i16::MIN), i16::MIN.to_le_bytes().to_vec()),
            (Variant::UInt16(u16::MAX), u16::MAX.to_le_bytes().to_vec()),
            (Variant::Int32(i32::MIN), i32::MIN.to_le_bytes().to_vec()),
            (Variant::UInt32(u32::MAX), u32::MAX.to_le_bytes().to_vec()),
            (Variant::Int64(i64::MIN), i64::MIN.to_le_bytes().to_vec()),
            (Variant::UInt64(u64::MAX), u64::MAX.to_le_bytes().to_vec()),
            (Variant::Float(-0.0), (-0.0f32).to_le_bytes().to_vec()),
            (Variant::Double(-0.0), (-0.0f64).to_le_bytes().to_vec()),
        ];
        for (variant, bytes) in cases {
            assert_eq!(
                variant_to_bytes(&variant),
                Some(bytes.clone()),
                "{variant:?}"
            );
            let ty = variant.scalar_type_id().expect("scalar type id");
            assert_eq!(
                bytes_to_variant(&bytes, ty),
                Some(variant.clone()),
                "{variant:?} 往返失败"
            );
        }
        for unsupported in [Variant::Empty, Variant::StatusCode(StatusCode::Good)] {
            assert!(
                variant_to_bytes(&unsupported).is_none(),
                "{unsupported:?} 应不支持字节映射"
            );
        }
    }

    /// QA 反例：`bytes_to_variant` 必须**严格等长**，不做截断 / 补零；布尔仅接受 0 / 1。
    #[test]
    fn qa_bytes_to_variant_is_strict() {
        // 布尔：只接受 [0] / [1]。
        assert_eq!(
            bytes_to_variant(&[0], VariantScalarTypeId::Boolean),
            Some(Variant::Boolean(false))
        );
        assert_eq!(
            bytes_to_variant(&[1], VariantScalarTypeId::Boolean),
            Some(Variant::Boolean(true))
        );
        assert!(bytes_to_variant(&[2], VariantScalarTypeId::Boolean).is_none());
        assert!(bytes_to_variant(&[], VariantScalarTypeId::Boolean).is_none());
        assert!(bytes_to_variant(&[0, 0], VariantScalarTypeId::Boolean).is_none());
        // 定长类型：少一字节 / 多一字节都必须拒绝。
        assert!(bytes_to_variant(&[0], VariantScalarTypeId::Int16).is_none());
        assert!(bytes_to_variant(&[0, 0, 0], VariantScalarTypeId::Int16).is_none());
        assert!(bytes_to_variant(&[0; 7], VariantScalarTypeId::Double).is_none());
        assert!(bytes_to_variant(&[0; 9], VariantScalarTypeId::UInt64).is_none());
        assert!(bytes_to_variant(&[], VariantScalarTypeId::Float).is_none());
    }

    /// QA 反例：`decode_read_values` 的数量不符 / 节点级坏状态码 / 无值都必须报 `ProtocolError`，
    /// 不得静默降级为「好值」。
    #[test]
    fn qa_decode_read_values_guards() {
        let points = vec![node_point(TEST_NS, DEMO_INT32_ID)];
        // 条数不符。
        let err = OpcuaDriver::decode_read_values(&points, Vec::new()).expect_err("count mismatch");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        let dv = |value: Option<Variant>, status: Option<StatusCode>| DataValue {
            value,
            status,
            source_timestamp: None,
            source_picoseconds: None,
            server_timestamp: None,
            server_picoseconds: None,
        };

        // 节点级坏状态码（即便带值）。
        let err = OpcuaDriver::decode_read_values(
            &points,
            vec![dv(
                Some(Variant::Int32(1)),
                Some(StatusCode::BadNodeIdUnknown),
            )],
        )
        .expect_err("bad status");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        // 无值。
        let err = OpcuaDriver::decode_read_values(&points, vec![dv(None, Some(StatusCode::Good))])
            .expect_err("no value");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        // 未支持类型（非标量）。
        let err = OpcuaDriver::decode_read_values(&points, vec![dv(Some(Variant::Empty), None)])
            .expect_err("unsupported type");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        // 正向：好状态 + 有值 → 小端字节。
        let ok = OpcuaDriver::decode_read_values(
            &points,
            vec![dv(Some(Variant::Int32(258)), Some(StatusCode::Good))],
        )
        .expect("ok");
        assert_eq!(ok[0].value, 258i32.to_le_bytes());
    }

    /// QA 回归（**RED**）：命名空间索引 `db` 为 `u32`，但 OPC UA 的 ns 是 `u16`。
    /// 当前 `addr.db as u16` **静默截断**（65537 → 1），会把读 / 写悄悄指向**另一个节点**。
    /// 工业场景下这是安全隐患，必须显式拒绝（`db > u16::MAX` ⇒ `ProtocolError`）。
    #[test]
    fn qa_namespace_index_beyond_u16_must_be_rejected() {
        for db in [0x1_0000u32, 0x1_0001, u32::MAX] {
            let mut p = node_point(TEST_NS, DEMO_INT32_ID);
            p.address.db = db;
            assert!(
                validate_node_address(&p).is_err(),
                "ns={db} 超出 u16 必须报错，不得静默截断到 {}",
                db as u16
            );
        }
        // u16::MAX 是合法上界。
        let mut ok = node_point(TEST_NS, DEMO_INT32_ID);
        ok.address.db = u32::from(u16::MAX);
        assert!(validate_node_address(&ok).is_ok());
    }
}
