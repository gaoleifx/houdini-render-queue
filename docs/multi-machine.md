# 多机渲染（局域网 Worker）设计

> 面向接手的 Agent：本文是**设计文档**，不是已完成的功能。要实现时按第 6 节的改动清单动手，
> 按第 10 节的阶段推进。所有字段名与现有代码一致，可直接对照 `src/model.rs` / `src/queue.rs`。

---

## 0. 目标与非目标

### 目标

让局域网内 2-3 台已装 Houdini 的机器共同消化一条队列，用户操作零变化：
还是把 `.hip` 拖进窗口、勾节点、按开始。多出来的只是队列里的任务可能跑在别的机器上。

### 非目标（明确不做，避免滑向渲染农场）

| 不做 | 原因 |
|---|---|
| 独立服务端进程 | master 就是现在这个 UI，关掉就全停，符合工具定位 |
| 多用户 / 权限 | 一台工作站一个用户 |
| 优先级队列 / 资源池配额 | 队列本来就是手排的，顺序即优先级 |
| 跨项目排队 / 跨网段 | 单机房、单子网 |
| 任务预占（reservation） | 2-3 台机器，先到先得足够 |
| 引入 async runtime | `std::net` + 每连接一线程，2-3 台完全够；项目现在是 thread + mpsc，别引入 tokio |

**什么时候该放弃自研**：机器涨到 10 台以上、要跨项目排队、要按集群配额调度 —— 直接用 Houdini 自带的
HQueue 或 Deadline。HRQ 的价值是"小、本地、够用"，超过这个规模就该换工具而不是继续加功能。

---

## 1. 环境前置条件（不满足就别做，做了也是白跑）

这三条是硬门槛，必须逐条确认：

1. **共享存储已就绪**，且所有机器用**同一套路径表达**访问它。
   - 推荐统一 UNC：`\\NAS\proj\shots\sh010\sh010.hip`
   - **不要**在 A 机用 `D:\proj`、B 机用 `\\NAS\proj` —— 那样的路径映射表会污染整个派发逻辑
   - 若存量工程用了盘符，先在 NAS 上给所有机器映射同一个盘符（如都映射 `Z:`），也比转换表简单
2. **hip 内部引用走相对路径**（`$HIP` / `$JOB`）或同一份 UNC。贴图、abc、bgeo 源序列同理。
   hip 里写死 `D:\assets\...` 的，只有那台机器能跑。
3. **输出路径落在共享盘**且可写。输出写在本地磁盘的话，渲完的文件散在各自机器上，等于白跑。
4. 每台机器装了**同版本 Houdini**（含 Render 权限的 commercial/indie 许可）。当前是独立节点锁定许可，
   并发不受 seat 限制，但仍受单机内存限制（见 5.2）。

---

## 2. 拓扑与角色

```
                    ┌────────────────────────────────────┐
                    │  Master = 现有 UI 进程              │
                    │  Rt 队列 · TcpListener :7878        │
                    │  唯一写数据库的进程                  │
                    └──────────────┬─────────────────────┘
                                   │  worker 主动出站拨号
                                   │  （只需 master 放行入站，worker 不开端口）
              ┌────────────────────┼────────────────────┐
        ┌─────▼─────┐        ┌─────▼─────┐        ┌─────▼─────┐
        │ Worker A  │        │ Worker B  │        │ 本机槽位   │
        │ --worker  │        │ --worker  │        │ （Local）  │
        └─────┬─────┘        └─────┬─────┘        └─────┬─────┘
              └──────────── 共享盘：hip / 资源 / 输出 ───┘
```

**角色差异只有一个**：谁起 `hython` 子进程。所以代码上把"执行"抽成两条实现，共用标记行解析。

```rust
// src/remote.rs 或直接放 queue.rs
enum Slot {
    Local,                    // 现有 run_worker（起本地 hython）
    Remote(WorkerId),         // 发 assign，收 line
}
```

