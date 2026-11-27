# 项目介绍（给 AI Agent 的交接文档）

本文件面向接手这个仓库的 AI Agent 或新同事：读完应能判断「改哪里、别碰哪里、怎么验证」。
人看的快速上手在 `README.md`，这里只补它没写的架构、协议与踩坑。

---

## 1. 这是什么

**Houdini Render Queue（HRQ）**：Windows 桌面小工具，把 Houdini 的**后台缓存**和**渲染**排成一条队列跑。

用户操作只有三步：把 `.hip` 拖进窗口 → 程序用 hython 打开它、列出可输出的缓存/渲染节点 → 勾选后按顺序后台执行（默认串行，可调并发），失败重试、支持按帧断点续渲。

它**不是**渲染农场：没有服务端、没有多机分发、不碰 Deadline。就是一台工作站上的本地批量执行器，替代「手动一个个点 Render / 手动一个个点 Save to Disk」。

关键定位判断（改需求时先想清这三条）：

- **执行者是 hython 子进程**，Rust 只做调度、进度汇总和界面。所有 Houdini 相关知识都在 `python/` 里。
- **一次任务 = 一个节点 + 一个帧集合**（连续区间或不连续帧，如 `1052,1055,1065-1070`），任务 ID 由 `工程 key` + `节点路径` 拼成，天然幂等。
- **状态必须能跨重启恢复**：SQLite 记工程、任务、已完成帧，重开程序队列还在。

---

## 2. 技术栈与硬约束

| 层 | 选型 | 约束 |
|---|---|---|
| 主程序 | Rust 2024 edition，`slint` 1.x | release 走 `windows_subsystem = "windows"`，双击不弹控制台 |
| 界面 | Slint（`ui/app.slint`，单文件 1300+ 行） | 无 WebView、无前端框架，改样式就改这一份 |
| 存储 | `rusqlite`（bundled） | WAL 模式，库在 `%APPDATA%\HoudiniRenderQueue\state.db` |
| 执行 | `hython.exe` 子进程 + JSON/标记行通信 | 子进程一律 `CREATE_NO_WINDOW`，不能闪黑框 |
| 原生交互 | 自己写的窗口子类化（`src/windrop.rs`） | 拖放、双击识别都不走 Slint 默认实现 |

平台只有 Windows 10/11。别引入跨平台抽象层（不需要的复杂度）。

---

## 3. 目录地图

```
src/main.rs      2145 行，界面状态与所有回调（App 结构体、工程列表、节点表、队列同步）
src/queue.rs      667 行，调度引擎：线程 + mpsc 指令通道 + 并发/重试/断点/取消
src/db.rs         376 行，SQLite：projects / jobs / frames / meta 四张表
src/frames.rs     282 行，帧集合 FrameSet：解析 `1052,1055,1065-1070`、合并相邻段、展开成逐帧列表
src/model.rs      259 行，领域模型：Category、JobStatus、NodeInfo、Project、JobSpec、JobView、Event
src/analyze.rs    152 行，起 hython 跑 analyze.py，截取 JSON，带超时
src/houdini.rs    154 行，扫安装目录找 hython；读 hip 头部 `_HIP_SAVEVERSION` 得到制作版本
src/windrop.rs    724 行，OLE 拖放 + 窗口子类化（双击 / 大图标）
src/pyproc.rs      55 行，脚本路径定位 + 隐藏窗口的 Command
src/remote.rs    约 900 行，局域网多机：NDJSON 行协议 + master 侧 Hub + worker 角色
src/snap.rs        43 行，`--snapshot` 截图（调试用）
src/fonts.rs      约 150 行，枚举本机系统字体（fontdb），给字体设置下拉框用

ui/app.slint     1369 行，全部界面：顶栏 / 三栏（工程、节点、队列）/ 日志列 / 底部状态栏
python/analyze.py 255 行，在 hython 里枚举输出节点 → JSON
python/render.py  266 行，在 hython 里按帧渲染 → `##` 标记行

