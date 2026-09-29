# QQ 官方 API v2 协议速查

> 来源：QQ 机器人官方文档 <https://bot.q.qq.com/wiki/develop/api-v2/>
> 抓取时间：文档标注最后更新 2026-09（部分页面 2026-07）
> 用途：实现 `qqbot-api` crate 时的对照清单

---

## 1. 基础信息

| 项 | 值 |
|---|---|
| OpenAPI 基址 | `https://api.bot.qq.com` |
| 取 token | `POST https://api.bot.qq.com/app/getAppAccessToken` |
| 鉴权头 | `Authorization: QQBot {ACCESS_TOKEN}` |
| Content-Type | `application/json; charset=utf-8` |
| 链路追踪 | 响应头 `X-Tps-trace-ID` / 响应体 `trace_id` |

### 1.1 获取 access_token

```bash
curl --location 'https://api.bot.qq.com/app/getAppAccessToken' \
  --header 'Content-Type: application/json' \
  --data '{ "appId": "APPID", "clientSecret": "CLIENTSECRET" }'
```

返回：

```json
{ "access_token": "ACCESS_TOKEN", "expires_in": "7200" }
```

**规则**：

- 有效期默认 7200s
- 有效期内重复获取返回**相同**值
- 距过期 **60s 内**获取会返回**新** token，旧 token 在 60s 内仍有效
- 失败时 **HTTP 仍为 200**，需看响应体 `code`

**错误码**：`100001` Too many requests；`100007` appid invalid；`100016` invalid appid or secret；`10004` 机器人不存在

---

## 2. 响应结构

成功：直接返回业务数据。

```json
{ "id": "ROBOT1.0_xxx", "timestamp": "2026-07-21T10:30:00+08:00" }
```

失败：

```json
{ "err_code": 40034005, "message": "回复消息msg_id已过期", "trace_id": "4a8a6156..." }
```

> ⚠️ **只依据 `err_code` 判断成败**，不要依据 `message`（内容会随时调整）。

### HTTP 状态码

| 值 | 含义 |
|---|---|
| 200 | 成功 |
| 204 | 成功，无包体（删除操作） |
| 201 / 202 | 异步操作成功，但仍返回 error body，需特殊处理 |
| 401 | 认证失败 |
| 404 | 未找到 API |
| 405 | Method 不允许 |
| 429 | 频率限制 |
| 500 / 504 | 处理失败 |

### 公共错误码

| 值 | 含义 |
|---|---|
| 10001 | UnknownAccount |
| 10003 | UnknownChannel |
| 10004 | UnknownGuild |
| 11251 / 11261 | ErrorWrongAppid |
| 11253 | ErrorCheckAppPrivilegeNotPass（未获接口权限） |
| 11254 | ErrorInterfaceForbidden（接口被封禁） |
| 11281 | ErrorCheckAdminFailed（系统错误，**最多重试一次**） |
| 11282 | ErrorCheckAdminNotPass（逻辑错误，提示用户授权） |
| 11252 | ErrorCheckAppPrivilegeFailed（系统错误，**最多重试一次**） |

---

## 3. 唯一身份机制

| 标识 | 场景 |
|---|---|
| `user_openid` | 单聊场景的用户标识 |
| `group_openid` | 群标识 |
| `member_openid` | 用户在群内的标识 |

> ⚠️ 不同 bot(AppID) 拿到的 openid **互不相同**。跨业务关联需平台后续的 unionid 机制。

---

## 3.5 实测验证记录（2026-09-28，真实机器人）

用真实凭据连线上验证过的事实，**与文档措辞存在偏差的地方以本节为准**：

| 项 | 实测结果 |
|---|---|
| `POST /app/getAppAccessToken` | 返回 `{"access_token":"...","expires_in":"7125"}`，**`expires_in` 是字符串**，长度 76 |
| `GET /gateway/bot` | 返回 `url = wss://api.sgroup.qq.com/websocket`（**不是** api.bot.qq.com），`shards = 1`，`session_start_limit.remaining = 1500` |
| op 10 Hello | `heartbeat_interval = 41250`（毫秒） |
| op 1 → op 11 | 心跳发出后约 42ms 收到 ACK，链路正常 |
| READY | `d.user.username` 即机器人昵称；`d.shard = [0,1]` |