**worker 不需要数据库、不需要分析 hip、不需要 UI 列表**。它只做四件事：
收 assign → 校验路径和版本 → 起 hython 跑 `render.py` → 把标记行回传。

---

## 3. 核心设计决策

### 3.1 分发粒度 = 帧块（不是帧，也不是整个 job）

| 粒度 | 问题 |
|---|---|
| 逐帧 | 每帧开一次 hython，加载 hip 30s~3min，开销完全盖过计算 |
| 整个 job 派一台 | 4 个任务 3 台机器，一台闲着 |
| **帧块** | 一台 worker 一次加载、连跑多帧；3 台切 `[1-20] [21-40] [41-60]` |

**关键收益：`python/render.py` 一行都不用改。**
它已经接受 `plan.json` 的任意帧列表，master 只需把 `frames` 切片，每片一个 hython 进程。
`##READY` / `##DONE` / `##FAIL` 语义完全不变，断点续渲的 `frames` 表逻辑天然复用。

### 3.2 只有 master 碰数据库

worker 回报的帧通过现有的 `WMsg::Frame` 通道回到引擎线程，由引擎统一 `db.mark_frame()`。
现有引擎是单线程串行消费 `wrx`，天然避免并发写库。**不要**让 worker 直连 SQLite（共享盘上的
SQLite + WAL 跨 SMB 是已知的灾难）。

### 3.3 master 发"逻辑任务"，worker 自己解析"物理路径"

`JobSpec.hython` 是 master 本机路径（`C:\Program Files\Side Effects Software\Houdini 21.0.596\bin\hython.exe`），
不能直接发给 worker。分配消息只带**逻辑信息**：

- `houdini`: 版本号字符串（`21.0.596`）
- `hip` / 输出路径：UNC 字符串（前置条件保证两边一致）

worker 收到后本地用 `houdini.rs` 现有的安装目录扫描逻辑，按版本号找到自己的 `hython.exe`。
找不到该版本 → `reject`，master 换一台。

这样**不需要任何路径/版本映射配置表**，worker 自己知道本机有什么。

---

## 4. 协议

### 4.1 传输与编码

- TCP，master 监听 `0.0.0.0:7878`（端口可在设置里改，存 `meta` 表 `remote_port`）
- **NDJSON**：一行一个 JSON 对象，`\n` 分隔，UTF-8
- 每个连接一个读线程 + 一个写线程；写走 `Mutex<TcpStream>` 或 `mpsc` + 单写线程（推荐后者，避免半行交错）
- 所有消息带 `t`（类型）字段；请求-应答类带 `seq`（u64，worker 自增）

选用 NDJSON 而不是裸 `##` 文本行的原因：标记行本身是"内容"，需要外层包一层才能带 job/块标识；
且后续加字段不用改解析器。

### 4.2 握手

worker → master：
```json
{"t":"hello","seq":1,"ver":1,"host":"WS-02","slots":1,
 "houdini":["21.0.596","20.5.445"],
 "paths":["\\\\NAS\\proj","\\\\NAS\\cache"]}
```

- `slots`：本机并发槽（见 5.2，默认 1）
- `houdini`：本机装了的版本列表
- `paths`：worker 声明可见的根前缀，仅用于 master 侧预筛，**真正校验在 accept 阶段**

master → worker：
```json
{"t":"welcome","seq":1,"master":"WS-01","proto":1,"poll_ms":1500}
```

`proto` 不匹配直接断开并在日志里说清"主控端与执行端版本不一致"。

### 4.3 派发：assign → accept / reject

worker 空闲时发 `claim` 带上自己的槽位状态：
```json
{"t":"claim","seq":12,"busy":0,"free":1}
```