devtest/         不装 Houdini 也能跑的回归环境（假 hou 模块 + 假 hip + 探针脚本）
tools/           图标生成与校验脚本（PIL + ctypes）
build.rs         rc.exe 编图标进 exe，找不到只警告
assets/          app.ico / app.png / app.rc
dist/            可分发产物（exe + python/），改完要手动同步
```

---

## 4. 三条主链路

### 4.1 分析一个 hip

```
拖入 .hip → collect_hips() 扫出 hip 列表 → start_analyze()
  → 后台线程 analyze::run(hython, hip, timeout)
    → hython python/analyze.py <hip>
      → 输出 ##BOOT / ##READY / ##NODES n / <<<ANALYZE_JSON_START>>>…END>>>
  → Event::Analyzed { nodes, global, houdini }
→ main.rs 建 Project、落库、重建节点表
```

- 节点来源三处：`/out` 的 ROP、`/stage` 的 LOP 渲染节点、`/obj` 下的 **SOP 缓存节点**（只认类型白名单，不做参数启发式，否则 `file1`/`null` 会被误判成缓存）。
- 缓存**永远排在渲染之前**（分类排序 + 队列插入顺序）。
- 每个 hip 默认用**文件里记录的制作版本**匹配本机安装（`_HIP_SAVEVERSION`，精确版本优先，同主次版本兜底）；用户手动选过则 `pinned = true`，不再跟随。

### 4.2 执行队列

```
勾选节点 → make_spec() → insert_job() → sync_jobs() → Cmd::SetJobs
  → 引擎线程 run()：维护 Vec<Rt>，按 concurrency 起 run_worker
    → 写 plan.json（帧列表）、起 hython render.py，逐行读 stdout
    → ##DONE f  → WMsg::Frame → 记 frames 表、回推 Event::Queue
    → 结束 → WResult::{Ok, Preflight, Fail, Canceled} → 重试或定状态
  → main.rs 收 Event::Queue → rebuild_queue()（先比 signature，没变就不重建）
```

- 取消是**协作式**：写一个 `*.cancel` 文件，Python 在每帧开头检查；20 秒还没退出才 `kill`。
- 断点续渲：`frames` 表记已完成的帧，重跑时只发缺失帧。
- 启动超时 5 分钟（许可证被占 / 工程过大）。

### 4.3 持久化

- `projects`：key / hip / shot / houdini / hython / nodes_json / gstart / gend / pinned
- `jobs`：任务快照（含 ord 顺序、status、attempts）。`fstart`/`fend` 存帧集合的**上下界**，
  真正的帧集合在 `frames` 列（规范写法，如 `1052,1055-1056,1065-1070`）；老库该列为空串，
  读的时候退回 `fstart..fend` 当连续区间
- `frames`：已完成帧，`(job_id, frame)` 主键。不连续帧也只是行数不同，无需特殊处理
- `meta`：字体、主题、窗口尺寸（`win_w` / `win_h` 逻辑像素、`win_max` 是否最大化）、
  `ranges`（帧范围覆盖，`{job_id: "1052,1055-1056"}`；
  老版本是 `[起, 止]` 数组，`load_ranges()` 两种都认）

启动时读库重建队列；上次退出时处于 `running` 的任务会回落到 `queued`（进程已经没了）。

### 4.4 多机执行（局域网，2026.2 起）

设计文档在 `docs/multi-machine.md`，实现是 `src/remote.rs` + `queue.rs` 里几处钩子。

```
主控（就是主界面）--listen 7878  →  起 Hub，TcpListener + 在线 worker 池
worker（同一 exe --worker --connect host:port）→ 主动拨号，收活、起本机 hython 跑 render.py
```

要点：

- **粒度是帧块**，不是逐帧：一块一个 hython 进程，`render.py` 一行没改。
- **派发前校验**：worker 收到 `assign` 先查 `hip` 是否存在、本机有没有对应版本的 hython，
  不满足就 `reject`，主控换一台。路径/版本问题在起进程前暴露。
- **乐观占用**：`dispatch` 发出去就把槽算忙，收到 `reject` 再退回来（不维护"已发未回"状态机）。
- **只有主控写数据库**：worker 的 `##DONE` 通过 `HubEvent::Frame` 回到引擎线程统一落库。
  共享盘上的 SQLite + WAL 跨 SMB 会坏。
