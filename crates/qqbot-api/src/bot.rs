//! 机器人接口（`/users/@me`、`/v2/generate_url_link`、`/v2/menu`、`/v2/panels`）的请求 / 响应类型。
//!
//! 纯类型 / 纯函数，**不含 IO**。HTTP 调用在 `client.rs`。
//!
//! # 覆盖的接口
//!
//! | 接口 | 方法 | 路径 | 相关类型 |
//! | --- | --- | --- | --- |
//! | 获取机器人详情 | GET | `/users/@me` | [`BotInfo`] |
//! | 生成分享链接 | POST | `/v2/generate_url_link` | [`ShareLinkRequest`] / [`ShareLinkResponse`] |
//! | 查询全局自定义菜单 | GET | `/v2/menu` | [`MenuResponse`] |
//! | 修改全局自定义菜单 | PUT | `/v2/menu` | [`MenuUpdateRequest`] / [`MenuUpdateResponse`] |
//! | 查询指令面板列表 | GET | `/v2/panels` | [`PanelListQuery`] / [`PanelListResponse`] |
//! | 创建指令面板 | POST | `/v2/panels` | [`PanelCreateRequest`] / [`PanelCreateResponse`] |
//! | 查询指令面板详情 | GET | `/v2/panels/{panel_id}` | [`PanelDetailResponse`] |
//! | 修改指令面板 | PUT | `/v2/panels/{panel_id}` | [`PanelUpdateRequest`] / [`PanelUpdateResponse`] |
//! | 删除指令面板 | DELETE | `/v2/panels/{panel_id}` | [`EmptyResponse`] |
//! | 修改指令面板关联对象 | PUT | `/v2/panels/{panel_id}/target` | [`PanelTargetRequest`] / [`EmptyResponse`] |
//!
//! 路径常量与拼接函数（[`USERS_ME_PATH`]、[`panel_path`] 等）也放在这里，供 `client.rs` 直接使用。
//!
//! # 通用坑位
//!
//! - 失败时 HTTP 状态码可能仍是 200，成败以响应体 `err_code` 为准（见 `client::decode_response`）。
//! - 官方字段表里嵌套结构一律标「否（可选）」，那是文档渲染的产物；
//!   本模块对**语义上必须存在**的字段（如 `PanelItem::name` / `item_type`）仍按必填处理，
//!   其余一律 [`Option`] 或带 `#[serde(default)]` 的空集合。


use serde::{Deserialize, Serialize};

// ==================== 路径 ====================

/// `GET /users/@me`：获取当前机器人（当前用户）详情。
pub const USERS_ME_PATH: &str = "/users/@me";

/// `POST /v2/generate_url_link`：生成机器人分享链接。
pub const GENERATE_URL_LINK_PATH: &str = "/v2/generate_url_link";

/// `GET` / `PUT /v2/menu`：全局自定义菜单。
pub const MENU_PATH: &str = "/v2/menu";

/// `GET` / `POST /v2/panels`：指令面板集合。
pub const PANELS_PATH: &str = "/v2/panels";

/// 一个机器人最多能创建的指令面板数量。
pub const MAX_PANELS_PER_BOT: usize = 20;

/// 单个指令面板最多能配置的面板元素数量。
pub const MAX_PANEL_ITEMS: usize = 20;

/// 指令面板列表分页的默认条数。
pub const DEFAULT_PANEL_PAGE_SIZE: u32 = 20;

/// 指令面板列表分页的最大条数。
pub const MAX_PANEL_PAGE_SIZE: u32 = 50;

/// `GET` / `PUT` / `DELETE /v2/panels/{panel_id}`：查询详情 / 修改 / 删除指令面板。
pub fn panel_path(panel_id: &str) -> String {
    format!("/v2/panels/{panel_id}")
}

/// `PUT /v2/panels/{panel_id}/target`：增删指令面板关联的用户 / 群。
pub fn panel_target_path(panel_id: &str) -> String {
    format!("/v2/panels/{panel_id}/target")
}

// ==================== GET /users/@me ====================

/// `GET /users/@me` 的响应：当前用户（机器人）详情。
///
/// 坑：
/// - `union_openid` / `union_user_account` 需要**特殊申请并配置**后才会返回；
///   官方另注明这两个字段「仅在单独拉取 member 信息时提供」，所以一律按可选处理。
/// - 官方响应示例里 `union_user_account` 是**空串**而不是缺字段，
///   判断「有没有」要同时看 `is_some()` 与 `is_empty()`。
/// - `share_url` / `welcome_msg` 只出现在官方「响应示例」里，字段表没有列出，按可选处理。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BotInfo {
    /// 用户 ID。
    pub id: String,
    /// 用户名。
    pub username: String,
    /// 头像 URL。
    #[serde(default)]
    pub avatar: Option<String>,
    /// 是否为机器人。
    #[serde(default)]
    pub bot: bool,
    /// 跨应用统一用户 OpenID（需特殊申请）。
    #[serde(default)]
    pub union_openid: Option<String>,
    /// 跨应用统一用户账号（需特殊申请）。
    #[serde(default)]
    pub union_user_account: Option<String>,
    /// 机器人分享链接（官方响应示例中出现，字段表未列出）。
    #[serde(default)]
    pub share_url: Option<String>,
    /// 欢迎语（官方响应示例中出现，字段表未列出）。
    #[serde(default)]
    pub welcome_msg: Option<String>,
}

