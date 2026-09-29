# 资源管理设计（群隔离 + 系统级）

> 映射与素材都放进 SQLite，运行时可增删。
> 资源分**群**与**系统**两种作用域：群资源只在本群可见，系统资源全局可见。

---

## 0. 三条核心规则

1. **群隔离** —— 在 A 群收录的图，只有 A 群能触发。
2. **同关键词，群优先** —— 本群有就用本群的，本群没有才落到系统资源。
3. **权限分层** —— 群资源归本群管理员；系统资源归**系统控制者**，走独立命令。

---

## 1. 数据模型

```sql
CREATE TABLE IF NOT EXISTS resources (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    -- group = 某群自建；system = 全局
    scope       TEXT    NOT NULL CHECK (scope IN ('group','system')),
    -- 群资源存 group_openid；系统资源存空串（不是 NULL，见下方说明）
    owner_id    TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    -- 素材文件路径。**收录时就解析成绝对路径**，之后不依赖进程工作目录。
    path        TEXT    NOT NULL,
    -- 发送时给平台判格式用（map.jpg / pack.zip）
    file_name   TEXT    NOT NULL,
    file_type   INTEGER NOT NULL,
    description TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (scope, owner_id, name)
);

-- 触发词。主名也在这张表里，保证「同一作用域内关键词唯一」。
CREATE TABLE IF NOT EXISTS resource_keywords (
    keyword     TEXT    NOT NULL,
    scope       TEXT    NOT NULL CHECK (scope IN ('group','system')),
    owner_id    TEXT    NOT NULL,
    resource_id INTEGER NOT NULL REFERENCES resources(id) ON DELETE CASCADE,
    PRIMARY KEY (keyword, scope, owner_id)
);

CREATE INDEX IF NOT EXISTS idx_resource_keywords_resource
    ON resource_keywords(resource_id);
```

### ⚠️ 为什么 owner_id 用空串而不是 NULL

**SQLite 的 UNIQUE 索引把 NULL 视为互不相等。**
如果系统资源把 owner_id 存成 NULL，上面那个三列主键就**拦不住重复**：
两条 (地图, system, NULL) 会同时存在，而「同关键词只能有一个系统资源」正是我们要的约束。
用空串做「无归属」的哨兵值，约束才真正生效。

### 为什么关键词要拆成独立表

如果把关键词存成 JSON 数组塞在 resources 里，
「同一群里两个资源抢同一个词」只能在应用层检测，而应用层总会漏。
交给主键，冲突在 INSERT 时直接抛出来。

三列主键的语义正好是我们要的：

- 同一个词可以在**不同的群**各有一份；
- 同一个词可以同时存在于某群和系统（触发时群优先）；
- 同一个词在**同一群内**只能指向一个资源。

---

## 2. 系统设置表

```sql
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
```

系统控制者列表存在 key = 'system_controllers'，值是 JSON 数组：

```json
["5CF47107AFE2275EE0173D298F6FF07E"]
```

**首次建表时播种这个默认值。**
播种只发生在 schema 升到 v2 的那一刻（migrate() 的版本分支里），
所以之后把列表清空**不会**把默认值复活 —— 否则「删掉最后一个控制者」会变成一个无法完成的动作。

### 优先级与锁定恢复

```text
QQBOT_SYSTEM_CONTROLLERS 环境变量  >  settings 表  >  内置默认值（仅用于首次播种）
```

环境变量 / 配置**显式设置时覆盖数据库**，这是唯一的**锁定恢复通道** ——
万一列表被改错导致所有人都不是控制者，还能靠它把自己加回去，
不必去手动改 SQLite 文件。

列表是个位数量级，JSON 足够，不值得再拆一张表；
增删是读-改-写，但**只有本进程写**，不存在并发覆盖。

---

## 3. 运行期查找

**关键冲突**：Router 在 register() 时静态建表，而资源是运行时可变的。
为每条关键词注册一条路由，意味着新增资源必须重启 —— 正好废掉「入数据库」的意义。

所以：启动时把整张表读进内存索引，只注册**一条**隐藏监听器。

```rust
struct Index {
    /// group_openid → (keyword → resource_id)
    groups: HashMap<String, HashMap<String, i64>>,
    /// keyword → resource_id（系统级）
    system: HashMap<String, i64>,
}

fn lookup(&self, group: Option<&str>, keyword: &str) -> Option<i64> {
    group
        .and_then(|g| self.groups.get(g))
        .and_then(|m| m.get(keyword))
        .or_else(|| self.system.get(keyword))
        .copied()
}
```

两处细节：

- **两级 HashMap 而不是 HashMap<(String, String), i64>** ——
  后者每次查找都要构造 owned key，而这是每条群消息都会走的路径。
  两级结构可以直接用 &str 借位查找，零分配。
- **群优先、系统兜底**就是上面 .or_else() 的位置；
  单聊没有群，group 为 None，自然只落到系统资源。