- **`Rt` 里的三个帧集合必须分清**（漏一个就会重复执行或漏跑）：
  - `done` 已完成
  - `remote` 已派给远端、还没回报
  - `local` 已交给本地线程、还没完成
  `free_frames()` = 全集 − 三者。
- **`paced` 标记**：任务第一次分片时「对半分」，之后谁拿都是整包剩余。不加这个标记，
  本地会 100→50→25→13→6 一路对半切，每段都重开一次 hython，加载开销把并行收益吃光。
- **`is_splittable`**：输出路径要含 `$F`/`${F`，且不能勾「顺序执行」。写单文件（mov、单个 usd）
  的任务必须整包派一台；解算类节点（依赖上一帧）靠人工勾顺序执行——程序判断不出来。
- **分片下限来自实测**：`Perf { boot_ms, frame_ms }` 按任务记在 meta 的 `perf` 键里，
  `min_frames()` = 1.5×加载/单帧。切出来的块如果小到「算 3 帧、加载 60 秒」，多机反而更慢。
  没测到过时退回经验值 8 帧。
- **失败要换机**：`Rt.avoid` 记下失败的 worker id，`AssignReq.exclude` 让 `dispatch` 跳过它。
  否则同一块会被派回同一台，一路重试一路失败，attempts 白白烧完。
- **掉线要退回**：Hub 给每台 worker 记 `inflight: Vec<(job, chunk)>`，连接断或心跳超时时把这份
  放进 `HubEvent::Offline.running`；引擎把对应帧退回 `free_frames()` 并把任务放回排队。
  不做这件事，机器一断网那些帧既不完成也不重派，任务永远停在「执行中」。
- **忙数由主控记账**：`dispatch` +1、`Result`/`Reject` −1，**不采信** worker 心跳/claim 里的数字。
  采信的话，过期的 claim 会把刚派出去的槽报成空闲，往 1 槽机器上压第二块。
- **没挂 Hub 时整条多机路径不执行**，单机行为与旧版完全一致（改动时守住这条）。

worker 侧两个坑：

- release 是 `windows_subsystem = "windows"`，双击起 worker 没有控制台 →
  `ensure_console()` 先 `AttachConsole(-1)` 再 `AllocConsole()`，日志另写 `%TEMP%\HoudiniRenderQueue\worker.log`。
- `HRQ_HYTHON` 指定的 hython 版本号可能是「自定义1」这类标签，`pick_install` 让它**优先于版本匹配**；
  同时 `same_minor` 只认纯数字版本串，否则 `20.5.684-fake` 会匹配到真实的 `20.5.684`。

---

## 5. 跨进程协议（改 Python 侧必看）

`analyze.py` 输出：

```
##BOOT
##READY
##NODES <count>
<<<ANALYZE_JSON_START>>>
{ "hip": …, "houdini": …, "error": null,
  "global": {"fstart","fend","rfstart","rfend"},
  "nodes": [{"path","type","category","output","output_parm","fstart","fend","frame_source"}] }
<<<ANALYZE_JSON_END>>>
```

帧计划 `plan.json` 是 `{"frames": [1052,1055,1065,1066]}` —— **逐帧列表**，
所以不连续帧在 Python 侧不需要任何改动（`frames[0]` 只用于预检探路）。

`render.py` 输出标记（Rust 在 `queue.rs` 的 `run_worker` 里解析）：