master 挑一个候选块，回 `assign`：
```json
{"t":"assign","seq":12,"job":"sh010:/out/render_fx","chunk":"1-20",
 "attempt":0,
 "hip":"\\\\NAS\\proj\\shots\\sh010\\sh010.hip",
 "node":"/out/render_fx","node_type":"ifd","category":"render",
 "houdini":"21.0.596","output":"\\\\NAS\\proj\\shots\\sh010\\render\\fx_$F4.exr",
 "frames":[1,2,3],"splittable":true}
```

`frames` 是**展开后的逐帧列表**（块内），worker 直接写成本地 `plan.json`。

worker 校验三件事，任一失败即 `reject`：

```json
{"t":"reject","seq":12,"job":"...","chunk":"1-20","why":"hip 不存在：\\\\NAS\\proj\\...（paths 未挂载？）"}
```

1. `hip` 文件存在
2. 本机有 `houdini` 版本的 `hython.exe`
3. 输出目录可写（复用 `render.py` 的 `preflight` 逻辑，或者干脆让 hython 自己报 `##PREFLIGHT`）

通过则 `accept`，然后起进程：
```json
{"t":"accept","seq":12,"job":"...","chunk":"1-20"}
```

**reject 的语义是"这台机器干不了"，不是"任务失败"** —— master 记下 `(worker, job, chunk)` 三元组的
失败原因，换一台重派；连续 N 台都 reject 才把任务判 `Failed`，并把 reject 原因汇总进任务卡片。

这是整个设计里最省事的一处：**路径和版本问题在派发阶段就暴露，不用等渲染跑一半才炸**。

### 4.4 执行期

worker 每读到一行 hython 输出，**只回传标记行**：

```json
{"t":"line","job":"...","chunk":"1-20","data":"##DONE 7"}
```

非标记行（Houdini 的启动日志、warnings）默认**只在本地落盘**，不回传：
`%TEMP%\HoudiniRenderQueue\worker-logs\<job>#<chunk>.log`。
一个 IFD 渲染能吐几千行，全塞回网络会把进度挤掉。加 `--send-all-lines` 调试开关。

master 侧收到 `line` 后，**复用现有 `run_worker` 里那段 `match tag` 解析**（抽成
`fn feed_line(ctx: &mut LineCtx, line: &str)`，本地和远端共用）。这样 `##DONE` 记帧、
`##NOTE` 转日志、`##FAIL` 判失败的行为两边完全一致，不会出现"本机能跑远端不能"。

取消：master → worker
```json
{"t":"cancel","job":"...","chunk":"1-20"}
```
worker 写本地 `*.cancel` 文件（现有协作式取消），Python 在每帧开头检查，行为与本地一致。

### 4.5 结束与心跳

worker 进程退出后：
```json
{"t":"result","job":"...","chunk":"1-20","kind":"ok|preflight|fail|canceled","detail":"...","code":0}
```
映射到现有的 `WResult`，`WMsg::Done` 原样复用。

心跳：worker 每 5s 发
```json
{"t":"beat","busy":1,"job":"...","chunk":"1-20","frame":7,"mem_mb":14200}
```
master 15s 收不到 beat 判定 `offline`。`mem_mb` 只用于设备面板展示（看看是不是内存炸了）。

### 4.6 断线语义（明确写死，别让实现自由发挥）

| 场景 | 行为 |
|---|---|
| worker 断线，正在跑 | master 把该块标记 `lost`，其中**未收到 `##DONE` 的帧**归还待分配池；已 DONE 的帧已经落库，不重跑 |
| worker 断线，没任务 | 直接从设备面板移除 |
| master 关闭 / 崩溃 | worker 收到 EOF → **跑完当前帧后停**，不再 claim。master 重启后重新派发剩余帧 |
| worker 重连 | 新连接、新 `WorkerId`（用 `host + 启动时刻` 区分），旧记录作废 |

帧级幂等是这套语义成立的基础：任何一帧重跑都只是覆盖写同一路径，代价是浪费一次计算，
不会产生脏数据。所以**不做 `pending-report` 补报机制**（过度设计）。

---

