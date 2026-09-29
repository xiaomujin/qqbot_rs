//! 群聊管理接口（`/v2/groups/...`）的请求 / 响应类型。
//!
//! 纯类型 / 纯函数，**不含 IO**。HTTP 调用在 `client.rs`。
//!
//! # 覆盖的接口
//!
//! | 方法 | 路径 | 主要类型 |
//! | --- | --- | --- |
//! | GET | `/v2/groups/{group_openid}/info` | [`GroupInfo`] |
//! | GET | `/v2/groups/{group_openid}/bot_state` | [`BotState`] |
//! | GET | `/v2/groups/{group_openid}/join_request_list` | [`JoinRequestListQuery`] / [`JoinRequestListResponse`] |
//! | POST | `/v2/groups/{group_openid}/approval_join_request/{member_openid}` | [`ApprovalJoinRequest`] |
//! | GET | `/v2/groups/{group_openid}/restrict_chat_setting` | [`RestrictChatSetting`] |
//! | POST | `/v2/groups/{group_openid}/restrict_chat_setting` | [`SetRestrictChatSettingRequest`] |
//! | GET | `/v2/groups/join_approval_strategy` | [`JoinApprovalStrategyListQuery`] / [`JoinApprovalStrategyListResponse`] |
//! | POST | `/v2/groups/join_approval_strategy` | [`CreateStrategyRequest`] / [`CreateStrategyResponse`] |
//! | PATCH | `/v2/groups/join_approval_strategy/{strategy_id}` | [`UpdateStrategyRequest`] / [`UpdateStrategyResponse`] |
//! | DELETE | `/v2/groups/join_approval_strategy/{strategy_id}` | 无请求体 / 无响应体 |
//! | POST | `/v2/groups/join_approval_strategy/{strategy_id}/execute` | [`ExecuteStrategyRequest`] |
//! | POST | `/v2/groups/join_approval_strategy/{strategy_id}/whitelist_users` | [`WhitelistUsersRequest`] / [`WhitelistUsersResponse`] |
//! | GET | `/v2/groups/{group_openid}/members` | [`GroupMemberListQuery`] / [`GroupMemberListResponse`] |
//! | GET | `/v2/groups/{group_openid}/members/{member_openid}` | [`GroupMember`] |
//! | POST | `/v2/groups/{group_openid}/batch_remove_members` | [`BatchRemoveMembersRequest`] / [`BatchRemoveMembersResponse`] |
//! | GET | `/v2/groups/{group_openid}/member_blacklist` | [`MemberBlacklistQuery`] / [`MemberBlacklist`] |
//! | POST | `/v2/groups/{group_openid}/member_blacklist` | [`MemberBlacklistRequest`] / [`MemberBlacklistOpResponse`] |
//!
//! # 字段约定
//!
//! - 字段名与官方 JSON **完全一致**（官方本身就是 snake_case，因此不需要额外
//!   `rename`）；枚举/联合类型做特殊处理时会写明。
//! - 请求体 derive `Serialize`（需要读回时同时 derive `Deserialize`）；
//!   响应体 derive `Debug, Clone, Default, Deserialize`。
//! - 响应里的普通字符串用 `String` + `#[serde(default)]`（缺失即空串）；
//!   文档标注「如有 / 仅 … 时有效 / 可能不返回」的字段用 `Option<String>`。
//! - 响应里的**枚举**字段包一层 `Option`：既容忍字段缺失，也不会用某个默认值把
//!   「未知」误判成一个具体取值。
//! - 数组一律 `Vec<T>` + `#[serde(default)]`，缺字段即空数组。
//! - 分页接口的 `cursor` 首次请求可不传（或传空串），后续传上一次响应的
//!   `next_cursor`；**`next_cursor` 为空串表示已到末页**。
//! - 官方文档列全了取值的枚举（封闭集合）不加 `#[serde(other)]` 兜底：本模块里
//!   所有枚举都是这种「表格列全取值」的封闭集合。
//!
//! # 响应为空的接口
//!
//! 入群申请审批、设置群成员禁言、删除策略、执行策略的响应体都是 `{}`（无内容），
//! 因此本模块不为它们定义响应类型；调用方用 `()` 或 `serde_json::Value` 接收即可。


use serde::{Deserialize, Serialize};

// ==================== 获取群基本信息 ====================

/// 获取群基本信息（`GET /v2/groups/{group_openid}/info`，30 QPM）的响应。
///
/// 坑：该接口仅白名单机器人可用，未申请权限时返回业务错误码 `11253`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupInfo {
    /// 群 OpenID。
    #[serde(default)]
    pub group_openid: String,
    /// 群名称。
    #[serde(default)]
    pub group_name: String,
    /// 群简介。
    #[serde(default)]
    pub group_finger_memo: String,
    /// 群分类（文本，如「文化」），不是分类 ID。
    #[serde(default)]
    pub group_class_text: String,
    /// 群标签列表；官方可能返回空数组。
    #[serde(default)]
    pub group_tags: Vec<String>,
    /// 群成员人数。
    #[serde(default)]
    pub group_member_num: u32,
}

// ==================== 获取机器人群内状态 ====================

/// 获取机器人在指定群中的状态（`GET /v2/groups/{group_openid}/bot_state`，30 QPM）。
///
/// ⚠️ 官方响应示例里 `member_role` 的收尾引号是漏的（`\"member` 未闭合），属于文档
/// 笔误；真实响应是合法 JSON。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BotState {
    /// 机器人自己的 openid。
    #[serde(default)]
    pub member_openid: String,
    /// 入群时间戳（RFC3339 格式）。
    #[serde(default)]
    pub joined_at: String,
    /// 是否接收主动推送。`true` 表示接受主动推送。
    #[serde(default)]
    pub allow_proactive_msg: bool,
    /// 群内接收消息的设置，见 [`RecvMsgSetting`]。
    #[serde(default)]
    pub recv_msg_setting: Option<RecvMsgSetting>,
    /// 机器人在群内的角色，见 [`MemberRole`]。
    #[serde(default)]
    pub member_role: Option<MemberRole>,
}

/// 群内接收消息的设置（`recv_msg_setting`）。官方文档列全了三种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecvMsgSetting {
    /// `all` — 接收群内全部消息。
    All,
    /// `only_mention` — 仅接收 @ 机器人的消息。
    OnlyMention,
    /// `mention_and_context` — 接收 @ 机器人以及上下文相关消息。
    MentionAndContext,
}

/// 群成员角色（`member_role`）。官方文档列全了三种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// `member` — 普通成员。
    Member,
    /// `owner` — 群主。
    Owner,
    /// `admin` — 管理员。
    Admin,
}

impl MemberRole {
    /// 群主或管理员。
    ///
    /// 官方只写「需拥有群管理员身份」，未说明群主是否自动具备该身份；这里按
    /// 「群主或管理员」处理，调用方可按需改用 `self == MemberRole::Admin`。
    pub const fn is_privileged(self) -> bool {
        matches!(self, MemberRole::Owner | MemberRole::Admin)
    }
}

// ==================== 入群申请列表拉取 ====================

/// 入群申请列表的查询参数（`GET /v2/groups/{group_openid}/join_request_list`，30 QPM）。
///
/// 机器人需拥有群管理员身份。
///
/// - `cursor`：分页游标，首次请求可不传或传空串。
/// - `limit`：单页数量，**默认 20，最大 50**（官方未说明超限行为，[`Self::with_limit`]
///   会自行夹住）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JoinRequestListQuery {
    /// 分页游标；不传时该字段不会出现在查询串里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 单页数量；不传由服务端按默认值 20 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl JoinRequestListQuery {
    /// 官方默认单页数量。
    pub const DEFAULT_LIMIT: u32 = 20;
    /// 官方单页数量上限。
    pub const MAX_LIMIT: u32 = 50;

    /// 设置游标。
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// 设置单页数量；超过官方上限时**夹到上限**，低于 1 时夹到 1。
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit.clamp(1, Self::MAX_LIMIT));
        self
    }
}

/// 入群申请列表的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JoinRequestListResponse {
    /// 入群申请列表。
    #[serde(default)]
    pub list: Vec<JoinRequest>,
    /// 下一页游标；**空串表示已到末页**。
    #[serde(default)]
    pub next_cursor: String,
}

