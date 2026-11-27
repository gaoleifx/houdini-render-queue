# Houdini Render Queue

本机桌面工具，用来管理 Houdini 的后台缓存与渲染队列。把 `.hip` 拖进窗口，自动分析可输出节点，勾选入队后按顺序后台执行。

## 功能

- **拖入 hip**：支持从资源管理器拖入文件或文件夹，也可点 `+` 添加
- **自动分析**：识别 `/out` 的 ROP、`/stage` 的 LOP 渲染节点，以及 `/obj` 下的 File Cache SOP（`filecache::2.0`）
- **逐节点设置**：每个节点独立修改帧范围（单击定位光标、双击整段选中），勾选即入队；缓存排在渲染之前
- **不连续帧**：帧范围可以写 `1001-1060`，也可以写 `1052,1055,1056,1065-1070`（逗号/分号/顿号分隔，区间分隔符认 `-` `–` `~` `至`；相邻帧提交后自动并成区间）
- **Houdini 版本**：探测本机所有安装；每个 hip 默认按文件里记录的制作版本匹配，也可手动锁定
- **执行控制**：默认串行，并发数可调；失败重试；按帧断点续渲；预检输出目录与磁盘空间
- **队列整理**：执行区卡片可上下调序、单条取消；「清空完成」摘掉已结束的任务；「重新开始」清零全部帧进度并重跑整批（两步确认，防误触）
- **界面**：三栏布局 + 最右侧独立日志列；底部状态栏显示当前 hip 完整路径与版本/节点统计；暗/亮主题；统一字体设置（本机全部系统字体，可搜索）；跟随与导出日志

## 结构

```
src/          Rust/Slint 主程序（队列调度、Windows 原生拖放、SQLite 状态库）
ui/app.slint  Slint 界面定义
python/
  analyze.py  在 hython 中加载 hip，枚举输出节点，输出 JSON
  render.py   在 hython 中按帧执行，逐帧回报进度，支持取消与断点续渲
assets/       app.ico（exe 图标）+ app.png（窗口图标）+ app.rc（资源脚本）
tools/        make_icon.py 生成图标；peek_icon.py / peek_window_icon.py 校验图标是否生效
devtest/      不启动 Houdini 也能跑回归的假 hou 模块
```

## 构建

需要 Rust 工具链（edition 2024）：

```bash
cargo build --release
```

产物在 `target/release/houdini-render-queue.exe`。运行时把 `python/` 目录放在 exe 同级，程序会按 `exe/python/` 定位脚本。

release 构建使用 Windows 图形子系统，双击不弹控制台窗口；`cargo build`（debug）仍保留控制台，便于看 `eprintln!` 日志。hython 子进程统一走 `pyproc::hidden()`，带 `CREATE_NO_WINDOW`，不会闪黑框。

### 图标

`build.rs` 会调用 Windows SDK 的 `rc.exe` 把 `assets/app.rc`（内含 `app.ico`）编进 exe；rc.exe 不在 PATH 时按 `RC` 环境变量 → PATH → `Windows Kits\10\bin\*` 顺序自动找，找不到只警告、不影响构建。窗口图标走 `ui/app.slint` 的 `icon:`（`assets/app.png`），任务栏大图标由 `windrop::set_big_icon()` 从 exe 资源补上（winit 只设小图标）。

改图标：编辑 `tools/make_icon.py` 的配色/形状后

```bash
python tools/make_icon.py --variant b --preview out.png   # 先看效果
python tools/make_icon.py --variant b                     # 生成 assets/app.ico + app.png
python tools/peek_icon.py dist/houdini-render-queue.exe check.png   # 校验 exe 图标
```

## 运行

直接双击 exe，拖入 `.hip` 即可。命令行调试开关：

```
--add <hip>              启动时添加工程
--run                    启动后立即开始队列
--drop-test <path>       自检原生拖放通道
--snapshot <file.bmp>    生成界面快照（可配 --snapshot-delay <毫秒>）
--light                  强制亮色主题
--probe-version <hip>    探测 hip 的制作版本
--set-font <字体> <字号> 调试：切换字体并落库
--set-version <下标>     调试：切换当前工程的 Houdini 版本
--font-panel             调试：直接打开字体面板
--font-filter <词>       调试：预置字体搜索词
--list-fonts             调试：打印枚举到的系统字体（含序号）后退出
--debug-invoke <回调名>  调试：直接触发一次按钮回调（clear-finished / restart-all /
                         worker-slots:<id>:<n> / worker-pause:<id>:<0|1>），配合 --snapshot 做回归
--debug-invoke-delay <ms> 调试：推迟上面这条的触发时间（多机要等 worker 连上来）
--listen [端口]          启动即开启多机监听（默认 7878；平时用界面开关即可）
--token <口令>           多机口令；不给就自动生成并记住（见设备面板）
--worker --connect <地址> 当执行端，连到主控干活（不显示界面）；
                         地址写 auto 或省略则局域网自动发现主控
--slots <n>              worker 的并发槽位，默认 1
--net-info               打印本机 IP / 端口 / 口令与现成的 worker 命令后退出
--devices-open           调试：启动即展开设备面板，配合 --snapshot 做回归
```