## 5. 调度

### 5.1 可分片性判定（必须在派发前判，否则会写坏文件）

`JobSpec` 满足以下任一条件时，`splittable = false`，整包派给一台机器：

1. `spec.frames.len() <= 1`
2. **输出路径不含帧变量**（`$F` / `$F4` / `$F3` / `$FF`）。
   现有 `NodeInfo.output` 里已经有展开后的路径样例，检查是否存在 4 位数字段即可，
   或直接用 `render.py` 里那段 `template` 正则反向判断。
   典型反例：输出 `shot.mov`、单个 `shot.usd` —— 多台机器同时写同一个文件必然损坏。
3. 用户在节点上勾了**"顺序执行"**（新增列，默认不勾）。跨帧有依赖的解算序列
   （DOP/SOP solver 依赖上一帧结果）不能分片，这个只能靠人判断，程序猜不出来。
4. 任务是 `Category::Cache` 且节点类型属于已知的带状态解算（`*solver*` / `*sim*` / `*pyro*` / `*vellum*`）
   —— 默认也按顺序处理，但允许用户覆盖。

### 5.2 片大小

```
slots   = 在线 worker 槽位总数 + 本机槽位
remain  = 尚未完成的帧数
chunk   = max(ceil(remain / slots), min_chunk)
```

`min_chunk` 的取值要考虑加载成本，用 master 观测到的历史数据：

- master 记录每个 hip 的 `load_ms`（`run_worker` 里 `##BOOT` → `##READY` 的时间差）和
  `frame_ms`（相邻 `##DONE` 的间隔中位数）
- 若 `min_chunk × frame_ms < load_ms`，说明切了也是白切（加载比算还久）→ `min_chunk` 直接取 `remain`
- 经验值：`min_chunk = 2`，`n <= 6` 且只有 1-2 台空闲时不切

**槽位默认 1，不是 CPU 核数。** Houdini 单实例渲染常吃 8-32GB，2 个并发经常让机器进 swap，
总吞吐反而下降。想让机器吃满，用户手动调到 2-4，并在设备面板显示内存水位。

### 5.3 选机

优先级（简单、可解释，不做加权评分）：

1. 排除 `offline`、`slots` 全占、`houdini` 版本不含所需版本、已 reject 过该块的
2. 同样可用时选**当前跑的任务数 / 槽位数比值最小**的
3. 仍相同按连接先后

不做"缓存节点亲和到正在算同一个 hip 的机器"这类优化——收益只在同一 hip 多任务时出现，
且会让调度变得不可预测，2-3 台机器不值得。

---

## 6. 代码改动清单

### 新增 `src/remote.rs`（约 550-650 行）

```
pub struct WorkerInfo { id, host, slots, busy, houdini: Vec<String>, paths: Vec<String>, last_beat, online }
pub struct WorkerPool { workers: HashMap<WorkerId, ...>, listeners: TcpListener }
pub fn serve(pool, evt, wtx)          // master：accept 循环 + 每连接一线程
pub fn run_worker_role(connect, ...)   // worker：拨号 + 重连 + claim 循环
```

worker 模式下**不启动 UI、不打开数据库、不扫描 hip**，只跑 `run_worker_role` 然后退出。

### `src/queue.rs`（约 +150 / −40）

| 改动 | 说明 |
|---|---|
| `Rt` 加 `blocks: Vec<Block>` | `Block { range: (i32,i32), slot: Slot, state: BlockState }` |
| `run_worker` 拆 `run_local` / `run_remote` | 标记行解析抽 `feed_line`，两边共用 |
| 调度段 | `active >= concurrency` 换成"按块抢占空闲槽位" |
| `WMsg` 加 `FrameOf(chunk, f)` | 现有 `Frame(id, f)` 保留给本地，块级粒度用新变体 |
| `WMsg::Done` 加 `chunk` 或改用 `(job, block)` 键 | 否则一个 job 多块返回时无法区分是哪块结束 |
| 取消 | `Cmd::CancelOne` 同时向该 job 所有远端块发 `cancel` |
| `Cmd::SetSlots(worker_id, n)` | 设备面板调整槽位 |