impl JoinRequestListResponse {
    /// 是否已经拉到末页。
    pub fn is_last_page(&self) -> bool {
        self.next_cursor.is_empty()
    }
}

/// 一条入群申请。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JoinRequest {
    /// 申请 ID，**审批时必须回传**（见 [`ApprovalJoinRequest::join_request_id`]）。
    #[serde(default)]
    pub join_request_id: String,
    /// 安全提示语：可疑消息直接返回 `warning_tips`；普通消息命中 `sec_risk_rules` 时
    /// 返回 `top_tips`；无提示时为空串。
    #[serde(default)]
    pub risk_tips: String,
    /// 用户在应用 / 开放平台下的统一标识（**可能没有**，官方标注「如有」）。
    #[serde(default)]
    pub union_openid: Option<String>,
    /// 申请人 openid。
    #[serde(default)]
    pub member_openid: String,
    /// 申请人昵称。
    #[serde(default)]
    pub username: String,
    /// 申请时间戳（RFC3339 格式）。
    #[serde(default)]
    pub apply_at: String,
    /// 申请来源，见 [`ApplySource`]。
    #[serde(default)]
    pub apply_source: Option<ApplySource>,
    /// 邀请人 openid；**仅 `apply_source = invited` 时有效**，其余情况是空串或缺失。
    #[serde(default)]
    pub invited_by: Option<String>,
    /// 申请人是否为机器人账号。
    #[serde(default)]
    pub bot: bool,
    /// 用户的入群验证方式，见 [`VerifyInfo`]。
    #[serde(default)]
    pub verify_info: Option<VerifyInfo>,
}

/// 申请来源（`apply_source`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplySource {
    /// `self_apply` — 主动申请。
    SelfApply,
    /// `invited` — 被邀请。
    Invited,
}

/// 用户入群验证方式（`verify_info`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VerifyInfo {
    /// 入群验证方式，见 [`VerifyMethod`]。
    #[serde(default)]
    pub method: Option<VerifyMethod>,
    /// 验证消息内容；**仅 `method = verify_message` 时可能携带**（官方描述里写成
    /// `auth_type`，与 `method` 是同一个字段）。
    #[serde(default)]
    pub verify_message: Option<String>,
    /// 问答列表；**仅 `method = admin_review_qa` 时可能携带**。
    #[serde(default)]
    pub review_qa_list: Vec<ReviewQA>,
}

/// 入群验证方式（`method`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMethod {
    /// `verify_message` — 验证消息。
    VerifyMessage,
    /// `admin_review_qa` — 管理员问答审核。
    AdminReviewQa,
}

/// 入群问答审核的一问一答。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReviewQA {
    /// 管理员设置的问题。
    #[serde(default)]
    pub question: String,
    /// 申请人填写的答案。
    #[serde(default)]
    pub answer: String,
}

// ==================== 入群申请审批 ====================

/// 入群申请审批请求体
/// （`POST /v2/groups/{group_openid}/approval_join_request/{member_openid}`，60 QPM）。
///
/// 机器人需拥有群管理员身份。`group_openid` 与 `member_openid` 都是**路径参数**，
/// 不在请求体里。
///
/// 坑（逐字段核对过）：
///
/// - 动作字段名是 **`op`**（不是 `action`），取值 `approve` / `decline`。官方描述里
///   `add_to_member_blacklist` 写的是「`action=decline` 时可填」，那个 `action` 指的就是 `op`。
/// - `join_request_id` 在表格里标「否」，但官方两个请求示例都带了它；建议把列表接口
///   拿到的 `join_request_id` 原样回传，否则审批可能定位不到申请。
/// - `reject_reason` **仅 `op = decline` 时有意义**。
/// - `add_to_member_blacklist = true` 会在拒绝的同时**把申请人加入群黑名单**，
///   默认 `false`。
///
/// 响应为空（`{}`），因此没有对应的响应类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalJoinRequest {
    /// 审批动作，见 [`ApprovalOp`]。
    pub op: ApprovalOp,
    /// 申请 ID，来自入群申请列表的 `join_request_id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_request_id: Option<String>,
    /// 拒绝理由；**仅 `op = decline` 时可填**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    /// 是否同时加入群黑名单，默认 `false`；**仅拒绝时有意义**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_to_member_blacklist: Option<bool>,
}

/// 审批动作（`op`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOp {
    /// `approve` — 通过。
    Approve,
    /// `decline` — 拒绝。
    Decline,
}

impl ApprovalJoinRequest {
    /// 通过入群申请。
    pub fn approve(join_request_id: impl Into<String>) -> Self {
        Self {
            op: ApprovalOp::Approve,
            join_request_id: Some(join_request_id.into()),
            reject_reason: None,
            add_to_member_blacklist: None,
        }
    }

    /// 拒绝入群申请；`reject_reason` 传空串按「未填」处理（该字段不会出现在请求体里）。
    pub fn decline(join_request_id: impl Into<String>, reject_reason: impl Into<String>) -> Self {
        Self {
            op: ApprovalOp::Decline,
            join_request_id: Some(join_request_id.into()),
            reject_reason: empty_as_none(reject_reason.into()),
            add_to_member_blacklist: None,
        }
    }

    /// 拒绝入群申请，并同时把申请人加入群黑名单（`add_to_member_blacklist = true`）。
    pub fn decline_and_blacklist(
        join_request_id: impl Into<String>,
        reject_reason: impl Into<String>,
    ) -> Self {
        let mut req = Self::decline(join_request_id, reject_reason);
        req.add_to_member_blacklist = Some(true);
        req
    }
}

/// 空串按「未填」处理。
///
/// 官方对多个可选字符串字段都用空串表示「未设置」，直接序列化会把「没填」变成
/// 「显式传了空串」，两者语义不同。
fn empty_as_none(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ==================== 查询群禁言状态 ====================

/// 查询群禁言状态（`GET /v2/groups/{group_openid}/restrict_chat_setting`，30 QPM）的响应。
///
/// 机器人需拥有群管理员身份。请求无参数（官方示例里请求体是 `{}`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RestrictChatSetting {
    /// 群级禁言规则（全员禁言配置）。
    #[serde(default)]
    pub global_rule: Option<GlobalMuteRule>,
    /// 当前处于禁言中的用户列表，**不含已过期**的禁言。
    #[serde(default)]
    pub members: Vec<MemberMuteState>,
}

/// 群级禁言规则（`global_rule`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GlobalMuteRule {
    /// 全员禁言模式，见 [`MuteMode`]。
    #[serde(default)]
    pub mode: Option<MuteMode>,
    /// 定时禁言规则列表（可包含多条）。
    #[serde(default)]
    pub schedule_rules: Vec<MuteScheduleRule>,
    /// 周期禁言规则列表（可包含多条）。
    #[serde(default)]
    pub recurring_rules: Vec<MuteRecurringRule>,
}

/// 全员禁言模式（`mode`）。官方文档列全了三种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuteMode {
    /// `none` — 未开启全员禁言。
    None,
    /// `always` — 始终禁言。
    Always,
    /// `schedule` — 定时 / 周期禁言。
    Schedule,
}

/// 一条定时禁言规则。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MuteScheduleRule {
    /// 任务 ID，用于标记此定时禁言任务。
    #[serde(default)]
    pub task_id: String,
    /// 禁言开始时间（RFC3339 格式）。
    #[serde(default)]
    pub start_at: String,
    /// 禁言结束时间（RFC3339 格式）。
    #[serde(default)]
    pub end_at: String,
    /// 此规则是否启用。
    #[serde(default)]
    pub enabled: bool,
}

/// 一条周期禁言规则。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MuteRecurringRule {
    /// 任务 ID，用于标记此周期禁言规则。
    #[serde(default)]
    pub task_id: String,
    /// 生效星期几，取值 **1~7（1 = 周一，7 = 周日）**，可多选。
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// 时段开始时间，格式 `HH:mm`（北京时间）。
    #[serde(default)]
    pub start_time: String,
    /// 时段结束时间，格式 `HH:mm`（北京时间）；**若小于 `start_time` 表示跨天到次日**。
    #[serde(default)]
    pub end_time: String,
    /// 此规则是否启用。
    #[serde(default)]
    pub enabled: bool,
}