### ⚠️ 最大的坑：Identify 的 token 必须带 `QQBot ` 前缀

文档在字段表里写的是「token 格式为 "QQBot {AccessToken}"」，很容易被当成普通描述而漏掉。
**漏掉前缀时服务端不会报错**，而是在收到 Identify 后约 100ms 直接回 **op 9 InvalidSession**，
表现为「WebSocket 连上了、Hello 也收到了、但永远等不到 READY，然后无限重连」。

```json
{
  "op": 2,
  "d": {
    "token": "QQBot {AccessToken}",
    "intents": 33554432,
    "shard": [0, 1],
    "properties": { "$os": "windows", "$browser": "qqbot-rs", "$device": "qqbot-rs" }
  }
}
```

注意 HTTP 请求头的 `Authorization` 用的是同一套格式：`QQBot {AccessToken}`。

---

## 4. WebSocket 网关

### 4.1 Payload 结构

```json
{ "id": "event_id", "op": 0, "d": {}, "s": 42, "t": "GATEWAY_EVENT_NAME" }
```

| 字段 | 描述 |
|---|---|
| `id` | 事件 id |
| `op` | opcode |
| `s` | 下行序列号，心跳需回传最新 `s` |
| `t` | 事件类型（op=0 时） |
| `d` | 事件内容（op=0 时） |

### 4.2 OpCode

| Code | 名称 | 方向 | 描述 |
|---|---|---|---|
| 0 | Dispatch | Receive | 服务端消息推送 |
| 1 | Heartbeat | Send/Receive | 心跳 |
| 2 | Identify | Send | 客户端鉴权 |
| 6 | Resume | Send | 恢复连接 |
| 7 | Reconnect | Receive | 服务端要求重连 |
| 9 | Invalid Session | Receive | identify/resume 参数错误 |
| 10 | Hello | Receive | 连接建立后第一条消息 |
| 11 | Heartbeat ACK | Receive/Reply | 心跳成功 |
| 12 | HTTP Callback ACK | Reply | **仅 webhook 模式**，表示收到推送 |
| 13 | 回调地址验证 | Receive | **仅 webhook 模式** |

### 4.3 Identify / READY

鉴权成功后下发 READY：

```json
{
  "op": 0, "s": 1, "t": "READY",
  "d": {
    "version": 1,
    "session_id": "082ee18c-0be3-491b-9d8b-fbd95c51673a",
    "user": { "id": "6158788878435714165", "username": "群pro测试机器人", "bot": true },
    "shard": [0, 0]
  }
}
```

### 4.4 心跳

`d` 为客户端收到的最新 `s`；首次连接传 `null`。

```json
{ "op": 1, "d": 251 }
```

成功回 `{ "op": 11 }`。

### 4.5 Resume

```json
{ "op": 6, "d": { "token": "my_token", "session_id": "session_id_i_stored", "seq": 1337 } }
```

补发完成后下发 RESUMED：`{ "op": 0, "s": 2002, "t": "RESUMED", "d": "" }`

> 建议处理完事件后记录 `s`，Resume 时传入，网关会自动补发该 seq 之后的事件。

### 4.6 Shard

`intents` 同级参数，两元素数组 `[i, n]`。例如 `[0, 4]` 表示共 4 片、当前是第 0 片；需继续建立 `[1,4]`、`[2,4]`、`[3,4]` 才能完整接收。

---

## 5. Intents

| Intent | 位 | 事件 |
|---|---|---|
| GUILDS | 1 << 0 | GUILD_CREATE / UPDATE / DELETE、CHANNEL_CREATE / UPDATE / DELETE |
| GUILD_MEMBERS | 1 << 1 | GUILD_MEMBER_ADD / UPDATE / REMOVE |
| GUILD_MESSAGES | 1 << 9 | MESSAGE_CREATE / DELETE（仅**私域**） |
| GUILD_MESSAGE_REACTIONS | 1 << 10 | MESSAGE_REACTION_ADD / REMOVE |
| DIRECT_MESSAGE | 1 << 12 | DIRECT_MESSAGE_CREATE / DELETE |
| **GROUP_AND_C2C_EVENT** | **1 << 25** | **C2C_MESSAGE_CREATE、FRIEND_ADD / DEL、C2C_MSG_REJECT / RECEIVE、GROUP_AT_MESSAGE_CREATE、GROUP_ADD_ROBOT、GROUP_DEL_ROBOT、GROUP_MSG_REJECT / RECEIVE** |
| INTERACTION | 1 << 26 | INTERACTION_CREATE（按钮交互） |
| MESSAGE_AUDIT | 1 << 27 | MESSAGE_AUDIT_PASS / REJECT |
| FORUMS_EVENT | 1 << 28 | 论坛事件（仅**私域**） |
| AUDIO_ACTION | 1 << 29 | AUDIO_START / FINISH / ON_MIC / OFF_MIC |

