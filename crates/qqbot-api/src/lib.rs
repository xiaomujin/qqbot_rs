//! QQ 官方 API v2 协议层。
//!
//! # 模块职责
//!
//! - 纯类型 / 纯函数（无 IO）：[`opcode`]、[`intents`]、[`payload`]、[`message`]、[`event`]
//! - 唯一携带 IO 的模块：[`client`]（HTTP + access_token 管理）
//!
//! # 官方协议要点
//!
//! - OpenAPI 基址：`https://api.bot.qq.com`
//! - 鉴权：请求头 `Authorization: QQBot {ACCESS_TOKEN}`
//! - ⚠️ 失败时 HTTP 状态码**可能仍是 200**，必须以响应体的 `err_code` 判定成败

pub mod bot;
pub mod client;
pub mod error;
pub mod event;
pub mod group;
pub mod intents;
pub mod message;
pub mod opcode;
pub mod payload;

pub use client::{
    ApiClient, ApiClientConfig, GatewayInfo, MediaUploadResult, SendResult, SessionStartLimit,
    TokenProvider, UploadConfig, UploadPartUrl, UploadPrepareResult,
};
pub use bot::{
    BotInfo, EmptyResponse, Menu, MenuItem, MenuItemType, MenuResponse, MenuSwitch,
    MenuUpdateRequest, MenuUpdateResponse, Panel, PanelCreateRequest, PanelCreateResponse,
    PanelDetailResponse, PanelItem, PanelItemType, PanelListQuery, PanelListResponse, PanelRecord,
    PanelScope, PanelTargetOp, PanelTargetRequest, PanelTargetType, PanelUpdateRequest,
    PanelUpdateResponse, ShareLinkData, ShareLinkRequest, ShareLinkResponse, SubMenuItem,
    SubMenuItemType,
};
pub use error::ApiError;
pub use group::{
    ApprovalJoinRequest, ApprovalOp, ApplySource, BatchRemoveMembersRequest,
    BatchRemoveMembersResponse, BlacklistOp, BlacklistUser, BotState, CreateStrategyRequest,
    CreateStrategyResponse, EnableState, ExecuteStrategyRequest, GlobalMuteRule, GroupAction,
    GroupActionOp, GroupId, GroupInfo, GroupMember, GroupMemberListQuery, GroupMemberListResponse,
    JoinApprovalStrategy, JoinApprovalStrategyListQuery, JoinApprovalStrategyListResponse,
    JoinRequest, JoinRequestListQuery, JoinRequestListResponse, MemberBlacklist,
    MemberBlacklistOpResponse, MemberBlacklistQuery, MemberBlacklistRequest, MemberMuteState,
    MemberRole, MuteMode, MuteOp, MuteRecurringRule, MuteScheduleRule, RecvMsgSetting,
    RestrictChatSetting, ReviewQA, SetMemberMuteState, SetRestrictChatSettingRequest,
    UpdateStrategyRequest, UpdateStrategyResponse, VerifyInfo, VerifyMethod, WhitelistOp,
    WhitelistUsersRequest, WhitelistUsersResponse,
};
pub use event::{Event, MessageEvent, RawNotice, User};
pub use intents::Intents;
pub use message::{
    Button, ButtonAction, InteractionCode, InteractionResponse, Keyboard, KeyboardContent,
    KeyboardRow, Markdown, Media, MessageExtInfo, MessageReference, Modal, MsgType, OutMessage,
    Permission, RenderData, StreamContentType, StreamInputMode, StreamInputState, StreamMessage,
    StreamMessageResult, Target,
};
pub use opcode::OpCode;
pub use payload::{Hello, Identify, Payload, Properties, RawPayload, Ready, ReadyUser, Resume};

/// 官方统一请求地址。
pub const DEFAULT_API_BASE: &str = "https://api.bot.qq.com";

/// 群聊 + 单聊所需的最小 intents 集合（`GROUP_AND_C2C_EVENT = 1 << 25`）。
pub const DEFAULT_INTENTS: Intents = Intents::GROUP_AND_C2C_EVENT;