/// 一名处于禁言中的成员。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberMuteState {
    /// 被禁言成员的 openid。
    #[serde(default)]
    pub member_openid: String,
    /// 禁言到期时间（RFC3339 格式）。
    #[serde(default)]
    pub mute_expire_at: String,
    /// 被禁言成员的昵称。
    #[serde(default)]
    pub username: String,
    /// 用户在应用 / 开放平台下的统一标识（**可能没有**，官方标注「如有」）。
    #[serde(default)]
    pub union_openid: Option<String>,
}

// ==================== 设置群成员禁言 ====================

/// 设置群成员禁言的请求体
/// （`POST /v2/groups/{group_openid}/restrict_chat_setting`，60 QPM）。
///
/// 机器人需拥有群管理员身份；**最大禁言时长 30 天**；单次设置的成员数不能超过 20 个。
///
/// 文档把 `members` 标为「否」，但空数组没有意义 —— 调用方仍需自行保证非空。
/// 响应为空（`{}`），因此没有对应的响应类型。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetRestrictChatSettingRequest {
    /// 用户禁言列表；每项通过 `op` 控制增 / 改 / 删，单次最多 20 个。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<SetMemberMuteState>,
}

impl SetRestrictChatSettingRequest {
    /// 官方限制：单次最多设置的成员数。
    pub const MAX_MEMBERS_PER_CALL: usize = 20;

    /// 用禁言列表构造。
    pub fn new(members: Vec<SetMemberMuteState>) -> Self {
        Self { members }
    }
}

/// 单个成员的禁言设置（官方结构名 `SetMemberMuteState`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetMemberMuteState {
    /// 操作类型，见 [`MuteOp`]。
    pub op: MuteOp,
    /// 被禁言成员的 openid。
    ///
    /// ⚠️ 官方提示：**增加 / 更新时只能操作普通成员**，不能操作群主、管理员和机器人。
    pub member_openid: String,
    /// 禁言到期时间（RFC3339 格式）；`op = del` 时**可传空串表示立即解除禁言**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_expire_at: Option<String>,
}

/// 成员禁言操作类型（`op`）。官方文档列全了三种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuteOp {
    /// `add` — 增加禁言。
    Add,
    /// `update` — 更新禁言到期时间。
    Update,
    /// `del` — 解除禁言。
    Del,
}

impl SetMemberMuteState {
    /// 新增禁言（`op = add`）。`mute_expire_at` 为 RFC3339，最长 30 天。
    pub fn add(member_openid: impl Into<String>, mute_expire_at: impl Into<String>) -> Self {
        Self {
            op: MuteOp::Add,
            member_openid: member_openid.into(),
            mute_expire_at: Some(mute_expire_at.into()),
        }
    }

    /// 更新禁言到期时间（`op = update`）。
    pub fn update(member_openid: impl Into<String>, mute_expire_at: impl Into<String>) -> Self {
        Self {
            op: MuteOp::Update,
            member_openid: member_openid.into(),
            mute_expire_at: Some(mute_expire_at.into()),
        }
    }

    /// 解除禁言（`op = del`）；按官方说明传空串表示立即解除。
    pub fn remove(member_openid: impl Into<String>) -> Self {
        Self {
            op: MuteOp::Del,
            member_openid: member_openid.into(),
            mute_expire_at: Some(String::new()),
        }
    }
}

// ==================== 查询入群自动审批策略列表 ====================

/// 查询入群自动审批策略列表的查询参数
/// （`GET /v2/groups/join_approval_strategy`，60 QPM）。
///
/// 返回的是**当前生效中**的策略，按创建时间倒序。
///
/// - `cursor`：分页游标，首次请求可不传或传空串。
/// - `limit`：单页数量，**默认 20，最大 50**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JoinApprovalStrategyListQuery {
    /// 分页游标；不传时该字段不会出现在查询串里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 单页数量；不传由服务端按默认值 20 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl JoinApprovalStrategyListQuery {
    /// 官方默认单页数量。
    pub const DEFAULT_LIMIT: u32 = 20;
    /// 官方单页数量上限。
    pub const MAX_LIMIT: u32 = 50;

    /// 设置游标。
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// 设置单页数量；超过官方上限时夹到上限，低于 1 时夹到 1。
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit.clamp(1, Self::MAX_LIMIT));
        self
    }
}

/// 入群自动审批策略列表的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JoinApprovalStrategyListResponse {
    /// 生效中的策略列表。
    #[serde(default)]
    pub strategies: Vec<JoinApprovalStrategy>,
    /// 下一页游标；**空串表示已到末页**。
    #[serde(default)]
    pub next_cursor: String,
}

impl JoinApprovalStrategyListResponse {
    /// 是否已经拉到末页。
    pub fn is_last_page(&self) -> bool {
        self.next_cursor.is_empty()
    }
}

/// 一条入群自动审批策略。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JoinApprovalStrategy {
    /// 策略 ID，由服务端生成（如 `st_d83eca11e9`）。
    #[serde(default)]
    pub strategy_id: String,
    /// 关联的群 openid 列表；**创建时用 `group_openids` 才有内容**，否则是空数组。
    #[serde(default)]
    pub group_openids: Vec<String>,
    /// 关联的 QQ 群号列表；**创建时用 `group_ids` 才有内容**。
    ///
    /// 官方把类型写成 `array`（uint64），但响应示例里是字符串、还可能被脱敏
    /// （`\"10****499\"`），所以用 [`GroupId`] 承载。
    #[serde(default)]
    pub group_ids: Vec<GroupId>,
    /// 白名单中的号码数量（官方说明是**估算值**，可能存在少量误差）。
    #[serde(default)]
    pub whitelist_user_count: u32,
    /// 策略是否启用，见 [`EnableState`]。
    #[serde(default)]
    pub is_enable: Option<EnableState>,
    /// 过期时间（RFC3339 格式）。
    #[serde(default)]
    pub expire_at: String,
    /// 创建时间（RFC3339 格式）。
    #[serde(default)]
    pub created_at: String,
    /// 最近更新时间（RFC3339 格式）。
    #[serde(default)]
    pub updated_at: String,
    /// 策略备注；官方响应示例里没出现，因此按可缺失处理。
    #[serde(default)]
    pub remark: Option<String>,
}

impl JoinApprovalStrategy {
    /// 策略是否处于启用状态（`is_enable = on`）。
    pub fn is_enabled(&self) -> bool {
        matches!(self.is_enable, Some(EnableState::On))
    }
}

/// 策略开关（`is_enable`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnableState {
    /// `on` — 启用。
    On,
    /// `off` — 关闭。
    Off,
}

/// QQ 群号。
///
/// 官方在**请求体**里把它标成 uint64，在**响应示例**里却是字符串，还可能被脱敏
/// （`\"10****499\"`）。这里用 untagged 联合两种都收：JSON 数字走 [`GroupId::Num`]，
/// JSON 字符串走 [`GroupId::Text`]，序列化时保持原样。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupId {
    /// 纯数字群号（官方标注的 uint64 形式）。
    Num(u64),
    /// 字符串形式；脱敏值（含 `*`）或超出 u64 的值只能走这里。
    Text(String),
}

impl GroupId {
    /// 能解析成数字时返回数字。
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            GroupId::Num(n) => Some(*n),
            GroupId::Text(s) => s.parse().ok(),
        }
    }
}

impl From<u64> for GroupId {
    fn from(v: u64) -> Self {
        GroupId::Num(v)
    }
}

impl From<String> for GroupId {
    fn from(v: String) -> Self {
        GroupId::Text(v)
    }
}

impl From<&str> for GroupId {
    fn from(v: &str) -> Self {
        GroupId::Text(v.to_string())
    }
}

// ==================== 创建入群自动审批策略 ====================