> **群聊 + 单聊只需 `GROUP_AND_C2C_EVENT = 1 << 25`**；需要按钮再加 `INTERACTION = 1 << 26`。

---

## 6. 事件

### 6.1 收发场景对照

| 场景 | 发送消息接口 | 接收事件 |
|---|---|---|
| QQ 单聊 | 发送单聊消息 / 流式消息 | `C2C_MESSAGE_CREATE` |
| QQ 群聊 | 发送群消息 | `GROUP_AT_MESSAGE_CREATE` / `GROUP_MESSAGE_CREATE` |
| 频道 | 发送子频道消息 / 频道私信 | 频道消息事件 |

### 6.2 GROUP_AT_MESSAGE_CREATE 字段

| 字段 | 类型 | 描述 |
|---|---|---|
| `id` | string | 消息 ID，可用于被动回复和撤回 |
| `author` | User | 发送者（`member_openid` 有值） |
| `content` | string | 消息文本（**已去除 @机器人前缀**） |
| `group_openid` | string | 群 OpenID |
| `timestamp` | string | RFC3339 |
| `message_type` | integer | 消息内容类型 |
| `message_scene` | MessageScene | 场景上下文 |
| `attachments` | []MessageAttachment | 消息附件 |
| `mentions` | []User | @ 的用户（不含机器人自身） |
| `ark_data` | ARKData | 结构化卡片数据 |
| `msg_elements` | []MsgElement | 消息元素列表 |

**User**

| 字段 | 描述 |
|---|---|
| `id` | 用户唯一标识（OpenID） |
| `username` | 昵称 |
| `bot` | 是否机器人 |
| `union_openid` | 跨应用统一 OpenID（可能为空） |
| `user_openid` | 单聊场景 |
| `member_openid` | 群聊场景 |
| `member_role` | `member` / `admin` / `owner` |

**MessageScene**：`source`（default=默认聊天窗口）、`ext`（key=value：`msg_idx`、`ref_msg_idx`、`auth_token`）

**MessageAttachment**：`url`、`filename`、`width`、`height`、`size`、`content_type`（`voice` / `image/jpeg` / `image/png` / `image/gif` / `video/mp4` / `file`）、`voice_wav_url`、`asr_refer_text`

**ARKData**：`prompt`、`ark_type`（tuwen / feed / miniapp / map / contact_card / video_share / music_together / picture）、`ark_name`、`fields`（tag/title/desc/jump_url/preview/source/nickname/avatar/address…）

**MsgElement**：`msg_idx`、`author`、`message_type`（0 文本 / 3 结构化卡片 / 101 并行消息 / 102 聊天记录 / 103 引用消息）、`content`、`attachments`、`ark_data`、`msg_elements`（递归）

---

## 7. 消息类型

通过 `msg_type` 指定：

| msg_type | 类型 | 内容字段 | 发送 | 接收 |
|---|---|---|---|---|
| 0 | 文本 | `content` | ✅ | ✅ |
| 2 | Markdown | `markdown` | ✅ | - |
| 7 | 富媒体 | `media`（需先上传得 `file_info`） | ✅ | ✅ |

---

## 8. 频率与时效规则（★ 核心约束）

### 8.1 主动 vs 被动

| 类型 | 特征 | 说明 |
|---|---|---|
| 主动消息 | 无任何条件 | 用户可在客户端关闭「允许主动发送」，关闭后一律失败 |
| 互动召回消息 | `is_wakeup=true` | 用户主动对话后每个周期可下发 1 条 |
| 被动消息（回复用户） | 携带 `msg_id` | |
| 被动消息（响应事件） | 携带 `event_id` | |