### `src/db.rs`（+20 行）

只加一处：`jobs` 表加列 `host TEXT DEFAULT ''`，记录**最后一次跑它的机器名**，用于事后排查
（"这个任务上次是在 WS-02 上失败的"）。老库用 `ALTER TABLE ... ADD COLUMN` 幂等升级。
**不建 `workers` 表** —— worker 是运行时概念，重启后从心跳重建即可。

### `src/main.rs`（+80 行）

- 解析 `--worker --connect <host:port>`，命中就直接走 worker 分支并 `return`，不初始化 Slint
- `--listen <port>` 覆盖默认端口
- 把 `WorkerPool` 快照作为新的 `Event::Workers(Vec<WorkerView>)` 推给界面
- 设置项持久化 `remote_port` / `remote_enabled`

### `src/model.rs`（+30 行）

```rust
pub struct WorkerView { pub id: String, pub host: String, pub online: bool,
                        pub slots: u32, pub busy: u32, pub houdini: String,
                        pub job: String, pub frame: i32, pub mem_mb: u64 }
pub struct JobSpec { /* 现有字段 */ pub splittable: bool }
```

`JobSpec.splittable` 由 `make_spec` 算好并落库（`jobs` 表加一列），保证重启后判定一致。

### `ui/app.slint`（+150 行）

顶栏加"设备"按钮 → 弹出面板：

```
┌ 设备 ────────────────────────────────┐
│ ● WS-01（本机）  槽 2/2  ████░░ 内存 14G │
│ ● WS-02          槽 1/1  ██░░░░ 内存 9G  │
│ ○ WS-03  离线 12 分钟                     │
│ + 添加设备：在目标机运行                │
│   houdini-render-queue.exe --worker --connect 192.168.1.10:7878 │
└──────────────────────────────────────┘
```

队列卡片右下角加一个机器标签（`WS-02`），分片的任务显示成 `WS-02 · 1-20`。
**颜色走 `Theme` token，不硬编码**（见 AGENTS.md 第 6 节）。

### 不改的部分

- `python/render.py`、`analyze.py`：**零改动**
- `frames.rs`：帧集合切块用现有 `expand()` 后按索引切，不需要新 API
- `windrop.rs`、`fonts.rs`、`snap.rs`：无关

---

## 7. 并发写文件的风险（重点）

一个 job 被切成 3 块并行时，三台机器会**同时**：

1. 各自在本机临时目录写 `plan.json` —— 现有文件名是 `%TEMP%\HoudiniRenderQueue\plans\<safe-id>.plan.json`，
   同一台机器上跑同一 job 的两个块会**撞名**。必须改成 `<safe-id>#<from>-<to>.plan.json`
2. 各自在 `/out` 里创建 `hrq_cache_proxy` 节点（仅内存，不存 hip）—— 无冲突
3. 写各自帧的输出文件 —— 靠 `$F` 变量分流，**这是 5.1 第 2 条要校验的原因**

另外 `render.py` 里"全部帧输出都小于 1KB 就判失败"的检查是**按块**生效的。
一个块只有 2 帧时这条检查的置信度低于整段 40 帧时。可接受，但要在失败日志里写清"该块"而不是"该任务"。

---

## 8. 失败处理汇总

| 情况 | 处理 |
|---|---|
| `reject`（路径/版本不可用） | 换机器重派，N 台都拒才判失败 |
| worker 跑挂（`result: fail`） | 走现有 retries，重试时**优先换一台机器** |
| 心跳丢失 | 该块未完成帧归还池，任务回到 `Queued` 状态的部分块重新参与调度 |
| 整块失败但部分帧 DONE | 已 DONE 的帧保留，重派时只发缺失帧（现有 `frames` 表逻辑，无需新代码） |
| worker 全部离线 | 任务停在 `Queued`，日志提示"没有可用执行节点"；本机槽位仍在，退化为单机运行 |
| master 与 worker 协议版本不一致 | 断开 + 日志报明确原因，不静默失败 |