| 标记 | 含义 | Rust 行为 |
|---|---|---|
| `##BOOT` | 进程起来了 | 忽略 |
| `##READY` | 工程加载完、节点已定位 | 解除 300s 启动超时 |
| `##FRAME f` | 开始跑第 f 帧 | 忽略（进度靠 DONE 累加） |
| `##DONE f` | 第 f 帧完成 | 记库、回推进度、写日志 |
| `##NOTE …` | 提示（如自动关掉了 Load From Disk） | 写日志，不带 `##` 前缀 |
| `##PREFLIGHT …` | 预检失败（目录不可写 / 磁盘 <500MB） | 任务失败但不重试 |
| `##FAIL …` | 致命错误 | 失败，按 retries 重试 |
| `##CANCELED` | 收到取消 | 状态「取消」 |
| `##FINISH` | 全部完成 | 成功 |

退出码：`0` 成功 / `2` 预检失败 / `3` 渲染失败 / `4` 被取消。

**约定：加新标记必须两边同时改**（Python `emit` + Rust `match tag`），否则会掉进日志兜底分支。

---

## 6. 界面结构与 Slint 约定

- 布局：顶栏（状态灯 / 并发 / 重试 / 开始暂停取消 / 字体 / 主题）＋ 左栏工程卡片 ＋ 中栏节点表 ＋ 右栏队列 ＋ 最右日志列 ＋ 24px 底部状态栏。
- 主题走 `Theme` 全局（暗 / 亮两套 token），**不要在组件里硬编码颜色**。
- 节点表用 `ListView`（只实例化可见行），不用 `ScrollView + VerticalLayout`（2985 行会卡死事件泵）。
- Rust 与 Slint 共用同一份 `Rc<VecModel<OutNode>>`：改单个节点用 `set_row_data`（`refresh_row`），别整表重建。

Slint 1.19 的坑（都在这里踩过，改 UI 前必读）：

1. 定高控件（徽章、圆点、小按钮）直接放进 `HorizontalLayout` 会**贴顶**，要包一层 `VerticalLayout { alignment: center; }` 或用 `y: (parent.height - h) / 2` 自己定位。
2. `HorizontalLayout` 里再嵌 `HorizontalLayout` 会把里面的 `Text`/`Tag` 拉伸，出现 200px+ 空隙。平铺 + 隐形 spacer 代替嵌套。
3. 组件体里**不能写** `Global.callback => {}` 或 `changed Global.prop`（解析器不支持）——全局里放 `in-out property` 当信号，组件用 `private property` 转一手再 `changed`。
4. `ScrollView` 的滚动位置是 `content-y`（`viewport-y` 已废弃）；`ListView` 用 `visible-width`（`viewport-width` 是内容宽度，别用）。
5. 卡片高度别写死：`Math.max(46px, lay.preferred-height)` + `clip: true`，否则字号放大到 115%/145% 会文字溢出压到下一张卡。
6. **`Window` 上不要写 `width` / `height`**：写了会让组件根元素的 layout-info 变成 `min == max`（实测 1600/1600），
   Slint 的 winit 后端据此判定「窗口不可缩放」，直接摘掉 `WS_THICKFRAME` 和最大化按钮——表现就是拖不动、也没有最大化。
   初始尺寸由 Rust 侧 `window().set_size()` 给（见 `restore_win_size`），`ui/app.slint` 里最多写 `min-height`。
7. **根内容元素写 `width: root.width` 同样会钉死窗口**：整个界面铺满窗口要交给布局（`horizontal-stretch: 1` / `100%`），
   任何 `root.width - Npx` 的派生宽度都会把 min/max 算成同一个值。
8. 四栏（工程 / 节点表 / 队列 / 日志）的宽度用 `min-width` + `preferred-width` + `max-width` + `horizontal-stretch` 表达，
   中栏必须显式写 `max-width: 100000px`：不写时 Slint 把它的 max 当成和 min 一样大，窗口最宽只能到「几栏 max 之和」。