### 8.2 被动消息窗口

| 场景 | 有效期 | 每条消息可回复次数 |
|---|---|---|
| **单聊** | **60 分钟** | **4 次** |
| **群聊** | **5 分钟** | **5 次** |
| 频道 | 5 分钟 | - |

### 8.3 主动消息频控

| 场景 | 认证 | Bot 维度 | 单关系维度 | 每日上限 |
|---|---|---|---|---|
| 单聊 | 企业/个人 | 10 qps / 20 qpm | 20 qpm | 1000 条/用户 |
| 单聊 | 未认证 | 5 qps / 30 qpm | 20 qpm | 1000 条/用户 |
| 群聊 | 企业/个人 | 60 qpm | 20 qpm | 1000 条/群 |
| 群聊 | 未认证 | 30 qpm | 20 qpm | 1000 条/群 |

**互动召回**：用户主动对话后 30 天内可下发，周期为 当天 / 1-3 天 / 3-7 天 / 7-30 天，共 **4 个周期**，每周期 1 条。

### 8.4 频道 / 私信额外限制

- 文字子频道：默认每子频道每天 20 条主动消息；每频道每天最多往 2 个子频道推送；每秒最多 5 条（主被动合计）
- 频道私信：每机器人每天对单用户 2 条主动消息；每天累计 200 条

### 8.5 消息去重

> 相同 `msg_id` 可能多次推送，请结合 `msg_seq` 去重。被动回复时，相同的 `msg_id + msg_seq` 重复发送会失败，可**递增 `msg_seq`** 实现对同一消息的多次回复。

### 8.6 撤回

机器人可撤回自己发送的消息，**超过 2 分钟不可撤回**。

---

## 9. 富媒体

### 9.1 类型与限制

| file_type | 类型 | 格式 | 软限制 | 硬限制 |
|---|---|---|---|---|
| 1 | 图片 | png / jpg | 20 MB | 200 MB |
| 2 | 视频 | mp4 | 30 MB | 200 MB |
| 3 | 语音 | silk | 20 MB | 200 MB |
| 4 | 文件 | 任意 | 200 MB | 200 MB |

> 超过软限制降级为文件类型上传；超过硬限制报错。
> 图片实际支持 jpg/png/gif/webp/bmp。

### 9.2 分片上传请求体（实测字段，容易记错）

**第一步 `POST /v2/users/{user_id}/upload_prepare`**（频率限制 10 QPS）

```json
{
  "file_type": 1,
  "file_size": "31457280",
  "file_name": "image.png",
  "md5":  "整个文件的 MD5",
  "sha1": "整个文件的 SHA1",
  "md5_10m": "文件前 10002432 字节（约 9.54MB）的 MD5"
}
```

返回：

```json
{
  "upload_id": "upload_xxx",
  "block_size": "10485760",
  "parts": [{ "index": 0, "presigned_url": "https://cos...", "block_size": "10485760" }],
  "upload_config": { "concurrency": 1, "retry_timeout": 300, "retry_delay": 1 }
}
```

⚠️ `file_size` / `block_size` 都是**字符串**；字段名是 `presigned_url`（不是 `url`）。

**第二步** 对每个分片 `HTTP PUT <presigned_url>`，body 为分片字节。
**预签名 URL 由对象存储校验签名，不能带 `Authorization` 头。**

**第三步 `POST /v2/users/{user_id}/upload_part_finish`**

```json
{ "upload_id": "upload_xxx", "part_index": 0, "block_size": "10485760", "md5": "分片的 MD5" }
```

**第四步 `POST /v2/users/{user_openid}/files`**

```json
{ "upload_id": "upload_xxx", "file_type": 1 }
```

→ 返回 `file_info`，即可用于 `msg_type=7` 发送。

### 9.2.1 分片上传流程（推荐）

```
1. 预上传  upload_prepare → upload_id + block_size + 各分片预签名 URL
2. 分片 PUT 按 block_size 分片，逐片 HTTP PUT 到预签名 URL
3. 确认分片 每片 PUT 成功后调 upload_part_finish
4. 完成合并 携带 upload_id 调用上传接口 → 返回 file_info
```