**一致性**：只有本进程写这张表，所以每次写成功后同步更新内存索引即可，
不需要 TTL 轮询，也不需要跨进程协议。

**优先级**：这条监听器必须压到最低（Rule.priority 取负数），
否则一条叫「日报」的资源会顶掉真正的日报命令。
收录时也要检查关键词是否与已有路由冲突，冲突直接拒绝。

**副作用**：资源不出现在帮助里（隐藏路由），所以配 资源列表 / 系统列表。

---

## 4. 命令

### 4.1 所有人

| 命令 | 说明 |
|---|---|
| 任意关键词 | 发送对应素材。**本群的优先，其次系统** |
| 资源列表 | 本群资源 + 系统资源，分别标注来源 |

### 4.2 本群管理员（member_role 为 admin / owner）

| 命令 | 说明 |
|---|---|
| 收录 关键词 [说明] | 回复一张图或文件时收录到**本群**；同词已存在则覆盖 |
| 别名 关键词 新词 | 给本群资源加触发词 |
| 删除资源 关键词 | 删除**本群**资源及其全部关键词 |

### 4.3 系统控制者（独立命令，不与管理员的混用）

| 命令 | 说明 |
|---|---|
| 系统收录 关键词 [说明] | 收录为**系统级**资源 |
| 系统别名 关键词 新词 | 给系统资源加触发词 |
| 系统删除 关键词 | 删除系统资源 |
| 系统列表 | 列出全部系统资源 |
| 系统控制者 | 查看当前控制者列表 |
| 系统控制者 添加 openid | 增加控制者 |
| 系统控制者 移除 openid | 移除控制者 |

系统命令在群聊和单聊都可用，只校验控制者身份 ——
这样控制者可以私聊管理，不必在群里刷屏。

---

## 5. 权限边界

| 操作 | 群管理员 | 系统控制者 |
|---|---|---|
| 收录 / 别名 / 删除 **本群**资源 | 可以 | 需同时是本群管理员 |
| 收录 / 别名 / 删除 **系统**资源 | 不行，必须走 系统* 命令 | 可以 |
| 删除别的群的资源 | 不行 | 不行 |

两条刻意的设计：

1. **删除资源删不到系统资源。** 想删系统资源只能用 系统删除。
   否则一个群管理员在群里随手一句「删除资源 地图」就可能把全局资源删掉。
2. **系统控制者不等于群管理员。** 控制者在 A 群管理资源，仍然需要 A 群的管理员身份；
   控制者身份只解锁 系统* 命令。两个维度互不派生。

---

## 6. 存储形态与 IO

**数据库只存映射与路径，素材留在磁盘上。**

这样做的直接收益是**换图不用碰数据库**：
直接替换磁盘上那个文件，下一次触发就是新图 —— 不重启、不写库、不需要失效逻辑。

发送时读一次文件。几 MB 的本地读是毫秒级，而**上传才是慢的那一步** ——
那一步由媒体的秒传缓存兜住，所以同一张图只在进程重启后第一次真正上传。

想再省一点可以按 (path, mtime) 加一层 moka 缓存；先不做，因为没有测量依据。

---

## 7. 与秒传缓存的关系

资源内容永不变化（除非主动覆盖），所以秒传缓存命中率是 100% ——
**除了进程重启后的第一次**。资源越大，重启后首次发送的代价越高。

两个可选优化，都不急：

1. **持久化 file_info 缓存**。前提是先补「file_info 失效 → 清缓存重传」的自愈，
   否则失效条目会被永久复用，表现为「图永远发不出去，重启也没用」。
2. **启动预热**。resources 表里有 size，可挑大条目在启动后异步上传一次。

---

## 8. 存储降级与迁移

db_path 置为空串会关掉持久化（store 为 None），此时资源功能**整体不可用**。
处理方式与日报一致：不注册这些命令，启动时打一条 warn。

SCHEMA_VERSION 从 1 提到 2，三张表加进 DDL。
**不需要写数据迁移** —— DDL 用的是 CREATE TABLE IF NOT EXISTS，
migrate() 里 v < SCHEMA_VERSION 的分支本来就会更新版本号，
新表与 messages 无关联，保留期清理与 incremental_vacuum 都不受影响。

⚠️ 唯一要留意的是播种：默认控制者只在「首次到达 v2」时写入，见第 2 节。

---

## 9. 收录方式（已定：两条都保留）

| 做法 | 文件落在哪 |
|---|---|
| 回复一张图 + `收录 关键词` | 下载后存进 `resources_dir`，再记录路径 |
| `收录 关键词 服务器本地路径` | **不复制**，直接记录该路径 |

本地导入是**保真通道**：塔科夫地图这类高分辨率图走 QQ 转发会被压糊到看不清小字。

两条路径都在收录时把相对路径解析成**绝对路径**并校验文件存在，
所以数据库里不会存进悬空路径，之后也不依赖进程的工作目录。
但文件仍可能在收录之后被删除 —— 发送时要处理「文件不见了」，
回复一条明确的提示而不是静默失败。