---

## 9. 实施阶段

### P0 — 零代码验证（必须先做，1 小时）

在第二台机器上**手工**做一遍，不做这步直接写代码等于赌博：

1. 把 hip 和资源放到 NAS，路径统一成 UNC
2. 第二台机器拷一份 `dist/`（exe + `python/`），直接以**普通模式**启动
3. 拖同一个 hip，勾同一个节点，手工跑同一段帧，输出到同一个共享目录
4. 检查：hip 能不能打开、贴图/缓存引用有没有断、输出是否落到共享盘、版本是否一致

**这一步会暴露路径、版本、许可三个真问题，而且全是配置问题，改代码解决不了。**
P0 通过后再动代码，否则会花两天写完发现工程根本没法在别的机器上打开。

### P1 — 最小可用（约 2-3 天）

- `--worker` 模式 + 握手 + assign/accept + 心跳
- 帧切块 + master 派发（固定 `min_chunk=2`，不做自适应）
- 设备面板（只读列表 + 在线状态）
- 取消、断线归还
- 单测：切块算法、可分片判定、NDJSON 编解码、心跳超时判定

验收：3 台机器跑一个 60 帧的渲染任务，总耗时接近单机的 1/2~1/3，中途拔掉一台网线不影响其余，
剩下两台的帧正常完成。

### P2 — 打磨

- 自适应片大小（`load_ms` / `frame_ms` 观测）
- 设备面板调槽位、手动摘除节点
- 失败换机重试
- 任务卡片显示机器标签

### P3 — 触发换工具的信号

出现以下任一条，转 HQueue / Deadline，不要再往 HRQ 里加：

- 机器 > 8 台
- 需要多项目共用一套算力
- 需要按截止时间 / 优先级抢占
- 需要渲染结果预览、审批流

---

## 10. 风险与未解问题

| 风险 | 说明 | 缓解 |
|---|---|---|
| **路径不一致** | 最可能的翻车点。有人用盘符、有人用 UNC，或者 NAS 只给部分机器挂了 | P0 强制验证；accept 阶段 `os.path.exists` 兜底 |
| **hip 内部绝对路径** | 工程里写死 `D:\assets`，只有原机器能跑 | P0 在目标机打开同一帧试渲；日志里明确报"引用的资源找不到" |
| **输出写本地** | 用户没改输出路径，渲完散在各自机器 | `preflight` 已检查可写，但**不检查是否共享**——加一条：输出路径不在 worker 的 `paths` 共享前缀里时警告 |
| **跨帧依赖被误分片** | 解算序列分片后结果错乱，且错得很隐蔽 | 5.1 的节点类型启发式 + "顺序执行"勾选；默认保守 |
| **网络带宽** | 输出走 SMB，EXR 序列可能打满千兆 | 大分辨率任务建议输出到本机 SSD 再拷回；或上 10GbE |
| **HP Z8 / 大内存机器不公平** | 简单按槽位算，快机器和慢机器拿一样多的帧 | P2 可加每机器权重（按历史帧耗时中位数），但先不做 |
| **master 单点** | master 关掉全部停 | 符合工具定位，接受。worker 会跑完当前帧再停，不丢已完成的帧 |

---

## 11. 一句话总结

> 不是造农场，是给现有执行槽加一个"远端"实现：**粒度用帧块**（省钱在 hython 加载），
> **校验放派发阶段**（路径和版本问题在起进程前就暴露），**只让 master 碰数据库**（避开 SMB + SQLite），
> **协议包一层 NDJSON**（复用现有标记行解析）。真正的成本在共享存储和工程规范，不在通讯。