// ==================== POST /v2/generate_url_link ====================

/// `POST /v2/generate_url_link` 请求体：生成机器人分享链接（用于邀请用户添加机器人为好友）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ShareLinkRequest {
    /// 回传给机器人后台的数据，**最长 32 字符**。
    /// 用户通过该链接添加机器人时，该值会透传给开发者。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}

/// `POST /v2/generate_url_link` 的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShareLinkResponse {
    /// 响应数据。官方把结果包了一层 `data`，没有平铺在顶层。
    #[serde(default)]
    pub data: ShareLinkData,
}

/// 分享链接数据（`ShareLinkResponse::data`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShareLinkData {
    /// 生成的分享链接。
    #[serde(default)]
    pub url: String,
}

// ==================== GET /v2/menu ====================

/// `GET /v2/menu` 的响应：查询全局自定义菜单。
///
/// 自定义菜单**仅 C2C（单聊）场景**有效，且是全局配置，设置后对所有用户生效，
/// 不支持按用户维度区分。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MenuResponse {
    /// 当前菜单的版本号。
    #[serde(default)]
    pub version: u64,
    /// 当前生效的菜单配置。**未设置过菜单时该字段为空**（`null` 或直接缺失）。
    #[serde(default)]
    pub menu: Option<Menu>,
}

/// 菜单配置。
///
/// ⚠️ `PUT /v2/menu` 是**全量覆盖**语义：传入的 `menu` 会替换原有的完整菜单配置，
/// 而不是增量合并 —— 想保留的旧菜单项必须原样带上，漏掉即视为删除。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Menu {
    /// 菜单项列表，**最多 10 个**，按列表顺序从左到右展示。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<MenuItem>,
}

/// 一级菜单项（`Menu::items`）。
///
/// 不同 `type` 只认各自的字段，其余字段**即便传了也会被忽略**（官方逐字段标注了「仅 … 时有效」）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MenuItem {
    /// 按钮名称，**最多 10 个字符，一个中文汉字算 2 个字符**。
    pub name: String,
    /// 按钮类型。
    #[serde(rename = "type")]
    pub item_type: MenuItemType,
    /// 子菜单列表，**仅 `type=menu` 时有效**。
    /// 子菜单**最多 5 个**，且**不支持再嵌套**子菜单。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_menu_items: Vec<SubMenuItem>,
    /// 发送的内容，**仅 `type=send_message` 时有效**。用户点击后该文本自动填入聊天输入框。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_message: Option<String>,
    /// 跳转链接 URL，**仅 `type=link` 时有效**，必须以 `https://` 开头。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// 开关配置，**仅 `type=switch` 时有效**。
    ///
    /// 字段名叫 `switch_config`（`switch` 是 Rust 关键字），JSON 名由 rename 保证为 `switch`。
    #[serde(rename = "switch", default, skip_serializing_if = "Option::is_none")]
    pub switch_config: Option<MenuSwitch>,
}

/// 二级菜单项（`MenuItem::sub_menu_items`）。
///
/// 与一级菜单项的差别：**不支持 `menu` 类型**（不能嵌套），名称上限放宽到 14 字符。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubMenuItem {
    /// 按钮名称，**最多 14 个字符，约 7 个中文汉字**。
    pub name: String,
    /// 按钮类型，**只能是 `send_message` 或 `link`**。
    #[serde(rename = "type")]
    pub item_type: SubMenuItemType,
    /// 发送的内容，**仅 `type=send_message` 时有效**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_message: Option<String>,
    /// 跳转链接 URL，**仅 `type=link` 时有效**，必须以 `https://` 开头。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// 开关配置（`MenuItem::switch_config`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MenuSwitch {
    /// 开关唯一标识。用户切换开关状态后会发送一条消息，消息 `ext` 中携带此字段：
    /// `switch_id` 为 `"search"` 时，打开后携带 `"search=1"`，关闭后不携带。
    #[serde(default)]
    pub switch_id: String,
    /// 开关的初始状态：`true` 默认打开，`false` 默认关闭。
    #[serde(default)]
    pub default: bool,
}

/// 一级菜单项类型。
///
/// 官方取值封闭（错误码 `40030014` 明确 `menu.type` 只支持这四种），故不加 `#[serde(other)]` 兜底。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuItemType {
    /// `switch` — 开关。
    Switch,
    /// `send_message` — 发送消息，文本自动填入聊天输入框。
    #[default]
    SendMessage,
    /// `link` — 链接跳转，必须 `https://` 开头。
    Link,
    /// `menu` — 含子菜单的折叠项。
    Menu,
}

/// 二级菜单项类型。官方取值封闭，且**明确不支持 `menu`**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubMenuItemType {
    /// `send_message` — 发送消息。
    #[default]
    SendMessage,
    /// `link` — 链接跳转。
    Link,
}

// ==================== PUT /v2/menu ====================

/// `PUT /v2/menu` 请求体：修改全局自定义菜单。
///
/// ⚠️ **全量覆盖**：官方原话「传入后会覆盖原有的完整菜单配置」。
/// 官方字段表把 `menu` 标为「否（可选）」，但语义上应始终传入；
/// 这里保留 [`Option`] 以贴合文档，`None` 会序列化成空 body `{}`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct MenuUpdateRequest {
    /// 菜单配置。传入后会覆盖原有的完整菜单配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<Menu>,
}