```
┌──────────────┐   ┌─────────────────────────┐   ┌──────────────────┐
│ 获取          │   │ for each chunk:         │   │ POST .../files   │
│ upload_id     │──▶│ PUT → presigned_url     │──▶│ { upload_id }    │
│ block_size    │   │ POST → part_finish      │   │ → file_info      │
│ presigned URLs│   └─────────────────────────┘   └──────────────────┘
└──────────────┘
```

### 9.3 URL 上传（文件已在公网）

```bash
POST /v2/users/{user_openid}/files
{ "file_type": 1, "url": "https://example.com/image.png" }
```

### 9.4 使用 file_info 发送

```json
{ "msg_type": 7, "media": { "file_info": "{上一步返回的 file_info}" } }
```

`srv_send_msg=true` 可在上传同时直接发送，但**会占用主动消息频次**。

### 9.5 注意事项

| 项 | 说明 |
|---|---|
| 场景隔离 | 单聊 `/v2/users/{user_openid}/files` 与群聊 `/v2/groups/{group_openid}/files` **不互通** |
| TTL | `file_info` 有有效期，过期需重新上传 |
| 秒传 | `md5_10m`（文件前 10002432 字节 ≈ 9.54MB 的 MD5）用于判断，避免重复上传 |
| 分片 | 默认 5MB，并发数与重试策略由 `upload_config` 下发 |
| 超时 | 上传接口建议 ≥ 5 秒 |
| `ttl` 语义 | `ttl` 为 `file_info` 有效期（秒），**`0` 表示可长期使用**（不是立即过期）。示例值 300 |
| `srv_send_msg` | 可选字段，文档未说明默认值；**必须显式传 `false`**，否则可能被当成「上传即发送」而占用主动消息频次 |
| 频率限制 | `/files` 50 QPS；`upload_prepare` 与 `upload_part_finish` 各 10 QPS |
| 合并请求体 | `POST /v2/{groups|users}/{openid}/files` 带 `{upload_id, file_type, srv_send_msg}` 即走合并路径，`url` 可为空 |

---

## 10. Webhook 模式（备选）

| 项 | 值 |
|---|---|
| 允许端口 | 80、443、8080、8443 |
| 签名 | Ed25519，seed = `botSecret`（不足 32 字节则重复拼接后截断） |
| 地址验证 | 平台推送 `plain_token` + `event_ts`，需返回 `signature` |
| ACK | 收到事件后回 `{ "op": 12 }` |

**WebSocket vs Webhook**：

| | WebSocket | Webhook |
|---|---|---|
| 公网要求 | 无需 | 需 HTTPS + 证书 |
| 事件补发 | 支持 Resume | 不支持 |
| 水平扩容 | 需按 shard 分配 | 无状态，易扩容 |
| 推荐场景 | **机器人首选** | 已有网关 / 大规模多实例 |

---

## 11. 端点全清单（频道相关除外）

> 本项目的 `qqbot-api` crate 覆盖下面**全部**端点。
> 频道（Guild / Channel / 身份组 / 论坛 / 音频 / 小程序）相关的接口**有意不实现** ——
> 那是另一套产品面，与群机器人无关。

### 11.1 基础与网关

| 用途 | 方法 + 路径 | 频率限制 |
|---|---|---|
| 取 token | `POST /app/getAppAccessToken` | — |
| 网关地址 | `GET /gateway` / `GET /gateway/bot` | — |
| 机器人详情 | `GET /users/@me` | 50 QPS |
| 生成分享链接 | `POST /v2/generate_url_link` | 50 QPS |

### 11.2 消息收发

| 用途 | 方法 + 路径 | 频率限制 |
|---|---|---|
| 发送单聊消息 | `POST /v2/users/{user_openid}/messages` | 100 QPS |
| **流式**发送单聊消息 | `POST /v2/users/{user_openid}/stream_messages` | 50 QPS |
| 撤回单聊消息 | `DELETE /v2/users/{user_openid}/messages/{message_id}` | 10 QPS |
| 发送群聊消息 | `POST /v2/groups/{group_openid}/messages` | 100 QPS |
| 撤回群聊消息 | `DELETE /v2/groups/{group_openid}/messages/{message_id}` | 10 QPS |
| 互动事件回调 | `PUT /interactions/{interaction_id}` | 50 QPS |