## 多机渲染（局域网）

主控还是这个程序，其他机器只当执行端。**前提是工程和资源都在共享盘上、
用同一套路径（推荐 UNC，如 `\\NAS\proj\...`）**，否则别的机器打不开 hip。

主力机上打开程序 → 顶栏「设备」→ 面板底部点「开启多机」。面板会给出**现成的接入命令**
（含本机 IP 和口令），抄到别的机器上跑就行：

```
houdini-render-queue.exe --worker --connect 192.168.1.10:7878 --token 48DYCQ
```

口令是开启多机时自动生成的，存在库里，重启不变；想自己定就 `--token 你的口令`。
地址偷懒可以写 `--connect auto`（或直接 `--worker`）：worker 会在局域网里听主控广播
自己找过来，主控换 IP 也能自己回来。命令行党依旧可以 `--listen 7878` 启动即开。

不确定网络通不通，先在主控机上跑 `houdini-render-queue.exe --net-info`，
它把 IP、端口、口令和完整的 worker 命令一起打出来。

worker 会自己报本机的 Houdini 版本、连不上会自动重连（5 秒内），日志写在
`%TEMP%\HoudiniRenderQueue\worker.log`。主控顶栏的「设备」能看到谁接进来了、
各自在跑哪一块，也能直接调每台的并发槽、或者临时停用一台（比如有人要拿它干别的）。

几条行为约定：

- **按帧块派发**：一个任务会被切成若干段（第一刀对半分，之后谁空闲谁拿），
  每段是一个 hython 进程；断点续渲照旧，帧记录在 master 的库里。
- **块不会切得太碎**：主控记着每个任务实测的「加载耗时 / 单帧耗时」（按机器分别记），
  切出来的块至少够摊薄一次 hip 加载。实测数据存在库里，重启后仍然有效。
- **快的机器多拿帧**：按历史单帧耗时加权分活，但差距封顶 4 倍。
- **输出路径不带帧变量（`$F`）的任务不会分片**，整包给一台机器——否则多台同时写一个文件必坏。
- **解算类节点请手动切「顺序」**：队列卡片上有「并行 / 顺序」开关。跨帧依赖的节点
  分片后结果会错，而且错得隐蔽，程序判断不出来。
- **路径或版本不对会拒绝接单**，主控换一台机器派，不会渲到一半才失败。
- **一块失败换一台机器重试**；机器掉线时它手上的帧自动退回队列，任务不会卡在「执行中」。
- **主控重启不丢在跑的块**：worker 会继续跑完当前块，重连时把块报给新主控接管，
  断线期间渲完的帧也会补报，不会被重跑。
- 关掉多机 / 关掉主控 = 全部停止；worker 会跑完当前帧再退出，已完成的帧不丢。
- 没有开启多机时完全是单机模式，行为与以前一模一样。


## 分发到其他电脑

`cargo build --release` 的产物已经自包含，拷过去就能跑，目标机不用装任何运行库：

- **CRT 静态链接**（`.cargo/config.toml` 里的 `+crt-static`）：不依赖 `VCRUNTIME140.dll` 和 VC++ 2015-2022 运行库；
- **`python/` 目录要跟 exe 一起拷**（`analyze.py`、`render.py`），放在 exe 同级；
- 目标机要求：Windows 10/11 x64 + 装了 Houdini（或用 `HRQ_HYTHON` 指定 hython）。没有 Houdini 能开界面，但解析会失败。

打不开时按顺序排查：

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 「此应用无法在你的电脑上运行 / 请联系应用发行者」 | Win11 **智能应用控制**（Smart App Control）拦了未签名程序 | 设置 → 隐私和安全性 → Windows 安全中心 → 应用和浏览器控制 → 智能应用控制 → 关闭（关掉后要重装系统才能再开）；或给 exe 做代码签名 |
| 「找不到 VCRUNTIME140.dll」 | 拿到的是静态链接之前的旧包 | 换 `dist/houdini-render-queue.exe` 新版 |
| 双击后什么都没发生 | 系统低于 Win10，或 32 位系统 | 不支持；需 Win10/11 x64 |
| 「Windows 已保护你的电脑」 | SmartScreen 对无签名新程序的首启提示 | 点「更多信息」→「仍要运行」 |

## 环境

- Windows 10/11
- Houdini 安装目录默认在 `C:\Program Files\Side Effects Software\Houdini *`，也可用环境变量 `HRQ_HYTHON` 指定（分号分隔多个 hython.exe 路径）
- 状态库存放在 `%APPDATA%\HoudiniRenderQueue\state.db`

## 界面预览

![四列布局](ui-four-column.png)

![暗色主题](ui-dark-final.png)

![亮色主题](ui-light-final.png)
