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
- **一次任务 = 一个节点 + 一段帧范围**，任务 ID 由 `工程 key` + `节点路径` 拼成，天然幂等。
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
src/main.rs      1771 行，界面状态与所有回调（App 结构体、工程列表、节点表、队列同步）
src/queue.rs      665 行，调度引擎：线程 + mpsc 指令通道 + 并发/重试/断点/取消
src/db.rs         359 行，SQLite：projects / jobs / frames / meta 四张表
src/model.rs      263 行，领域模型：Category、JobStatus、NodeInfo、Project、JobSpec、JobView、Event
src/analyze.rs    152 行，起 hython 跑 analyze.py，截取 JSON，带超时
src/houdini.rs    154 行，扫安装目录找 hython；读 hip 头部 `_HIP_SAVEVERSION` 得到制作版本
src/windrop.rs    724 行，OLE 拖放 + 窗口子类化（双击 / 大图标）
src/pyproc.rs      55 行，脚本路径定位 + 隐藏窗口的 Command
src/snap.rs        43 行，`--snapshot` 截图（调试用）

ui/app.slint     1324 行，全部界面：顶栏 / 三栏（工程、节点、队列）/ 日志列 / 底部状态栏
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
- `jobs`：任务快照（含 ord 顺序、status、attempts）
- `frames`：已完成帧，`(job_id, frame)` 主键
- `meta`：字体、主题等 UI 偏好

启动时读库重建队列；上次退出时处于 `running` 的任务会回落到 `queued`（进程已经没了）。

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

---

## 7. 构建、运行、自测

```bash
cargo build              # debug，保留控制台方便看 eprintln!
cargo build --release    # 产物 target/release/houdini-render-queue.exe
```

- 运行需要 exe 同级有 `python/`（`pyproc::script_path` 按 `exe/python/` → `../../python/` → `./python/` 顺序找）。
- 分发：改完 exe 与 python 都要复制到 `dist/`，以及运行实例目录 `C:\BatchRender\houdini-render-queue\`（**被占用时需先关程序**）。

调试开关（`main.rs` 里解析）：

```
--add <hip> / --run / --drop-test <path>
--snapshot <file.bmp> [--snapshot-delay <ms>]     抓主窗口截图，写 BMP
--light / --set-font <字体> <字号> / --set-version <下标>
--probe-version <hip> / --font-panel
--debug-invoke clear-finished|restart-all          直接触发回调，做回归
```

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
7. **从 git bash 起 hython 时 `HOME` 不对**，pref dir 会跑偏导致 packages（Axiom 等）不加载、`hipFile.load` 直接抛异常。手动探生产 hip 要带 `HOUDINI_USER_PREF_DIR=C:/Users/Gaole/Documents/houdini21.0`。
8. **状态字符串中英双写**：库和快照里 `status` 可能存 `success` 也可能存 `成功`，`JobStatus::parse` 两者都认。新增状态别忘了同步。
9. **引擎主循环用 `recv_timeout(120ms)` + `pending_cmd`**，不要改回 `sleep`，否则勾选/删除要干等一轮。
10. **`cargo:rustc-link-arg` 不能带引号**，路径有空格时先把 `.res` 复制到无空格目录。

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