impl MenuUpdateRequest {
    /// 用一份完整菜单构造覆盖请求。
    pub fn new(menu: Menu) -> Self {
        Self { menu: Some(menu) }
    }
}

/// `PUT /v2/menu` 的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MenuUpdateResponse {
    /// 本次修改后的菜单版本号，可用于后续判断配置是否有变更。
    #[serde(default)]
    pub version: u64,
}

// ==================== 指令面板：公共枚举 ====================

/// 指令面板的生效场景（`scope`）。
///
/// 查询与创建接口的**取值集合相同**（都是这四种，错误码 `40030011` 列出了全部），
/// 差别只在创建时的**约束**：`channel` / `dm` 只能是全局面板。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelScope {
    /// `c2c` — 单聊。支持 `target_type=specific`（配合 `user_openids`）。
    #[default]
    C2c,
    /// `group` — 群聊。支持 `target_type=specific`（配合 `group_openids`）。
    Group,
    /// `channel` — 文字子频道。**仅支持全局配置（`target_type=all`）**。
    Channel,
    /// `dm` — 频道私信。**仅支持全局配置（`target_type=all`）**。
    ///
    /// serde 的 `snake_case` 会把变体名 `Dm` 转成 `d_m`，与官方取值不符，故显式 rename。
    #[serde(rename = "dm")]
    Dm,
}

impl PanelScope {
    /// 官方 JSON 取值。
    pub const fn as_str(self) -> &'static str {
        match self {
            PanelScope::C2c => "c2c",
            PanelScope::Group => "group",
            PanelScope::Channel => "channel",
            PanelScope::Dm => "dm",
        }
    }

    /// 是否支持 `target_type=specific`。官方：**只有 `c2c` / `group` 支持**，
    /// `channel` / `dm` 传 `specific` 会返回 `40030012`。
    pub const fn supports_specific_target(self) -> bool {
        matches!(self, PanelScope::C2c | PanelScope::Group)
    }
}

/// 面板作用范围（`target_type`）。
///
/// 官方取值封闭（`40030012`：仅 `all` / `specific`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelTargetType {
    /// `all` — 对该场景下所有用户 / 群生效（全局配置）。
    #[default]
    All,
    /// `specific` — 仅对指定用户 / 群生效。
    /// **仅 `c2c` / `group` 场景可能为 `specific`**；`channel` / `dm` 只能传 `all`。
    Specific,
}

/// 面板元素类型（`panel.items[].type`）。
///
/// 官方取值封闭（`40030015`：仅 `command` / `link`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelItemType {
    /// `command` — 指令。用户点击后 `name` 会填入聊天输入框。
    #[default]
    Command,
    /// `link` — 链接跳转，必须配合 `link` 字段。
    Link,
}

// ==================== 指令面板：公共结构 ====================

/// 面板元素（指令或链接项）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PanelItem {
    /// 元素名称，**最多 14 个字符，约 7 个中文汉字**。
    /// `type=command` 时用户点击后该内容填入聊天输入框；`type=link` 时仅用于面板展示。
    pub name: String,
    /// 元素描述，在面板中展示给用户，**最多 30 个字符，约 15 个中文汉字**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// 元素类型。
    #[serde(rename = "type")]
    pub item_type: PanelItemType,
    /// 是否仅管理员可操作：`true` 仅频道 / 群管理员可点击，`false` 所有用户可点击。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_admin: Option<bool>,
    /// 跳转链接 URL，**仅 `type=link` 时有效**。用户点击后在浏览器中打开该地址。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// 面板配置内容。
///
/// ⚠️ `PUT /v2/panels/{panel_id}` 传入的 `panel` 会**覆盖原有的面板元素列表和备注**，
/// 但**不影响已关联的用户 / 群列表** —— 关联对象要用 [`PanelTargetRequest`] 单独改。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Panel {
    /// 面板元素列表，**一个指令面板最多配置 20 个元素**（[`MAX_PANEL_ITEMS`]）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PanelItem>,
    /// 面板备注，**最多 255 个字符**，用于开发者标记面板用途，**不对用户展示**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
    /// 当前版本号。请求里可省略，响应里也常缺省，故按可选处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

// ==================== GET /v2/panels ====================

/// `GET /v2/panels` 的查询参数。
///
/// `scope` **必填**（官方：「必须传入 scope 参数进行场景筛选」）；
/// 首次请求不传 `cursor`（或传空串），后续请求传上一页响应里的 `next_cursor`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelListQuery {
    /// 生效场景，必填。
    pub scope: PanelScope,
    /// 分页游标。首次请求不传或传空串。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 每页拉取条数，**默认 20，最大 50**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl PanelListQuery {
    /// 第一页查询（不带游标、不带 limit，由服务端按默认 20 条返回）。
    pub fn new(scope: PanelScope) -> Self {
        Self { scope, cursor: None, limit: None }
    }

    /// 设置每页条数（服务端上限 50，超出会返回 `40030013`）。
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// 设置分页游标（通常直接传上一页的 `next_cursor`）。
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