### 11.3 富媒体

| 用途 | 方法 + 路径 | 频率限制 |
|---|---|---|
| 单聊上传（URL 或合并） | `POST /v2/users/{user_openid}/files` | 50 QPS |
| 单聊预上传 | `POST /v2/users/{user_id}/upload_prepare` | 10 QPS |
| 单聊分片完成 | `POST /v2/users/{user_id}/upload_part_finish` | 10 QPS |
| 群聊上传（URL 或合并） | `POST /v2/groups/{group_openid}/files` | 50 QPS |
| 群聊预上传 | `POST /v2/groups/{group_id}/upload_prepare` | 10 QPS |
| 群聊分片完成 | `POST /v2/groups/{group_id}/upload_part_finish` | 10 QPS |

### 11.4 机器人：菜单与指令面板

| 用途 | 方法 + 路径 | 频率限制 |
|---|---|---|
| 查询全局自定义菜单 | `GET /v2/menu` | 30 QPM |
| 修改全局自定义菜单 | `PUT /v2/menu` | 5 QPM |
| 查询指令面板列表 | `GET /v2/panels` | 30 QPM |
| 创建指令面板 | `POST /v2/panels` | 10 QPM |
| 查询指令面板详情 | `GET /v2/panels/{panel_id}` | 30 QPM |
| 修改指令面板 | `PUT /v2/panels/{panel_id}` | 10 QPM |
| 删除指令面板 | `DELETE /v2/panels/{panel_id}` | 10 QPM |
| 增删面板关联对象 | `PUT /v2/panels/{panel_id}/target` | 60 QPM |

### 11.5 群聊管理

| 用途 | 方法 + 路径 | 频率限制 |
|---|---|---|
| 群信息 | `GET /v2/groups/{group_openid}/info` | 30 QPM |
| 机器人状态 | `GET /v2/groups/{group_openid}/bot_state` | 30 QPM |
| 入群申请列表 | `GET /v2/groups/{group_openid}/join_request_list` | 30 QPM |
| 审批入群 | `POST /v2/groups/{group_openid}/approval_join_request/{member_openid}` | 60 QPM |
| 查询群禁言状态 | `GET /v2/groups/{group_openid}/restrict_chat_setting` | 30 QPM |
| 设置群成员禁言 | `POST /v2/groups/{group_openid}/restrict_chat_setting` | 60 QPM |
| 群成员列表 | `GET /v2/groups/{group_openid}/members` | 60 QPM |
| 群成员详情 | `GET /v2/groups/{group_openid}/members/{member_openid}` | 30 QPM |
| 批量移出成员 | `POST /v2/groups/{group_openid}/batch_remove_members` | 30 QPM |
| 黑名单查询 | `GET /v2/groups/{group_openid}/member_blacklist` | 30 QPM |
| 黑名单操作 | `POST /v2/groups/{group_openid}/member_blacklist` | 60 QPM |
| 审批策略列表 | `GET /v2/groups/join_approval_strategy` | 60 QPM |
| 创建审批策略 | `POST /v2/groups/join_approval_strategy` | 60 QPM |
| 修改审批策略 | `PATCH /v2/groups/join_approval_strategy/{strategy_id}` | 60 QPM |
| 删除审批策略 | `DELETE /v2/groups/join_approval_strategy/{strategy_id}` | 60 QPM |
| 执行审批策略 | `POST /v2/groups/join_approval_strategy/{strategy_id}/execute` | 60 QPM |
| 改策略白名单 | `POST /v2/groups/join_approval_strategy/{strategy_id}/whitelist_users` | 60 QPM |

> 完整清单见 <https://bot.q.qq.com/wiki/sitemap.xml>（`api-v2/autogen/api/` 下为自动生成的全量接口页）。

---

## 12. 补齐接口的约束要点

这一节只记「看漏了就会踩坑」的部分，字段级细节见 `crates/qqbot-api/src/{bot,group,message}.rs` 的文档注释。

### 12.1 互动事件回调（`PUT /interactions/{id}`）

- 收到 `INTERACTION_CREATE` 后**必须**回应，否则用户端一直 loading；
  **指令回调类场景超时只有 3 秒**。