/// 创建入群自动审批策略的请求体（`POST /v2/groups/join_approval_strategy`，60 QPM）。
///
/// 坑（逐字段核对过）：
///
/// - `group_openids` 与 `group_ids` **二选一必填**：同时传入、或都不传都会被服务端
///   拒绝；各自最多 100 个。
/// - `is_enable` 不传默认 `on`；`expire_at` 不传默认**一年后**过期。
/// - `remark` 最多 255 个汉字，可不填。
/// - `strategy_id` 由服务端生成；**一个机器人最多 20 个策略**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateStrategyRequest {
    /// 关联的群 openid 列表，最多 100 个；与 `group_ids` 互斥。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
    /// 关联的 QQ 群号列表，最多 100 个；与 `group_openids` 互斥。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<GroupId>,
    /// 是否启用策略；不传默认 `on`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_enable: Option<EnableState>,
    /// 过期时间（RFC3339 格式）；不传默认一年后过期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<String>,
    /// 策略备注，最多 255 个汉字。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
}

impl CreateStrategyRequest {
    /// 官方限制：关联群的最大数量。
    pub const MAX_GROUPS: usize = 100;

    /// 用群 openid 创建（与 `group_ids` 二选一）。
    pub fn for_group_openids(group_openids: Vec<String>) -> Self {
        Self { group_openids, ..Self::default() }
    }

    /// 用 QQ 群号创建（与 `group_openids` 二选一）。
    pub fn for_group_ids(group_ids: Vec<GroupId>) -> Self {
        Self { group_ids, ..Self::default() }
    }
}

/// 创建入群自动审批策略的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateStrategyResponse {
    /// 服务端生成的策略 ID。
    #[serde(default)]
    pub strategy_id: String,
    /// 是否启用，见 [`EnableState`]。
    #[serde(default)]
    pub is_enable: Option<EnableState>,
    /// 过期时间（RFC3339 格式）。
    #[serde(default)]
    pub expire_at: String,
}

// ==================== 修改入群自动审批策略 ====================

/// 修改入群自动审批策略的请求体
/// （`PATCH /v2/groups/join_approval_strategy/{strategy_id}`，60 QPM）。
///
/// `strategy_id` 是**路径参数**。可改：生效状态、失效时间、关联群增删、备注。
///
/// ⚠️ `group_action` 里的群标识形式（openid 还是 QQ 群号）**必须与创建时一致**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStrategyRequest {
    /// 是否启用策略，见 [`EnableState`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_enable: Option<EnableState>,
    /// 过期时间（RFC3339 格式）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<String>,
    /// 关联群增删操作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_action: Option<GroupAction>,
    /// 策略备注，最多 255 个汉字。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
}

/// 关联群增删操作（`group_action`）。
///
/// 不实现 `Default`：`op` 没有合理的默认值，`group_openids` / `group_ids` 还
/// 必须与创建时的群标识形式一致，默认构造只会掩盖错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupAction {
    /// 操作类型，见 [`GroupActionOp`]。
    pub op: GroupActionOp,
    /// 待操作的群 openid 列表；与 `group_ids` 互斥。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
    /// 待操作的 QQ 群号列表；与 `group_openids` 互斥。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<GroupId>,
}

/// 关联群操作类型（`op`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupActionOp {
    /// `add` — 新增关联群。
    Add,
    /// `del` — 删除关联群。
    Del,
}

/// 修改入群自动审批策略的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateStrategyResponse {
    /// 是否启用，见 [`EnableState`]。
    #[serde(default)]
    pub is_enable: Option<EnableState>,
    /// 过期时间（RFC3339 格式）。
    #[serde(default)]
    pub expire_at: String,
}

// ==================== 删除入群自动审批策略 ====================

// `DELETE /v2/groups/join_approval_strategy/{strategy_id}`（60 QPM）：
// `strategy_id` 是路径参数，请求体与响应体都为空（`{}`），
// 因此没有对应的请求 / 响应类型。

// ==================== 执行入群自动审批策略 ====================

/// 执行入群自动审批策略的请求体
/// （`POST /v2/groups/join_approval_strategy/{strategy_id}/execute`，60 QPM）。
///
/// `strategy_id` 是**路径参数**，请求体是空对象 `{}`。
///
/// 语义：对策略关联的**全部群**发起全量扫描，命中白名单号码的入群申请自动审批通过；
/// **异步执行，约 10 分钟完成**。响应为空（`{}`），因此没有对应的响应类型。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ExecuteStrategyRequest {}

// ==================== 修改白名单号码 ====================

/// 修改入群自动审批策略白名单的请求体
/// （`POST /v2/groups/join_approval_strategy/{strategy_id}/whitelist_users`，60 QPM）。
///
/// 坑（逐字段核对过）：
///
/// - 两个字段都是**必填**（`op` 和 `whitelist_users`）。
/// - 单次最多 10000 个号码，**整个策略的号码上限 10W**。
/// - 号码必须是**字符串**：官方明确说是为了避免 JS 精度问题（QQ 号超过 2^53 时用数字
///   会丢精度），所以这里是 `Vec<String>`，不要换成整数。
///
/// 不实现 `Default`：`op` 与 `whitelist_users` 都是必填，没有合理默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhitelistUsersRequest {
    /// 操作类型，见 [`WhitelistOp`]。
    pub op: WhitelistOp,
    /// QQ 号码列表（字符串形式）。
    #[serde(default)]
    pub whitelist_users: Vec<String>,
}

impl WhitelistUsersRequest {
    /// 官方限制：单次最多提交的号码数。
    pub const MAX_USERS_PER_CALL: usize = 10_000;

    /// 新增号码。
    pub fn add(whitelist_users: Vec<String>) -> Self {
        Self { op: WhitelistOp::Add, whitelist_users }
    }

    /// 删除号码。
    pub fn del(whitelist_users: Vec<String>) -> Self {
        Self { op: WhitelistOp::Del, whitelist_users }
    }
}

/// 白名单操作类型（`op`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhitelistOp {
    /// `add` — 新增号码。
    Add,
    /// `del` — 删除号码。
    Del,
}

/// 修改白名单的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WhitelistUsersResponse {
    /// 策略 ID。
    #[serde(default)]
    pub strategy_id: String,
    /// 操作后策略当前白名单号码数（官方说明是**估算值**）。
    #[serde(default)]
    pub whitelist_user_count: u32,
    /// 策略更新时间（RFC3339 格式）。
    #[serde(default)]
    pub updated_at: String,
}

// ==================== 获取群成员列表 ====================

/// 获取群成员列表的查询参数
/// （`GET /v2/groups/{group_openid}/members`，60 QPM）。
///
/// ⚠️ 该接口**只有 `cursor` 一个参数，没有 `limit`**：每次最多返回
/// [`Self::PAGE_SIZE_CAP`] 条，翻页只能靠 `next_cursor`。该能力正在内邀接入中
/// （仅白名单机器人可用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupMemberListQuery {
    /// 分页游标，首次请求可不传或传空串；后续传上一次响应的 `next_cursor`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl GroupMemberListQuery {
    /// 官方限制：单次最多返回的成员数（由服务端固定，不受请求参数控制）。
    pub const PAGE_SIZE_CAP: usize = 30;

    /// 设置游标。
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

/// 获取群成员列表的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupMemberListResponse {
    /// 成员列表，每次最多返回 30 条。
    #[serde(default)]
    pub members: Vec<GroupMember>,
    /// 下一页游标；**空串表示已到末页**。
    #[serde(default)]
    pub next_cursor: String,
}

impl GroupMemberListResponse {
    /// 是否已经拉到末页。
    pub fn is_last_page(&self) -> bool {
        self.next_cursor.is_empty()
    }
}

/// 群成员信息。
///
/// 同时是 `GET /v2/groups/{group_openid}/members`（列表元素）与
/// `GET /v2/groups/{group_openid}/members/{member_openid}`（单个成员）的响应结构：
/// 两个接口的字段完全一致。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GroupMember {
    /// 成员 OpenID。
    #[serde(default)]
    pub member_openid: String,
    /// 用户昵称。
    #[serde(default)]
    pub username: String,
    /// 群成员角色，见 [`MemberRole`]。
    #[serde(default)]
    pub member_role: Option<MemberRole>,
    /// 是否机器人。
    #[serde(default)]
    pub bot: bool,
    /// 入群时间戳（RFC3339 格式）。
    #[serde(default)]
    pub joined_at: String,
    /// 用户在应用 / 开放平台下的统一标识（**可能没有**，官方标注「如有」）。
    #[serde(default)]
    pub union_openid: Option<String>,
}