9. `ScrollView` 里的内容宽度绑**面板**宽度（`left-body.width - 18px`），不要绑 `visible-width` 或 `root.width`
   ——前者和 ScrollView 的 preferred-width 互相依赖，后者会把窗口尺寸钉死。

窗口尺寸记忆：`meta` 里 `win_w` / `win_h`（逻辑像素，物理像素按 scale-factor 折回）+ `win_max`。
`on_close_requested` 落库一次，另有 1s 轮询兜底（被强杀也不丢）；启动 `restore_win_size()` 读回来并夹进当前屏幕工作区
（`windrop::work_area()`），换显示器不会开出比屏幕还大的窗口。最大化时只记 `win_max`、不覆盖宽高。

---

## 7. 构建、运行、自测

```bash
cargo build              # debug，保留控制台方便看 eprintln!
cargo build --release    # 产物 target/release/houdini-render-queue.exe
```

- **分发包必须自包含**：`.cargo/config.toml` 给 `x86_64-pc-windows-msvc` 加了 `-C target-feature=+crt-static`，
  产物不依赖 `VCRUNTIME140.dll` / VC++ 运行库，**别删**（删了就会出现「我这儿能跑、别人电脑打不开」）。
- 运行需要 exe 同级有 `python/`（`pyproc::script_path` 按 `exe/python/` → `../../python/` → `./python/` 顺序找）。
  脚本**不**编进 exe，分发时 exe 与 `python/` 必须一起拷。