/// 面板记录（列表项 / 详情）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelRecord {
    /// 面板 ID。
    pub panel_id: String,
    /// 生效场景。
    pub scope: PanelScope,
    /// 作用范围。
    pub target_type: PanelTargetType,
    /// 面板配置内容。
    pub panel: Panel,
    /// 面板创建时间，RFC3339 格式（如 `2024-01-15T10:30:00Z`）。
    /// 官方列表响应示例里**没有**这个字段，故按可选处理。
    #[serde(default)]
    pub created_at: Option<String>,
    /// 面板更新时间，RFC3339 格式。同上，列表示例里没有。
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 面板版本号。
    #[serde(default)]
    pub version: u64,
}

/// `GET /v2/panels` 的响应：分页拉取指定场景下已生效的指令面板列表，按设置时间**倒序**排列。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelListResponse {
    /// 面板记录列表，按设置时间倒序排列。
    #[serde(default)]
    pub records: Vec<PanelRecord>,
    /// 下一页游标。**空串表示已到最后一页，无更多数据**。
    #[serde(default)]
    pub next_cursor: String,
    /// 是否已拉取到最后一页：`true` 表示无更多数据。
    #[serde(default)]
    pub is_end: bool,
}

impl PanelListResponse {
    /// 是否还有下一页。
    ///
    /// 两个条件都要看：官方既说「`next_cursor` 空串表示已到最后一页」，
    /// 又单独给了 `is_end` 标记，因此任一条件命中即认为没有更多数据。
    pub fn has_more(&self) -> bool {
        !self.is_end && !self.next_cursor.is_empty()
    }
}

// ==================== POST /v2/panels ====================

/// `POST /v2/panels` 请求体：创建指令面板。
///
/// - 一个机器人**最多创建 20 个指令面板**（[`MAX_PANELS_PER_BOT`]）。
/// - `c2c` / `group` 支持 `target_type=specific`；`channel` / `dm` **只能传 `all`**。
#[derive(Debug, Clone, Serialize)]
pub struct PanelCreateRequest {
    /// 生效场景，必填。
    pub scope: PanelScope,
    /// 作用范围。官方字段表标为可选；省略时服务端行为未明说，建议显式传。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_type: Option<PanelTargetType>,
    /// 用户 openid 列表，**仅 `c2c` 且 `target_type=specific` 时有效，一次最多 20 个**。
    /// 后续可用 [`PanelTargetRequest`] 增删。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_openids: Vec<String>,
    /// 群 openid 列表，**仅 `group` 且 `target_type=specific` 时有效，一次最多 20 个**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
    /// 面板配置内容，必填。
    pub panel: Panel,
}

impl PanelCreateRequest {
    /// 创建该场景下的全局面板（`target_type=all`）。
    pub fn global(scope: PanelScope, panel: Panel) -> Self {
        Self {
            scope,
            target_type: Some(PanelTargetType::All),
            user_openids: Vec::new(),
            group_openids: Vec::new(),
            panel,
        }
    }

    /// 创建仅对指定用户生效的 C2C 面板（`scope=c2c` + `target_type=specific`）。
    pub fn for_users(panel: Panel, user_openids: Vec<String>) -> Self {
        Self {
            scope: PanelScope::C2c,
            target_type: Some(PanelTargetType::Specific),
            user_openids,
            group_openids: Vec::new(),
            panel,
        }
    }

    /// 创建仅对指定群生效的群面板（`scope=group` + `target_type=specific`）。
    pub fn for_groups(panel: Panel, group_openids: Vec<String>) -> Self {
        Self {
            scope: PanelScope::Group,
            target_type: Some(PanelTargetType::Specific),
            user_openids: Vec::new(),
            group_openids,
            panel,
        }
    }
}

/// `POST /v2/panels` 的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelCreateResponse {
    /// 新创建的面板 ID。后续修改 / 删除 / 查询详情均需使用此 ID。
    #[serde(default)]
    pub panel_id: String,
}

// ==================== GET /v2/panels/{panel_id} ====================

/// `GET /v2/panels/{panel_id}` 的响应：指定指令面板的完整配置详情。
///
/// 比列表多出 `user_openids` / `group_openids`（关联对象），
/// 且它们**仅 `target_type=specific` 时返回**。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelDetailResponse {
    /// 面板 ID。
    pub panel_id: String,
    /// 生效场景。
    pub scope: PanelScope,
    /// 作用范围。
    pub target_type: PanelTargetType,
    /// 面板配置内容。
    pub panel: Panel,
    /// 面板创建时间，RFC3339 格式。
    #[serde(default)]
    pub created_at: Option<String>,
    /// 面板更新时间，RFC3339 格式。
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 面板版本号。
    #[serde(default)]
    pub version: u64,
    /// 关联的用户 openid 列表，**仅 `c2c` 且 `target_type=specific` 时返回，最多 1000 条**。
    #[serde(default)]
    pub user_openids: Vec<String>,
    /// 关联的群 openid 列表，**仅 `group` 且 `target_type=specific` 时返回，最多 1000 条**。
    #[serde(default)]
    pub group_openids: Vec<String>,
}

// ==================== PUT /v2/panels/{panel_id} ====================