impl GroupMember {
    /// 是否为群主或管理员。
    pub fn is_privileged(&self) -> bool {
        self.member_role.is_some_and(MemberRole::is_privileged)
    }
}

// ==================== 群成员批量移除 ====================

/// 群成员批量移除的请求体
/// （`POST /v2/groups/{group_openid}/batch_remove_members`，30 QPM）。
///
/// ⚠️ 该能力**正在内邀接入中**，仅白名单机器人可用（错误码 `11253`）。
///
/// - `member_openids`：必填，**单次最多 20 个**。
/// - `add_to_member_blacklist`：可选，**默认 `false`**；为 `true` 时移除的同时拉黑，
///   拉黑失败的 openid 会在响应里回传。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchRemoveMembersRequest {
    /// 需要移除的成员 member_openid 列表。
    #[serde(default)]
    pub member_openids: Vec<String>,
    /// 是否同时加入群黑名单，默认 `false`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_to_member_blacklist: Option<bool>,
}

impl BatchRemoveMembersRequest {
    /// 官方限制：单次最多移除的成员数。
    pub const MAX_MEMBERS_PER_CALL: usize = 20;

    /// 仅移除成员。
    pub fn new(member_openids: Vec<String>) -> Self {
        Self { member_openids, add_to_member_blacklist: None }
    }

    /// 移除成员并同时拉黑。
    pub fn remove_and_blacklist(member_openids: Vec<String>) -> Self {
        Self { member_openids, add_to_member_blacklist: Some(true) }
    }
}

/// 群成员批量移除的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchRemoveMembersResponse {
    /// 成功时返回 `\"success\"`。
    #[serde(default)]
    pub remove_members_result: String,
    /// 拉黑失败的 openid 列表；未开启拉黑时官方示例返回空数组，也可能不返回该字段。
    #[serde(default)]
    pub add_to_member_blacklist_fail_openids: Vec<String>,
}

// ==================== 群黑名单查询 ====================

/// 群黑名单查询的查询参数
/// （`GET /v2/groups/{group_openid}/member_blacklist`，30 QPM）。
///
/// ⚠️ 该能力**正在内邀接入中**，仅白名单机器人可用。
///
/// - `cursor`：分页游标，首次请求可不传或传空串。
/// - `limit`：单页数量，**默认 20，最大 100**（注意与入群申请列表的上限 50 不同）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberBlacklistQuery {
    /// 分页游标；不传时该字段不会出现在查询串里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 单页数量；不传由服务端按默认值 20 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MemberBlacklistQuery {
    /// 官方默认单页数量。
    pub const DEFAULT_LIMIT: u32 = 20;
    /// 官方单页数量上限（比入群申请列表的 50 更宽）。
    pub const MAX_LIMIT: u32 = 100;

    /// 设置游标。
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// 设置单页数量；超过官方上限时夹到上限，低于 1 时夹到 1。
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit.clamp(1, Self::MAX_LIMIT));
        self
    }
}

/// 群黑名单查询的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberBlacklist {
    /// 黑名单用户列表。
    #[serde(default)]
    pub users: Vec<BlacklistUser>,
    /// 下一页游标；**空串表示已到末页**。
    #[serde(default)]
    pub next_cursor: String,
}

impl MemberBlacklist {
    /// 是否已经拉到末页。
    pub fn is_last_page(&self) -> bool {
        self.next_cursor.is_empty()
    }
}

/// 黑名单中的一名用户（官方结构名 `BlacklistUser`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlacklistUser {
    /// 用户在应用 / 开放平台下的统一标识（**可能没有**，官方标注「如有」）。
    #[serde(default)]
    pub union_openid: Option<String>,
    /// 用户 openid。
    #[serde(default)]
    pub member_openid: String,
    /// 用户昵称。
    #[serde(default)]
    pub username: String,
    /// 拉黑时间戳（RFC3339 格式）。
    #[serde(default)]
    pub banned_at: String,
    /// 是否为机器人账号。
    #[serde(default)]
    pub bot: bool,
}

// ==================== 群黑名单操作 ====================

/// 群黑名单操作的请求体
/// （`POST /v2/groups/{group_openid}/member_blacklist`，60 QPM）。
///
/// 坑（逐字段核对过）：
///
/// - 两个字段都是**必填**：`op`（`add` / `del`）和 `member_openids`。
/// - `member_openids` **单次最多 20 个**。
/// - 官方说明：**只有目标用户不在群中时才能加入群黑名单**（在群里要先移除，或用批量
///   移除接口的 `add_to_member_blacklist`）。
/// - 该能力正在内邀接入中。
///
/// 不实现 `Default`：`op` 与 `member_openids` 都是必填，没有合理默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberBlacklistRequest {
    /// 操作类型，见 [`BlacklistOp`]。
    pub op: BlacklistOp,
    /// 目标成员 openid 列表，单次最多 20 个。
    #[serde(default)]
    pub member_openids: Vec<String>,
}

impl MemberBlacklistRequest {
    /// 官方限制：单次最多操作的成员数。
    pub const MAX_MEMBERS_PER_CALL: usize = 20;

    /// 加入黑名单。
    pub fn add(member_openids: Vec<String>) -> Self {
        Self { op: BlacklistOp::Add, member_openids }
    }

    /// 移出黑名单。
    pub fn del(member_openids: Vec<String>) -> Self {
        Self { op: BlacklistOp::Del, member_openids }
    }
}

/// 黑名单操作类型（`op`）。官方文档列全了两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlacklistOp {
    /// `add` — 加入黑名单。
    Add,
    /// `del` — 移出黑名单。
    Del,
}