- 分发：改完 exe 与 python 都要复制到 `dist/`，以及运行实例目录 `C:\BatchRender\houdini-render-queue\`（**被占用时需先关程序**）。
- **出包是验证的固定收尾动作**：每次改完并验证通过，就 `cargo build --release` 并把 exe 同步到 `dist/` 和实例目录，不用等用户提。
  只改 Rust/Slint 时只需拷 exe（界面已编进 exe）；`python/` 有改动才一起同步。拷贝前先 `tasklist | grep houdini-render` 确认没有实例在跑。

调试开关（`main.rs` 里解析）：

```
--add <hip> / --run / --drop-test <path>
--snapshot <file.bmp> [--snapshot-delay <ms>]     抓主窗口截图，写 BMP
--light / --set-font <字体> <字号> / --set-version <下标>
--probe-version <hip> / --font-panel
--font-filter <词>                                 预置字体搜索词，验证过滤
--probe-window                                     打印根元素 layout-info（窗口拖不动时先看 min 是否等于 max）
--list-fonts                                       打印枚举到的系统字体（带序号）后退出
--debug-invoke clear-finished|restart-all          直接触发回调，做回归
--debug-invoke worker-slots:<id>:<n>               调某台 worker 的槽位（同设备面板 +/-）
--debug-invoke worker-pause:<id>:<0|1>             停用/启用某台 worker
--debug-invoke-delay <ms>                          推迟上面这条的触发时间（多机要等 worker 连上）
--listen [端口]                                    开多机监听（默认 7878），不起就不走多机路径
--worker --connect <host:port> [--slots n]         当执行端（不初始化界面/数据库）
--devices-open                                     启动即展开设备面板，配合 --snapshot 回归
```

字体这块的两个坑（`src/fonts.rs`）：枚举用 `fontdb`（Slint 内部查字体用的同一个库），
不要换回 GDI 的 `EnumFontFamiliesExW`——它报的是 face name，会把 `Bahnschrift Light`
这类字重变体全列出来、还漏中文族名，喂给 Slint 一半匹配不上；`meta` 里存的是族名
`font_family_name`（不是下标，枚举条数换机器就变），老库的 `font_family_index` 只在读不到
族名时兜底并顺手迁移。

自测三板斧：

1. **不装 Houdini 跑 Python**：`PYTHONPATH=devtest python python/analyze.py devtest/out/sh010.hip`，`devtest/hou.py` 是假 hou 模块。
2. **隔离状态库**：`APPDATA=<临时目录>`，可以随便灌假任务、随便重启，不动用户真实队列。
3. **界面回归**：`--snapshot` 出图 + PIL 量像素。**`PostMessage` 伪造点击无效**（winit 不响应），真点击必须 `SetCursorPos` + `mouse_event`；弹层是独立窗口，`--snapshot` 抓不到，要用 `PrintWindow`。

---

## 8. 改代码前必须知道的坑

1. **别杀用户正在跑的实例**：`taskkill /IM houdini-render-queue.exe` 会连用户自己的队列一起杀。只按 PID 精确结束。
2. **只拷 `state.db` 不拷 `-wal`** 会读到旧数据；还原时先删 `state.db-wal` / `-shm` 再拷。
3. **File Cache 的 Load From Disk 必须关**：`filecache::2.0` 开着它时 cook 出来的是「去磁盘读缓存」，新目录没文件 → 空几何，代理 ROP 再把空的 902 字节 bgeo 写回同一路径，整段缓存自我污染。`render.py` 的 `disable_loadfromdisk()` 就是干这个的，别当成多余代码删掉。
4. **空缓存要报失败**：所有帧输出都 <1KB 时 `##FAIL`，不能静默报完成。
5. **SOP 缓存节点没有 `render()`**：要临时建一个 `/out` 下的 `geometry` ROP 当代理，帧号从展开路径还原成 `$F4` 模板。
6. **`hou.setUpdateMode(Manual)` 下 `node.geometry()` 不触发 cook**，探针会得到 0 点。探上游用 AutoUpdate。
7. **`HOME` 指到用户主目录 = 所有 package 静默失效**（这是「解析失败」最常见的原因）。hython 拿 `HOME` 当 Houdini 家目录：`HOME=C:\Users\<name>` 时 pref dir 解析成 `C:\Users\<name>\houdini21.0`，`Documents\houdini21.0\packages` 一个都不加载，用到 `axiom_solver_core::3.3` 的 hip 直接 `Bad node type found`。GUI 的 `houdini.exe` 自己会把 `HOME` 改成 `%USERPROFILE%\Documents`，所以只有我们起的子进程会中招。现在 `pyproc::hidden()` 里的 `align_home_env()` 会在 `HOME == %USERPROFILE%` 且没显式 `HOUDINI_USER_PREF_DIR` 时把 `HOME` 摘掉，对齐 GUI 行为；**别删这段**。从 git bash 手动探生产 hip 仍要带 `HOUDINI_USER_PREF_DIR=C:/Users/Gaole/Documents/houdini21.0`（bash 的 `HOME=/c/Users/Gaole` 同样会踩）。排查手法：`hython -c "import hou;print(hou.getenv('HOUDINI_USER_PREF_DIR'))"` 看 pref dir 落在哪。
8. **状态字符串中英双写**：库和快照里 `status` 可能存 `success` 也可能存 `成功`，`JobStatus::parse` 两者都认。新增状态别忘了同步。
9. **引擎主循环用 `recv_timeout(120ms)` + `pending_cmd`**，不要改回 `sleep`，否则勾选/删除要干等一轮。
10. **`cargo:rustc-link-arg` 不能带引号**，路径有空格时先把 `.res` 复制到无空格目录。
11. **帧范围输入框：`edited` 只记草稿，`committed` 才提交**。`TextInput.edited` 每敲一个字符都会触发，把 `1001-1050` 改成 `1001-1060` 的路上必然经过 `1001-`、`1001-106` 这种非法中间态（结束帧还没打完 / 比起始帧小）。把校验、写库、`sync_jobs`、报错全塞进 `edited` 的结果就是用户被自己的中间态反复打断、输入框被 `rebuild_nodes` 覆盖。现在的分工是：`edited` → `edit_range_draft`（只存取草稿 + 决定 `range-invalid` 红框），`committed`（回车 + 失焦）→ `commit_range`（解析、写库、进队列）。点「开始」「全选」等入口前先调 `flush_draft`，因为失焦和点击的先后顺序不可靠。**别再往 `edited` 里加任何写操作。**
12. **帧数是集合的帧数，不是 `fend - fstart + 1`**。`JobSpec.frames`（`src/frames.rs` 的 `FrameSet`）是唯一真源：
    `1052,1055,1056,1065-1070` 是 9 帧，而上下界 1052–1070 是 19 帧。进度、`plan.json`、断点续渲全部走
    `FrameSet::len()/expand()/count_done()`；`fstart`/`fend` 只用来拼显示文本和兼容老库。
    单个任务上限 `MAX_FRAMES = 100_000`，防止手滑 `1-99999999` 把内存撑爆。