/// `PUT /v2/panels/{panel_id}` 请求体：修改指令面板。
///
/// ⚠️ `panel` 为**全量覆盖**：面板元素列表与备注都会被替换，
/// 但**不影响已关联的用户 / 群列表**（关联对象用 [`PanelTargetRequest`] 改）。
#[derive(Debug, Clone, Serialize)]
pub struct PanelUpdateRequest {
    /// 面板配置内容，必填。
    pub panel: Panel,
}

/// `PUT /v2/panels/{panel_id}` 的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelUpdateResponse {
    /// 本次修改后的面板版本号。
    #[serde(default)]
    pub version: u64,
}

// ==================== PUT /v2/panels/{panel_id}/target ====================

/// 关联对象操作类型（`op`）。官方取值封闭（`40030017`：仅 `add` / `del`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelTargetOp {
    /// `add` — 添加关联对象。
    Add,
    /// `del` — 移除关联对象。
    Del,
}

/// `PUT /v2/panels/{panel_id}/target` 请求体：增删指令面板关联的用户 / 群。
///
/// 约束（都会报错，调用前应自行判断）：
/// - `channel` / `dm` 是全局配置，不支持本接口 → `40030018`；
/// - `target_type=all` 的全局面板不支持添加关联对象 → `40030021`；
/// - `c2c` 场景只认 `user_openids`，`group` 场景只认 `group_openids`。
#[derive(Debug, Clone, Serialize)]
pub struct PanelTargetRequest {
    /// 操作类型，必填。
    pub op: PanelTargetOp,
    /// 用户 openid 列表，**仅 `c2c` 场景有效，一次最多 20 个**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_openids: Vec<String>,
    /// 群 openid 列表，**仅 `group` 场景有效，一次最多 20 个**。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
}

impl PanelTargetRequest {
    /// 空请求，随后用 `user_openids` / `group_openids` 填目标。
    pub fn new(op: PanelTargetOp) -> Self {
        Self { op, user_openids: Vec::new(), group_openids: Vec::new() }
    }
}

// ==================== 无响应体接口 ====================

/// 「无响应体」接口的响应占位。
///
/// `DELETE /v2/panels/{panel_id}` 与 `PUT /v2/panels/{panel_id}/target`
/// 官方标注响应为「无」，响应示例实际是 `{}`。
///
/// 这里刻意不用 `()`：`serde_json` 的 `()` 只接受 `null`，
/// 拿它接 `{}` 会得到 "invalid type: map, expected unit"，
/// 于是 `client::decode_response::<()>` 会把一次成功调用判成协议错误。
///
/// 反过来，`{}` 专用的结构体也接不住**空 body**（`decode_response` 把空体视作
/// `Value::Null`）。所以这里用 `#[serde(from)]` 先把任意 JSON 收成
/// `Option<Value>`，`null` 与 `{}` 都能落进来。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "Option<serde_json::Value>")]
pub struct EmptyResponse {}

impl From<Option<serde_json::Value>> for EmptyResponse {
    fn from(_: Option<serde_json::Value>) -> Self {
        Self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- GET /users/@me ----------

    #[test]
    fn bot_info_parses_official_example() {
        // 官方「响应示例」原文
        let raw = r#"{
          "id": "5777414462219517083",
          "username": "阳光小助手",
          "avatar": "https://thirdqq.qlogo.cn/g?b=oidb&k=AbCdEfGhIjKlMnOpQrStUv&kti=xyzABC&s=0&t=1781676795",
          "bot": true,
          "union_openid": "9F2E872045CCCC5948BEAF5B5FCCDF22",
          "union_user_account": "",
          "share_url": "https://qun.qq.com/qunpro/robot/qunshare?robot_uin=3889007780&robot_appid=102083127&biz_type=0",
          "welcome_msg": "欢迎加入我们的群聊"
        }"#;
        let info: BotInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(info.id, "5777414462219517083");
        assert_eq!(info.username, "阳光小助手");
        assert!(info.bot);
        assert!(info.avatar.unwrap().contains("thirdqq.qlogo.cn"));
        assert_eq!(info.union_openid.as_deref(), Some("9F2E872045CCCC5948BEAF5B5FCCDF22"));
        // 官方示例里是空串而非缺字段 —— 别用 is_some() 判断「有没有」
        assert_eq!(info.union_user_account.as_deref(), Some(""));
        assert!(info.share_url.unwrap().contains("qun.qq.com"));
        assert_eq!(info.welcome_msg.as_deref(), Some("欢迎加入我们的群聊"));
    }