/// 群黑名单操作的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemberBlacklistOpResponse {
    /// `op = add` 时是拉黑失败的 openid 列表；`op = del` 时同义（官方原话）。
    #[serde(default)]
    pub fail_openids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 序列化辅助：请求体 → JSON 值，便于与官方字段名逐字段比对。
    fn value_of<T: Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn group_info_response_matches_doc_example() {
        let raw = r#"{
          "group_openid": "3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A",
          "group_name": "读书分享会",
          "group_finger_memo": "每周共读一本好书",
          "group_class_text": "文化",
          "group_tags": ["阅读", "文学", "成长"],
          "group_member_num": 256
        }"#;
        let info: GroupInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(info.group_openid, "3E5D8A1F7B2C9E4D6A0F1B3C5D7E9F2A");
        assert_eq!(info.group_name, "读书分享会");
        assert_eq!(info.group_finger_memo, "每周共读一本好书");
        assert_eq!(info.group_class_text, "文化");
        assert_eq!(info.group_tags, vec!["阅读", "文学", "成长"]);
        assert_eq!(info.group_member_num, 256);
    }

    #[test]
    fn bot_state_response_matches_doc_example() {
        // 官方示例里 member_role 的收尾引号漏了（"member 未闭合），属文档笔误；
        // 这里补上引号，其余字段原样照搬。
        let raw = r#"{
          "member_openid": "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D",
          "joined_at": "2025-06-15T14:30:00+08:00",
          "allow_proactive_msg": false,
          "recv_msg_setting": "only_mention",
          "member_role": "member"
        }"#;
        let state: BotState = serde_json::from_str(raw).unwrap();
        assert_eq!(state.member_openid, "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D");
        assert_eq!(state.joined_at, "2025-06-15T14:30:00+08:00");
        assert!(!state.allow_proactive_msg);
        assert_eq!(state.recv_msg_setting, Some(RecvMsgSetting::OnlyMention));
        assert_eq!(state.member_role, Some(MemberRole::Member));
        assert!(!state.member_role.unwrap().is_privileged());

        // 枚举取值与官方表格逐一对齐
        assert_eq!(serde_json::from_str::<RecvMsgSetting>(r#""all""#).unwrap(), RecvMsgSetting::All);
        assert_eq!(
            serde_json::from_str::<RecvMsgSetting>(r#""mention_and_context""#).unwrap(),
            RecvMsgSetting::MentionAndContext
        );
        assert_eq!(serde_json::from_str::<MemberRole>(r#""owner""#).unwrap(), MemberRole::Owner);
        assert_eq!(serde_json::from_str::<MemberRole>(r#""admin""#).unwrap(), MemberRole::Admin);
        assert!(MemberRole::Admin.is_privileged());
        assert!(MemberRole::Owner.is_privileged());
        assert!(!MemberRole::Member.is_privileged());
    }

    #[test]
    fn join_request_list_response_matches_doc_example() {
        let raw = r#"{
          "list": [
            {
              "join_request_id": "Ael-dmvlRdC9fZepnrfMhamsZgO103pSjmzwUz5SyyORaQMX-q0zkY3Q1caz71KiH2nJzehE-QWM-xCWzIsLg1i1vAPkdJfdPkUDVImXoiR8OY_s40J7OsFZGaEFUdDkIhAPs9uMXOxNpW91mGWQTlaFnmgksAxk",
              "risk_tips": "",
              "union_openid": "FE003FAF76C4817251FDC128A16753BB",
              "member_openid": "FE003FAF76C4817251FDC128A16753BB",
              "username": "痞孓小光光╮hw灰",
              "apply_at": "2026-08-05T14:19:09+08:00",
              "apply_source": "self_apply",
              "invited_by": "",
              "bot": false,
              "verify_info": {
                "method": "verify_message",
                "verify_message": "几款看看",
                "review_qa_list": []
              }
            }
          ],
          "next_cursor": "1785767153250497"
        }"#;
        let resp: JoinRequestListResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.list.len(), 1);
        assert!(!resp.is_last_page());
        let req = &resp.list[0];
        assert_eq!(req.username, "痞孓小光光╮hw灰");
        assert_eq!(req.apply_at, "2026-08-05T14:19:09+08:00");
        assert_eq!(req.apply_source, Some(ApplySource::SelfApply));
        assert_eq!(req.invited_by.as_deref(), Some(""));
        assert!(!req.bot);

        let verify = req.verify_info.as_ref().unwrap();
        assert_eq!(verify.method, Some(VerifyMethod::VerifyMessage));
        assert_eq!(verify.verify_message.as_deref(), Some("几款看看"));
        assert!(verify.review_qa_list.is_empty());

        // 另一种验证方式（官方表格里的 admin_review_qa）
        let qa: VerifyInfo = serde_json::from_str(
            r#"{"method":"admin_review_qa","review_qa_list":[{"question":"Q","answer":"A"}]}"#,
        )
        .unwrap();
        assert_eq!(qa.method, Some(VerifyMethod::AdminReviewQa));
        assert_eq!(qa.review_qa_list[0].question, "Q");
        assert_eq!(qa.review_qa_list[0].answer, "A");
        assert!(qa.verify_message.is_none());
        assert_eq!(serde_json::from_str::<ApplySource>(r#""invited""#).unwrap(), ApplySource::Invited);
    }

    #[test]
    fn join_request_list_query_serializes_cursor_and_limit() {
        assert_eq!(JoinRequestListQuery::DEFAULT_LIMIT, 20);
        assert_eq!(JoinRequestListQuery::MAX_LIMIT, 50);

        assert_eq!(value_of(&JoinRequestListQuery::default()), json!({}));
        let q = JoinRequestListQuery::default().with_cursor("c1").with_limit(20);
        assert_eq!(value_of(&q), json!({"cursor": "c1", "limit": 20}));
        // 超过官方上限时夹住，避免服务端拒绝
        assert_eq!(JoinRequestListQuery::default().with_limit(999).limit, Some(50));
    }

    #[test]
    fn approval_join_request_body_matches_doc_example() {
        // 官方「通过用户审批」示例
        let approve = ApprovalJoinRequest {
            op: ApprovalOp::Approve,
            join_request_id: Some("AURi8Rr6MfGdUNedupWf2uV5XiayURHaetzwGyOdrj6m".into()),
            reject_reason: None,
            add_to_member_blacklist: None,
        };
        assert_eq!(
            value_of(&approve),
            json!({"op": "approve", "join_request_id": "AURi8Rr6MfGdUNedupWf2uV5XiayURHaetzwGyOdrj6m"})
        );
        assert_eq!(
            value_of(&ApprovalJoinRequest::approve("J1")),
            json!({"op": "approve", "join_request_id": "J1"})
        );

        // 官方「拒绝并拉黑」示例
        let decline =
            ApprovalJoinRequest::decline_and_blacklist("AVKiFWpdy0", "示例拒绝：机器人自动拒绝");
        assert_eq!(
            value_of(&decline),
            json!({
                "op": "decline",
                "join_request_id": "AVKiFWpdy0",
                "reject_reason": "示例拒绝：机器人自动拒绝",
                "add_to_member_blacklist": true
            })
        );

        // 拒绝但没写理由：reject_reason 整个字段不出现（官方标为「否」）
        assert_eq!(
            value_of(&ApprovalJoinRequest::decline("J1", "")),
            json!({"op": "decline", "join_request_id": "J1"})
        );
    }

    #[test]
    fn restrict_chat_setting_response_matches_doc_example() {
        let raw = r#"{
          "global_rule": {
            "mode": "schedule",
            "schedule_rules": [
              {
                "task_id": "task_7ffd5d31e2b37c1c872acb51",
                "start_at": "2026-07-22T10:44:00+08:00",
                "end_at": "2026-07-22T11:44:00+08:00",
                "enabled": false
              },
              {
                "task_id": "task_e9ca43ca9a31b539d824639c",
                "start_at": "2026-07-22T10:54:00+08:00",
                "end_at": "2026-07-22T11:54:00+08:00",
                "enabled": false
              }
            ],
            "recurring_rules": [
              {
                "task_id": "task_3a6348b8fb04bbc48b8a8709",
                "weekdays": [1, 2, 3, 4, 5, 6, 7],
                "start_time": "13:05",
                "end_time": "14:05",
                "enabled": true
              }
            ]
          },
          "members": [
            {
              "member_openid": "EC58D87F598C8294A533B9D458DAAF33",
              "mute_expire_at": "2026-08-05T11:23:04+08:00",
              "username": "T小不点101",
              "union_openid": "EC58D87F598C8294A533B9D458DAAF33"
            }
          ]
        }"#;
        let setting: RestrictChatSetting = serde_json::from_str(raw).unwrap();
        let rule = setting.global_rule.as_ref().unwrap();
        assert_eq!(rule.mode, Some(MuteMode::Schedule));
        assert_eq!(rule.schedule_rules.len(), 2);
        assert_eq!(rule.schedule_rules[0].task_id, "task_7ffd5d31e2b37c1c872acb51");
        assert!(!rule.schedule_rules[0].enabled);
        assert_eq!(rule.recurring_rules[0].weekdays, vec![1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(rule.recurring_rules[0].start_time, "13:05");
        assert_eq!(rule.recurring_rules[0].end_time, "14:05");
        assert!(rule.recurring_rules[0].enabled);
        assert_eq!(setting.members[0].member_openid, "EC58D87F598C8294A533B9D458DAAF33");
        assert_eq!(setting.members[0].mute_expire_at, "2026-08-05T11:23:04+08:00");
        assert_eq!(setting.members[0].username, "T小不点101");
        assert_eq!(serde_json::from_str::<MuteMode>(r#""none""#).unwrap(), MuteMode::None);
        assert_eq!(serde_json::from_str::<MuteMode>(r#""always""#).unwrap(), MuteMode::Always);
    }

    #[test]
    fn set_restrict_chat_setting_body_matches_doc_example() {
        assert_eq!(SetRestrictChatSettingRequest::MAX_MEMBERS_PER_CALL, 20);

        let req = SetRestrictChatSettingRequest::new(vec![SetMemberMuteState::add(
            "EC58D87F598C8294A533B9D458DAAF33",
            "2026-08-05T11:23:05+08:00",
        )]);
        assert_eq!(
            value_of(&req),
            json!({"members": [{
                "op": "add",
                "member_openid": "EC58D87F598C8294A533B9D458DAAF33",
                "mute_expire_at": "2026-08-05T11:23:05+08:00"
            }]})
        );

        // op 的另外两个取值
        assert_eq!(
            value_of(&SetMemberMuteState::update("M1", "T1")),
            json!({"op": "update", "member_openid": "M1", "mute_expire_at": "T1"})
        );
        // 解除禁言：按官方说明传空串表示立即解除
        assert_eq!(
            value_of(&SetMemberMuteState::remove("M1")),
            json!({"op": "del", "member_openid": "M1", "mute_expire_at": ""})
        );

        // members 为空时整个字段不序列化
        assert_eq!(value_of(&SetRestrictChatSettingRequest::default()), json!({}));
    }

    #[test]
    fn join_approval_strategy_list_response_matches_doc_example() {
        let raw = r#"{
          "strategies": [
            {
              "strategy_id": "st_d83eca11e9",
              "group_openids": [],
              "group_ids": [],
              "whitelist_user_count": 2,
              "is_enable": "on",
              "expire_at": "2027-08-05T15:30:16+08:00",
              "created_at": "2026-08-05T15:30:16+08:00",
              "updated_at": "2026-08-05T15:45:28+08:00"
            },
            {
              "strategy_id": "st_7c0b77d442",
              "group_openids": [],
              "group_ids": ["10****499"],
              "whitelist_user_count": 0,
              "is_enable": "on",
              "expire_at": "2027-08-04T11:20:40+08:00",
              "created_at": "2026-08-04T11:20:40+08:00",
              "updated_at": "2026-08-04T11:20:40+08:00"
            },
            {
              "strategy_id": "st_42cc272536",
              "group_openids": [],
              "group_ids": ["26****763", "26****978"],
              "whitelist_user_count": 3,
              "is_enable": "on",
              "expire_at": "2027-07-31T11:40:21+08:00",
              "created_at": "2026-07-31T11:40:21+08:00",
              "updated_at": "2026-08-05T14:28:37+08:00"
            }
          ],
          "next_cursor": ""
        }"#;
        let resp: JoinApprovalStrategyListResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.strategies.len(), 3);
        assert!(resp.is_last_page());
        assert!(resp.strategies[0].is_enabled());
        assert_eq!(resp.strategies[0].whitelist_user_count, 2);
        assert_eq!(resp.strategies[0].expire_at, "2027-08-05T15:30:16+08:00");
        assert_eq!(resp.strategies[1].group_ids, vec![GroupId::Text("10****499".into())]);
        assert_eq!(resp.strategies[2].group_ids.len(), 2);
        assert_eq!(serde_json::from_str::<EnableState>(r#""off""#).unwrap(), EnableState::Off);

        // GroupId 同时吃数字与字符串（官方请求里是 uint64，响应示例里是字符串）
        assert_eq!(serde_json::from_str::<GroupId>("123456").unwrap(), GroupId::Num(123456));
        assert_eq!(
            serde_json::from_str::<GroupId>(r#""123456""#).unwrap(),
            GroupId::Text("123456".into())
        );
        assert_eq!(GroupId::Text("123456".into()).as_u64(), Some(123456));
        assert_eq!(GroupId::Text("10****499".into()).as_u64(), None);
        assert_eq!(value_of(&GroupId::Num(42)), json!(42));
        assert_eq!(value_of(&GroupId::from("abc")), json!("abc"));
    }

    #[test]
    fn join_approval_strategy_list_query_serializes_cursor_and_limit() {
        assert_eq!(JoinApprovalStrategyListQuery::DEFAULT_LIMIT, 20);
        assert_eq!(JoinApprovalStrategyListQuery::MAX_LIMIT, 50);
        assert_eq!(value_of(&JoinApprovalStrategyListQuery::default()), json!({}));
        let q = JoinApprovalStrategyListQuery::default().with_cursor("c1").with_limit(7);
        assert_eq!(value_of(&q), json!({"cursor": "c1", "limit": 7}));
    }

    #[test]
    fn create_strategy_request_and_response_match_doc() {
        assert_eq!(CreateStrategyRequest::MAX_GROUPS, 100);

        // 官方请求示例（示例里的 \u003c / \u003e 就是 < / >）
        let req = CreateStrategyRequest {
            group_openids: vec!["<xxxxxxxx1".into(), "xxxxx2>".into()],
            group_ids: Vec::new(),
            is_enable: Some(EnableState::On),
            expire_at: Some(String::new()),
            remark: None,
        };
        assert_eq!(
            value_of(&req),
            json!({"group_openids": ["<xxxxxxxx1", "xxxxx2>"], "is_enable": "on", "expire_at": ""})
        );

        // 只传 group_ids 时 group_openids 不出现（二选一，不能同时传）
        let req = CreateStrategyRequest {
            group_ids: vec![GroupId::Num(100001)],
            is_enable: Some(EnableState::Off),
            ..Default::default()
        };
        assert_eq!(value_of(&req), json!({"group_ids": [100001], "is_enable": "off"}));

        // 构造器同样只放一侧
        assert_eq!(
            value_of(&CreateStrategyRequest::for_group_openids(vec!["G1".into()])),
            json!({"group_openids": ["G1"]})
        );
        assert_eq!(
            value_of(&CreateStrategyRequest::for_group_ids(vec![GroupId::Num(7)])),
            json!({"group_ids": [7]})
        );

        let resp: CreateStrategyResponse = serde_json::from_str(
            r#"{"strategy_id":"st_d83eca11e9","is_enable":"on","expire_at":"2027-08-05T15:30:16+08:00"}"#,
        )
        .unwrap();
        assert_eq!(resp.strategy_id, "st_d83eca11e9");
        assert_eq!(resp.is_enable, Some(EnableState::On));
        assert_eq!(resp.expire_at, "2027-08-05T15:30:16+08:00");
    }

    #[test]
    fn update_strategy_request_and_response_match_doc() {
        // 官方「停用规则」示例
        let req = UpdateStrategyRequest { is_enable: Some(EnableState::Off), ..Default::default() };
        assert_eq!(value_of(&req), json!({"is_enable": "off"}));

        // 官方「增加群 OpenID」示例
        let req = UpdateStrategyRequest {
            group_action: Some(GroupAction {
                op: GroupActionOp::Add,
                group_openids: vec!["aBCsdfasd".into()],
                group_ids: Vec::new(),
            }),
            ..Default::default()
        };
        assert_eq!(
            value_of(&req),
            json!({"group_action": {"op": "add", "group_openids": ["aBCsdfasd"]}})
        );

        // 群号形式（须与创建时保持一致）+ 备注
        let req = UpdateStrategyRequest {
            group_action: Some(GroupAction {
                op: GroupActionOp::Del,
                group_openids: Vec::new(),
                group_ids: vec![GroupId::Num(100001)],
            }),
            remark: Some("备注".into()),
            ..Default::default()
        };
        assert_eq!(
            value_of(&req),
            json!({"group_action": {"op": "del", "group_ids": [100001]}, "remark": "备注"})
        );

        let resp: UpdateStrategyResponse = serde_json::from_str(
            r#"{"is_enable":"off","expire_at":"2027-08-05T15:30:16+08:00"}"#,
        )
        .unwrap();
        assert_eq!(resp.is_enable, Some(EnableState::Off));
        assert_eq!(resp.expire_at, "2027-08-05T15:30:16+08:00");
    }

    #[test]
    fn execute_strategy_request_is_empty_object() {
        // 官方请求示例就是 {}
        assert_eq!(value_of(&ExecuteStrategyRequest::default()), json!({}));
        assert_eq!(serde_json::to_string(&ExecuteStrategyRequest {}).unwrap(), "{}");
    }

    #[test]
    fn whitelist_users_request_and_response_match_doc() {
        assert_eq!(WhitelistUsersRequest::MAX_USERS_PER_CALL, 10_000);

        let req = WhitelistUsersRequest::add(vec!["1234567".into(), "1234568".into()]);
        assert_eq!(
            value_of(&req),
            json!({"op": "add", "whitelist_users": ["1234567", "1234568"]})
        );

        // 号码必须是字符串，避免 JS 精度问题
        let del = value_of(&WhitelistUsersRequest::del(vec!["1234567".into()]));
        assert_eq!(del, json!({"op": "del", "whitelist_users": ["1234567"]}));
        assert!(del["whitelist_users"][0].is_string());

        let resp: WhitelistUsersResponse = serde_json::from_str(
            r#"{"strategy_id":"st_d83eca11e9","whitelist_user_count":2,"updated_at":"2026-08-05T15:45:28+08:00"}"#,
        )
        .unwrap();
        assert_eq!(resp.strategy_id, "st_d83eca11e9");
        assert_eq!(resp.whitelist_user_count, 2);
        assert_eq!(resp.updated_at, "2026-08-05T15:45:28+08:00");
    }

    #[test]
    fn group_member_list_response_matches_doc_example() {
        let raw = r#"{
            "members": [
                {
                    "member_openid": "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D",
                    "username": "阳光小助手",
                    "member_role": "member",
                    "bot": false,
                    "joined_at": "2025-08-20T09:15:00+08:00",
                    "union_openid": "9F2E872045CCCC5948BEAF5B5FCCDF22"
                },
                {
                    "member_openid": "EC58D87F598C8294A533B9D458DAAF33",
                    "username": "T小不点101",
                    "member_role": "member",
                    "bot": false,
                    "joined_at": "2025-07-01T10:30:00+08:00",
                    "union_openid": "FE003FAF76C4817251FDC128A16753BB"
                }
            ],
            "next_cursor": "bG1fNmIxOTM1NTRjNy4zMA"
        }"#;
        let resp: GroupMemberListResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.members.len(), 2);
        assert_eq!(resp.members[0].username, "阳光小助手");
        assert_eq!(resp.members[0].member_role, Some(MemberRole::Member));
        assert!(!resp.members[1].bot);
        assert_eq!(resp.next_cursor, "bG1fNmIxOTM1NTRjNy4zMA");
        assert!(!resp.is_last_page());

        // 该接口只有 cursor，没有 limit（每次最多 30 条由服务端控制）
        assert_eq!(GroupMemberListQuery::PAGE_SIZE_CAP, 30);
        assert_eq!(value_of(&GroupMemberListQuery::default()), json!({}));
        assert_eq!(
            value_of(&GroupMemberListQuery::default().with_cursor("c")),
            json!({"cursor": "c"})
        );
    }

    #[test]
    fn group_member_info_response_matches_doc_example() {
        let raw = r#"{
          "member_openid": "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D",
          "username": "小明",
          "member_role": "admin",
          "bot": false,
          "joined_at": "2025-08-20T09:15:00+08:00",
          "union_openid": "B4C6D8E0F2A4B6C8D0E2F4A6B8C0D2E4"
        }"#;
        let member: GroupMember = serde_json::from_str(raw).unwrap();
        assert_eq!(member.username, "小明");
        assert_eq!(member.member_role, Some(MemberRole::Admin));
        assert!(member.is_privileged());
        assert_eq!(member.joined_at, "2025-08-20T09:15:00+08:00");
        assert_eq!(member.union_openid.as_deref(), Some("B4C6D8E0F2A4B6C8D0E2F4A6B8C0D2E4"));
    }

    #[test]
    fn batch_remove_members_request_and_response_match_doc() {
        assert_eq!(BatchRemoveMembersRequest::MAX_MEMBERS_PER_CALL, 20);

        let req = BatchRemoveMembersRequest::new(vec!["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D".into()]);
        assert_eq!(
            value_of(&req),
            json!({"member_openids": ["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D"]})
        );

        // 同时拉黑时显式传 true（默认 false 由服务端兜底，这里不发送该字段）
        let req = BatchRemoveMembersRequest::remove_and_blacklist(vec!["M1".into()]);
        assert_eq!(
            value_of(&req),
            json!({"member_openids": ["M1"], "add_to_member_blacklist": true})
        );

        let resp: BatchRemoveMembersResponse = serde_json::from_str(
            r#"{"remove_members_result":"success","add_to_member_blacklist_fail_openids":[]}"#,
        )
        .unwrap();
        assert_eq!(resp.remove_members_result, "success");
        assert!(resp.add_to_member_blacklist_fail_openids.is_empty());
    }

    #[test]
    fn member_blacklist_response_matches_doc_example() {
        let raw = r#"{
          "users": [
            {
              "union_openid": "9F2E872045CCCC5948BEAF5B5FCCDF22",
              "member_openid": "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D",
              "username": "阳光少年",
              "banned_at": "2025-07-01T10:30:00+08:00",
              "bot": false
            }
          ],
          "next_cursor": ""
        }"#;
        let list: MemberBlacklist = serde_json::from_str(raw).unwrap();
        assert_eq!(list.users.len(), 1);
        assert_eq!(list.users[0].username, "阳光少年");
        assert_eq!(list.users[0].banned_at, "2025-07-01T10:30:00+08:00");
        assert!(!list.users[0].bot);
        assert!(list.is_last_page());

        // 黑名单分页上限是 100（入群申请列表是 50）
        assert_eq!(MemberBlacklistQuery::DEFAULT_LIMIT, 20);
        assert_eq!(MemberBlacklistQuery::MAX_LIMIT, 100);
        assert_eq!(MemberBlacklistQuery::default().with_limit(500).limit, Some(100));
        assert_eq!(value_of(&MemberBlacklistQuery::default()), json!({}));
        let q = MemberBlacklistQuery::default().with_cursor("c2").with_limit(100);
        assert_eq!(value_of(&q), json!({"cursor": "c2", "limit": 100}));
    }

    #[test]
    fn member_blacklist_request_and_response_match_doc() {
        assert_eq!(MemberBlacklistRequest::MAX_MEMBERS_PER_CALL, 20);

        let add = MemberBlacklistRequest::add(vec!["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D".into()]);
        assert_eq!(
            value_of(&add),
            json!({"op": "add", "member_openids": ["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D"]})
        );

        let del = MemberBlacklistRequest::del(vec!["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D".into()]);
        assert_eq!(
            value_of(&del),
            json!({"op": "del", "member_openids": ["7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D"]})
        );

        let resp: MemberBlacklistOpResponse = serde_json::from_str(r#"{"fail_openids":[]}"#).unwrap();
        assert!(resp.fail_openids.is_empty());
    }

    #[test]
    fn optional_fields_may_be_missing() {
        let info: GroupInfo = serde_json::from_str("{}").unwrap();
        assert!(info.group_tags.is_empty() && info.group_name.is_empty());

        let state: BotState = serde_json::from_str("{}").unwrap();
        assert!(state.recv_msg_setting.is_none() && state.member_role.is_none());
        assert!(!state.allow_proactive_msg);

        let req: JoinRequest = serde_json::from_str(r#"{"join_request_id":"J1"}"#).unwrap();
        assert!(req.verify_info.is_none() && req.invited_by.is_none() && req.union_openid.is_none());
        assert!(req.apply_source.is_none());

        let setting: RestrictChatSetting = serde_json::from_str("{}").unwrap();
        assert!(setting.global_rule.is_none() && setting.members.is_empty());

        let strategy: JoinApprovalStrategy =
            serde_json::from_str(r#"{"strategy_id":"st_1"}"#).unwrap();
        assert!(strategy.group_ids.is_empty() && strategy.is_enable.is_none());
        assert!(strategy.remark.is_none() && strategy.group_openids.is_empty());
        assert!(!strategy.is_enabled());

        let member: GroupMember = serde_json::from_str(r#"{"member_openid":"M1"}"#).unwrap();
        assert!(member.member_role.is_none() && member.union_openid.is_none());
        assert!(!member.is_privileged());

        let user: BlacklistUser = serde_json::from_str(r#"{"member_openid":"M1"}"#).unwrap();
        assert!(user.union_openid.is_none() && !user.bot);

        let removed: BatchRemoveMembersResponse = serde_json::from_str("{}").unwrap();
        assert!(removed.add_to_member_blacklist_fail_openids.is_empty());

        let op: MemberBlacklistOpResponse = serde_json::from_str("{}").unwrap();
        assert!(op.fail_openids.is_empty());

        let verify: VerifyInfo = serde_json::from_str(r#"{"method":"admin_review_qa"}"#).unwrap();
        assert!(verify.verify_message.is_none() && verify.review_qa_list.is_empty());

        let created: CreateStrategyResponse = serde_json::from_str("{}").unwrap();
        assert!(created.is_enable.is_none() && created.strategy_id.is_empty());

        let updated: UpdateStrategyResponse = serde_json::from_str("{}").unwrap();
        assert!(updated.is_enable.is_none());

        let whitelist: WhitelistUsersResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(whitelist.whitelist_user_count, 0);

        let list: JoinApprovalStrategyListResponse = serde_json::from_str("{}").unwrap();
        assert!(list.is_last_page() && list.strategies.is_empty());
    }
}