13. **输入框不会自动回显「规范写法」**。用户一旦在 `TextInput` 里打过字，`text: node.range` 这条绑定就被
    Slint 解掉了（命令式赋值会移除绑定），所以 `commit_range` 里 `refresh_row` 改的是行模型，格子里仍然是
    用户亲手打的那串字。规范写法（相邻帧并成区间）在右侧执行区卡片上看，重新解析 / 切工程后格子也会更新。
14. **`devtest/hou.py` 必须与 `render.py` 用到的 hou API 同步**。`render.py` 会 `isinstance(node, hou.SopNode)`，
    假 hou 里缺这个类就直接 AttributeError 打断整轮渲染——补 `SopNode` 子类才能跑通端到端回归。

---

## 9. 常见改动怎么做

| 需求 | 动哪里 |
|---|---|
| 支持一种新的缓存/渲染节点类型 | `python/analyze.py` 的 `RENDER_TYPES` / `CACHE_TYPES` / `CACHE_TYPE_PREFIXES`，并在 `devtest/hou.py` 补一个用例 |
| 改并发 / 重试默认值 | `src/queue.rs` 的 `concurrency` / `max_retries` 初值（界面可调） |
| 改任务排序、插队逻辑 | `main.rs` 的 `insert_job` / `queue_everything` |
| 加界面元素 | `ui/app.slint`（属性 + 回调）→ `main.rs` 的 `on_*` 处理块 → 需要落库再加 `db.rs` |
| 换图标 | `python tools/make_icon.py --variant b` → 重编（`build.rs` 用 rc.exe）→ `python tools/peek_icon.py` 校验 |
| 加命令行调试开关 | `main.rs` 的 `main()` 参数解析（已有开关都在这儿） |

**版本号（`Cargo.toml` + 关于框）**：格式 `YYYY.M.N`，当前 `2026.2.1`。

- 小改动、或功能只做了一部分 → 只加第三位：`2026.2.0` → `2026.2.1` → `2026.2.2`。
- 只有**重大功能完整落地**才动第二位，第三位归零：`2026.2.x` → `2026.3.0`。
- 第二位代表「一个完整的功能阶段」，不要每次改完都往上加。

---

## 10. 词汇表

- **hip**：Houdini 工程文件（`.hip` / `.hiplc` / `.hipnc`）。
- **hython**：Houdini 自带的无界面 Python 解释器，本项目所有实际执行都在它里面。
- **ROP / LOP**：`/out` 下的输出节点 / `/stage` 下的 USD 渲染节点。
- **File Cache SOP**：`/obj` 下的 `filecache::2.0`，把上游几何烘成 `.bgeo.sc` 序列。
- **代理 ROP**：为 SOP 缓存临时创建的 `geometry` ROP，让 SOP 也能逐帧写盘。
- **预检（preflight）**：开渲前检查输出目录可写、磁盘 >500MB。
- **断点续渲**：按 `frames` 表跳过已完成帧。
- **pinned**：用户手动锁定了某个 hip 的 Houdini 版本，不再跟随自动识别。