    #[test]
    fn bot_info_tolerates_missing_optional_fields() {
        // union_openid / union_user_account 需特殊申请；avatar 也可能缺
        let info: BotInfo = serde_json::from_str(r#"{"id":"1","username":"bot"}"#).unwrap();
        assert_eq!(info.id, "1");
        assert!(!info.bot);
        assert!(info.avatar.is_none());
        assert!(info.union_openid.is_none());
        assert!(info.union_user_account.is_none());
        assert!(info.share_url.is_none());
    }

    // ---------- POST /v2/generate_url_link ----------

    #[test]
    fn share_link_request_matches_official_example() {
        let req = ShareLinkRequest { callback_data: Some("custom_data_123".into()) };
        assert_eq!(serde_json::to_value(&req).unwrap(), json!({ "callback_data": "custom_data_123" }));
    }

    #[test]
    fn share_link_request_omits_absent_callback_data() {
        // callback_data 是可选字段（最长 32 字符），不传时不应出现该键
        let req = ShareLinkRequest::default();
        assert_eq!(serde_json::to_value(&req).unwrap(), json!({}));
    }

    #[test]
    fn share_link_response_parses_official_example() {
        let raw = r#"{
          "data": {
            "url": "https://qun.qq.com/qunpro/robot/qunshare?robot_appid=1234567890&robot_uin=12345678&data=xxx"
          }
        }"#;
        let resp: ShareLinkResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.data.url.starts_with("https://qun.qq.com/qunpro/robot/qunshare"));
        assert!(resp.data.url.contains("robot_appid=1234567890"));
    }

    // ---------- GET /v2/menu ----------

    #[test]
    fn menu_response_parses_official_example() {
        let raw = r#"{
          "menu": {
            "items": [
              {
                "type": "send_message",
                "name": "帮助",
                "send_message": "/help"
              }
            ]
          },
          "version": 1
        }"#;
        let resp: MenuResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.version, 1);
        let menu = resp.menu.unwrap();
        assert_eq!(menu.items.len(), 1);
        let item = &menu.items[0];
        assert_eq!(item.name, "帮助");
        assert_eq!(item.item_type, MenuItemType::SendMessage);
        assert_eq!(item.send_message.as_deref(), Some("/help"));
        assert!(item.link.is_none());
        assert!(item.sub_menu_items.is_empty());
        assert!(item.switch_config.is_none());
    }

    #[test]
    fn menu_response_without_menu_is_ok() {
        // 官方：未设置过菜单时该字段为空
        let with_null: MenuResponse = serde_json::from_str(r#"{"version":0,"menu":null}"#).unwrap();
        assert!(with_null.menu.is_none());

        let missing: MenuResponse = serde_json::from_str("{}").unwrap();
        assert!(missing.menu.is_none());
        assert_eq!(missing.version, 0);
    }

    #[test]
    fn menu_switch_is_serialized_as_switch() {
        // 字段叫 switch_config，JSON 名必须是 switch
        let item = MenuItem {
            name: "搜索".into(),
            item_type: MenuItemType::Switch,
            switch_config: Some(MenuSwitch { switch_id: "search".into(), default: true }),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            json!({
              "type": "switch",
              "name": "搜索",
              "switch": { "switch_id": "search", "default": true }
            })
        );
    }

    // ---------- PUT /v2/menu ----------

    #[test]
    fn menu_update_request_matches_official_example() {
        // 官方「创建包含多种类型的菜单」请求示例
        let req = MenuUpdateRequest::new(Menu {
            items: vec![
                MenuItem {
                    name: "帮助".into(),
                    item_type: MenuItemType::SendMessage,
                    send_message: Some("/help".into()),
                    ..Default::default()
                },
                MenuItem {
                    name: "官网".into(),
                    item_type: MenuItemType::Link,
                    link: Some("https://example.com".into()),
                    ..Default::default()
                },
                MenuItem {
                    name: "更多".into(),
                    item_type: MenuItemType::Menu,
                    sub_menu_items: vec![SubMenuItem {
                        name: "设置".into(),
                        item_type: SubMenuItemType::SendMessage,
                        send_message: Some("/settings".into()),
                        link: None,
                    }],
                    ..Default::default()
                },
            ],
        });

        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
              "menu": {
                "items": [
                  { "type": "send_message", "name": "帮助", "send_message": "/help" },
                  { "type": "link", "name": "官网", "link": "https://example.com" },
                  {
                    "type": "menu",
                    "name": "更多",
                    "sub_menu_items": [
                      { "type": "send_message", "name": "设置", "send_message": "/settings" }
                    ]
                  }
                ]
              }
            })
        );
    }

    #[test]
    fn menu_update_response_parses_official_example() {
        let resp: MenuUpdateResponse = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(resp.version, 1);
    }

    // ---------- GET /v2/panels ----------

    #[test]
    fn panel_list_response_parses_official_example() {
        let raw = r#"{
          "records": [
            {
              "panel_id": "p_102030405_x8k2",
              "scope": "c2c",
              "target_type": "all",
              "panel": {
                "items": [
                  {
                    "type": "command",
                    "name": "查询天气",
                    "desc": "查询当前天气"
                  }
                ]
              },
              "version": 1
            }
          ],
          "next_cursor": "",
          "is_end": true
        }"#;
        let resp: PanelListResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.is_end);
        assert_eq!(resp.next_cursor, "");
        assert!(!resp.has_more(), "空串游标 + is_end 应判定为没有下一页");
        assert_eq!(resp.records.len(), 1);

        let rec = &resp.records[0];
        assert_eq!(rec.panel_id, "p_102030405_x8k2");
        assert_eq!(rec.scope, PanelScope::C2c);
        assert_eq!(rec.target_type, PanelTargetType::All);
        assert_eq!(rec.version, 1);
        assert_eq!(rec.panel.items.len(), 1);
        assert_eq!(rec.panel.items[0].item_type, PanelItemType::Command);
        assert_eq!(rec.panel.items[0].name, "查询天气");
        assert_eq!(rec.panel.items[0].desc.as_deref(), Some("查询当前天气"));
        assert!(rec.panel.items[0].only_admin.is_none());
        // 官方列表示例里没有 created_at / updated_at
        assert!(rec.created_at.is_none());
        assert!(rec.updated_at.is_none());
    }

    #[test]
    fn panel_list_response_tolerates_missing_optional_fields() {
        let resp: PanelListResponse = serde_json::from_str("{}").unwrap();
        assert!(resp.records.is_empty());
        assert_eq!(resp.next_cursor, "");
        assert!(!resp.is_end);
        assert!(!resp.has_more());
    }

    #[test]
    fn panel_list_response_has_more_only_when_cursor_present() {
        let more: PanelListResponse =
            serde_json::from_str(r#"{"next_cursor":"c_2","is_end":false}"#).unwrap();
        assert!(more.has_more());

        // 游标非空但 is_end=true：以 is_end 为准
        let ended: PanelListResponse =
            serde_json::from_str(r#"{"next_cursor":"c_2","is_end":true}"#).unwrap();
        assert!(!ended.has_more());
    }

    #[test]
    fn panel_list_query_serializes_official_example() {
        // 官方请求示例：GET /v2/panels?scope=c2c&limit=10
        // 查询串由 serde 驱动（reqwest 的 query 特性），这里校验字段名与取值。
        let q = PanelListQuery::new(PanelScope::C2c).with_limit(10);
        assert_eq!(serde_json::to_value(&q).unwrap(), serde_json::json!({"scope": "c2c", "limit": 10}));
    }

    #[test]
    fn panel_list_query_omits_absent_cursor() {
        // 不传 cursor / limit 时，序列化结果里不应出现这两个键
        let q = PanelListQuery::new(PanelScope::Dm);
        assert_eq!(serde_json::to_value(&q).unwrap(), serde_json::json!({"scope": "dm"}));
    }

    // ---------- POST /v2/panels ----------

    #[test]
    fn panel_create_request_matches_official_global_example() {
        let req = PanelCreateRequest::global(
            PanelScope::C2c,
            Panel {
                items: vec![
                    PanelItem {
                        name: "查询天气".into(),
                        desc: Some("查询当前天气".into()),
                        item_type: PanelItemType::Command,
                        ..Default::default()
                    },
                    PanelItem {
                        name: "更多服务".into(),
                        item_type: PanelItemType::Link,
                        link: Some("https://example.com".into()),
                        ..Default::default()
                    },
                ],
                remark: Some("C2C面板".into()),
                version: None,
            },
        );

        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
              "scope": "c2c",
              "target_type": "all",
              "panel": {
                "items": [
                  { "type": "command", "name": "查询天气", "desc": "查询当前天气" },
                  { "type": "link", "name": "更多服务", "link": "https://example.com" }
                ],
                "remark": "C2C面板"
              }
            })
        );
    }

    #[test]
    fn panel_create_request_matches_official_specific_example() {
        let req = PanelCreateRequest::for_groups(
            Panel {
                items: vec![PanelItem {
                    name: "群签到".into(),
                    desc: Some("每日签到".into()),
                    item_type: PanelItemType::Command,
                    ..Default::default()
                }],
                remark: None,
                version: None,
            },
            vec!["openid_group_001".into(), "openid_group_002".into()],
        );

        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
              "scope": "group",
              "target_type": "specific",
              "group_openids": ["openid_group_001", "openid_group_002"],
              "panel": {
                "items": [
                  { "type": "command", "name": "群签到", "desc": "每日签到" }
                ]
              }
            })
        );
    }

    #[test]
    fn panel_create_request_for_users_uses_c2c_scope() {
        let req = PanelCreateRequest::for_users(
            Panel { items: vec![], remark: None, version: None },
            vec!["openid_user_001".into()],
        );
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
              "scope": "c2c",
              "target_type": "specific",
              "user_openids": ["openid_user_001"],
              "panel": {}
            })
        );
    }

    #[test]
    fn panel_create_response_parses_official_example() {
        let resp: PanelCreateResponse =
            serde_json::from_str(r#"{"panel_id":"p_x8k2x8k2x8k2"}"#).unwrap();
        assert_eq!(resp.panel_id, "p_x8k2x8k2x8k2");
    }

    // ---------- GET /v2/panels/{panel_id} ----------

    #[test]
    fn panel_detail_response_parses_official_example() {
        let raw = r#"{
          "panel_id": "p_x8k2x8k2x8k2",
          "scope": "group",
          "target_type": "specific",
          "panel": {
            "items": [
              {
                "type": "command",
                "name": "群签到",
                "desc": "每日签到"
              }
            ]
          },
          "version": 1,
          "user_openids": [],
          "group_openids": [
            "openid_group_001"
          ]
        }"#;
        let resp: PanelDetailResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.panel_id, "p_x8k2x8k2x8k2");
        assert_eq!(resp.scope, PanelScope::Group);
        assert_eq!(resp.target_type, PanelTargetType::Specific);
        assert_eq!(resp.version, 1);
        assert!(resp.user_openids.is_empty());
        assert_eq!(resp.group_openids, vec!["openid_group_001".to_string()]);
        assert_eq!(resp.panel.items[0].name, "群签到");
        // 官方详情示例里同样没有 created_at / updated_at
        assert!(resp.created_at.is_none());
        assert!(resp.updated_at.is_none());
    }

    #[test]
    fn panel_paths_match_official_urls() {
        assert_eq!(USERS_ME_PATH, "/users/@me");
        assert_eq!(GENERATE_URL_LINK_PATH, "/v2/generate_url_link");
        assert_eq!(MENU_PATH, "/v2/menu");
        assert_eq!(PANELS_PATH, "/v2/panels");
        assert_eq!(panel_path("p_x8k2x8k2x8k2"), "/v2/panels/p_x8k2x8k2x8k2");
        assert_eq!(
            panel_target_path("p_x8k2x8k2x8k2"),
            "/v2/panels/p_x8k2x8k2x8k2/target"
        );
    }

    // ---------- PUT /v2/panels/{panel_id} ----------

    #[test]
    fn panel_update_request_matches_official_example() {
        let req = PanelUpdateRequest {
            panel: Panel {
                items: vec![PanelItem {
                    name: "新指令".into(),
                    desc: Some("更新后的指令".into()),
                    item_type: PanelItemType::Command,
                    ..Default::default()
                }],
                remark: Some("更新备注".into()),
                version: None,
            },
        };

        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
              "panel": {
                "items": [
                  { "type": "command", "name": "新指令", "desc": "更新后的指令" }
                ],
                "remark": "更新备注"
              }
            })
        );
    }

    #[test]
    fn panel_update_response_parses_official_example() {
        let resp: PanelUpdateResponse = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(resp.version, 1);
    }

    // ---------- DELETE /v2/panels/{panel_id} ----------

    #[test]
    fn empty_response_accepts_official_empty_object() {
        // 官方 DELETE 响应示例是 {}；用 () 接会报 invalid type: map, expected unit
        let resp: EmptyResponse = serde_json::from_str("{}").unwrap();
        let _ = resp;
    }

    // ---------- PUT /v2/panels/{panel_id}/target ----------

    #[test]
    fn panel_target_request_matches_official_examples() {
        let add = PanelTargetRequest {
            op: PanelTargetOp::Add,
            user_openids: vec![],
            group_openids: vec!["openid_group_003".into()],
        };
        assert_eq!(
            serde_json::to_value(&add).unwrap(),
            json!({ "op": "add", "group_openids": ["openid_group_003"] })
        );

        let del = PanelTargetRequest {
            op: PanelTargetOp::Del,
            user_openids: vec!["openid_user_001".into()],
            group_openids: vec![],
        };
        assert_eq!(
            serde_json::to_value(&del).unwrap(),
            json!({ "op": "del", "user_openids": ["openid_user_001"] })
        );
    }

    #[test]
    fn panel_target_request_new_omits_both_id_lists() {
        let req = PanelTargetRequest::new(PanelTargetOp::Del);
        assert_eq!(serde_json::to_value(&req).unwrap(), json!({ "op": "del" }));
    }

    // ---------- 枚举取值 ----------

    #[test]
    fn enum_json_values_match_docs() {
        assert_eq!(serde_json::to_value(PanelScope::C2c).unwrap(), json!("c2c"));
        assert_eq!(serde_json::to_value(PanelScope::Group).unwrap(), json!("group"));
        assert_eq!(serde_json::to_value(PanelScope::Channel).unwrap(), json!("channel"));
        // serde 的 snake_case 会把 Dm 变成 d_m，这里靠显式 rename 兜住
        assert_eq!(serde_json::to_value(PanelScope::Dm).unwrap(), json!("dm"));

        assert_eq!(serde_json::to_value(PanelTargetType::All).unwrap(), json!("all"));
        assert_eq!(serde_json::to_value(PanelTargetType::Specific).unwrap(), json!("specific"));
        assert_eq!(serde_json::to_value(PanelItemType::Command).unwrap(), json!("command"));
        assert_eq!(serde_json::to_value(PanelItemType::Link).unwrap(), json!("link"));
        assert_eq!(serde_json::to_value(PanelTargetOp::Add).unwrap(), json!("add"));
        assert_eq!(serde_json::to_value(PanelTargetOp::Del).unwrap(), json!("del"));

        assert_eq!(serde_json::to_value(MenuItemType::Switch).unwrap(), json!("switch"));
        assert_eq!(serde_json::to_value(MenuItemType::SendMessage).unwrap(), json!("send_message"));
        assert_eq!(serde_json::to_value(MenuItemType::Link).unwrap(), json!("link"));
        assert_eq!(serde_json::to_value(MenuItemType::Menu).unwrap(), json!("menu"));
        assert_eq!(
            serde_json::to_value(SubMenuItemType::SendMessage).unwrap(),
            json!("send_message")
        );
        assert_eq!(serde_json::to_value(SubMenuItemType::Link).unwrap(), json!("link"));
    }

    #[test]
    fn only_c2c_and_group_support_specific_targets() {
        assert!(PanelScope::C2c.supports_specific_target());
        assert!(PanelScope::Group.supports_specific_target());
        assert!(!PanelScope::Channel.supports_specific_target());
        assert!(!PanelScope::Dm.supports_specific_target());
    }

    #[test]
    fn limits_match_docs() {
        assert_eq!(MAX_PANEL_ITEMS, 20);
        assert_eq!(MAX_PANELS_PER_BOT, 20);
        assert_eq!(DEFAULT_PANEL_PAGE_SIZE, 20);
        assert_eq!(MAX_PANEL_PAGE_SIZE, 50);
    }
}