- 同一个 `interaction_id` **只能回应一次**。
- `interaction_id` 取自事件 `d.id`，**不带** `INTERACTION_CREATE:` 前缀。
- `code`：`0` 成功 / `1` 操作失败 / `2` 操作频繁 / `3` 重复操作 / `4` 没有权限 / `5` 仅管理员操作。
- `code=0` 且 `type=14`（清空会话）时，后台会下发「会话已清空」小灰条。

### 12.2 流式消息（`POST /v2/users/{user_openid}/stream_messages`）

- **仅单聊**。官方明确「群消息不支持流式参数」。
- 每个分片带**同一个** `stream_msg_id`，`index` 从 0 递增。
- **首片不传 `stream_msg_id`**，用响应里的 `id` 作为后续分片的 `stream_msg_id`。
- `input_state`：`1` = 生成中，`10` = 生成结束。
- `input_mode`：`append`（默认，拼接）/ `replace`（全量正文，须以上游已下发前缀开头）。
- 错误码 `40007`「已下发内容前缀不可修改」—— `replace` 模式下改了已经发出去的正文就会撞上它。

### 12.3 自定义菜单（`GET/PUT /v2/menu`）

- **仅 C2C（单聊）场景有效**，且是全局配置，不能按用户区分。
- `PUT` 是**全量覆盖**：没带上的旧菜单项等同于删除。
- 一级菜单项**最多 10 个**；子菜单**最多 5 个且不能再嵌套**。
- 名称长度：一级 **10 字符**（一个汉字算 2 个字符），二级 **14 字符**。
- `link` 必须 `https://` 开头。

### 12.4 指令面板（`/v2/panels`）

- 一个机器人**最多 20 个面板**；每个面板**最多 20 个元素**。
- 元素 `name` **≤14 字符**，`desc` **≤30 字符**，`remark` **≤255 字符**。
- `PUT /v2/panels/{id}` 覆盖元素与备注，但**不影响已关联的用户 / 群**（那些走 `/target`）。
- `/target` 单次最多 20 个关联对象；详情接口最多返回 **1000 条**关联 openid。
- `channel` / `dm` 场景**只能是全局面板**（`target_type=all`），传 `specific` 报 `40030012`。
- **全局面板（`target_type=all`）不支持 `/target` 接口**，报 `40030021`。
- 分页：`limit` 默认 20、最大 50；`next_cursor` 空串 **或** `is_end=true` 都表示到底。

### 12.5 群聊管理

| 接口 | 必须记住的约束 |
|---|---|
| `GET .../info` | **仅白名单机器人可用**，未申请权限返回 `11253` |
| `GET .../join_request_list` | `limit` 默认 20、最大 50；`join_request_id` 审批时要原样回传 |
| `POST .../approval_join_request/{member_openid}` | 字段名是 `op`（`approve` / `decline`），**不是** `action` |
| `POST .../restrict_chat_setting` | 单次 ≤20 人；**最长 30 天**；只能操作**普通成员**（群主/管理员/机器人不行） |
| `GET .../members` | 每页**最多返回 30 条** |
| `POST .../batch_remove_members` | 单次 ≤20 人；可顺带拉黑，失败名单在响应里 |
| `GET .../member_blacklist` | `limit` 默认 20、**最大 100** |
| `POST .../member_blacklist` | `op` = `add` / `del`；单次 ≤20 人；**目标成员还在群里时无法拉黑** |
| `POST /v2/groups/join_approval_strategy` | `group_openids` 与 `group_ids` **二选一必填**，各 ≤100；一个机器人**最多 20 个策略** |
| 同上 | `is_enable` 不传默认 `on`；`expire_at` 不传默认**一年后** |
| `PATCH .../{strategy_id}` | 群标识形式必须与创建时一致（当初用 openid 就一直用 openid） |
| `POST .../{strategy_id}/execute` | **异步**执行，官方说约 **10 分钟**完成 |
| `POST .../whitelist_users` | 单次最多 **10000** 个号码；号码用**字符串**传（避免 JS 精度问题） |

> 群号（`group_ids`）在请求体里官方标 uint64，在响应示例里却是字符串、还可能被脱敏
> （`"10****499"`）。`group::GroupId` 用 untagged 联合两种都收。
