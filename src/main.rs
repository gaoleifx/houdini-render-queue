// release 构建使用 Windows 图形子系统，双击运行不再弹出黑色控制台窗口；
// debug 构建仍保留控制台，方便查看 eprintln 调试日志。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod analyze;
mod db;
mod fonts;
mod frames;
mod houdini;
mod model;
mod pyproc;
mod queue;
mod snap;
mod windrop;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use slint::{Model, ModelRc, VecModel};
use walkdir::WalkDir;

use db::Db;
use frames::{FrameErr, FrameSet};
use model::*;
use queue::{Cmd, Engine};


// ───────────────────────── 应用状态 ─────────────────────────

struct App {
    projects: Vec<Project>,
    /// 当前中栏展示的节点索引 (项目下标, 节点下标)
    visible: Vec<(usize, usize)>,
    /// 节点表的模型本身（界面共用这一份）：勾选只改对应行，不重建整个列表
    node_rows: Rc<VecModel<OutNode>>,
    /// 执行区的任务定义（顺序即执行顺序）
    jobs: Vec<JobSpec>,
    /// 引擎回推的任务快照
    views: Vec<JobView>,
    /// 已入队节点：job_id -> 状态
    queued: HashMap<String, JobStatus>,
    /// 节点帧范围覆盖：job_id -> 帧集合（可以是 1052,1055,1065-1070 这种不连续帧）
    ranges: HashMap<String, FrameSet>,
    /// 正在编辑但还没提交的帧范围草稿 (行下标, 文本)：
    /// 打字过程中只记在这里，不写库、不写 ranges，回车/失焦才提交
    draft: Option<(usize, String)>,
    /// 上一次报过的帧范围错误：同样的错误连着来（回车后又失焦）只记一条
    last_range_err: Option<(String, std::time::Instant)>,
    search: String,
    filter: i32,
    only_unadded: bool,
    log: Vec<String>,
    log_dirty: bool,
    installs: Vec<houdini::HoudiniInstall>,
    hud_index: usize,
    concurrency: usize,
    retries: u32,
    last_sig: String,
    drag_hover: bool,
    engine: Engine,
    ui_db: Db,
    evt_tx: mpsc::Sender<Event>,
    autorun: bool,
}

fn job_id(key: &str, node: &str) -> String {
    format!("{key}::{node}")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

fn push_log(app: &mut App, line: String) {
    eprintln!("{line}");
    app.log.push(line);
    if app.log.len() > 4000 {
        let d = app.log.len() - 4000;
        app.log.drain(0..d);
    }
    app.log_dirty = true;
}

fn flush_log(ui: &AppWindow, app: &mut App) {
    if !app.log_dirty {
        return;
    }
    app.log_dirty = false;
    let start = app.log.len().saturating_sub(1500);
    ui.set_log_text(app.log[start..].join("\n").into());
}

// ───────────────────────── 模型刷新 ─────────────────────────

fn rebuild_shots(ui: &AppWindow, app: &App) {
    let shots: Vec<Shot> = app
        .projects
        .iter()
        .map(|p| {
            let mut tone = 2;
            let mut sub = p.file_name();
            let mut meta = String::new();
            if p.loading {
                tone = 1;
                meta = "解析中…".into();
            } else if let Some(e) = &p.error {
                // 错误信息可能带多行 hython 输出，左栏只取第一行
                tone = 3;
                meta = truncate(e.lines().next().unwrap_or(""), 40);
            } else {
                let caches = p.nodes.iter().filter(|n| n.cat() == Category::Cache).count();
                let renders = p.nodes.len() - caches;
                // 自动识别到的制作版本与实际使用版本不一致时，把两者都写出来
                let vtxt = match &p.auto_version {
                    Some(v) if p.pinned && !v.eq_ignore_ascii_case(&p.houdini) => {
                        format!("v{v} → {}", p.houdini)
                    }
                    Some(v) => format!("v{v}"),
                    None => format!("v{}", p.houdini),
                };
                meta = format!("{vtxt} · 缓存{caches} 渲染{renders}");
            }
            if sub.is_empty() {
                sub = p.shot.clone();
            }
            Shot {
                name: p.shot.clone().into(),
                sub: sub.into(),
                meta: meta.into(),
                tone,
            }
        })
        .collect();
    ui.set_shots(ModelRc::new(VecModel::from(shots)));
    ui.set_project_count(app.projects.len().to_string().into());
}

/// 版本下拉的候选项：第一项是「自动」（跟随 hip 里记录的制作版本），
/// 后面依次是本机安装的各个 Houdini。返回 (候选项, 当前选中下标)。
fn version_options(app: &App, p: &Project) -> (Vec<slint::SharedString>, i32) {
    // 「自动」显示 hip 的制作版本；读不到就显示会实际使用的默认版本。
    // 同系列补丁号差异不在这里展开（日志里有），免得下拉文字被截断。
    let shown = p.auto_version.clone().unwrap_or_else(|| p.houdini.clone());
    let mut opts: Vec<slint::SharedString> = vec![format!("自动 · {shown}").into()];
    opts.extend(app.installs.iter().map(|i| slint::SharedString::from(i.version.clone())));
    let idx = if p.pinned {
        app.installs
            .iter()
            .position(|i| i.version == p.houdini)
            .map(|i| i as i32 + 1)
            .unwrap_or(0)
    } else {
        0
    };
    (opts, idx)
}

/// 依据 hip 的制作版本挑本机安装：优先完全一致 → 同主次版本 → 默认版本。
fn resolve_install(
    installs: &[houdini::HoudiniInstall],
    default_idx: usize,
    auto: Option<&str>,
) -> (Option<usize>, &'static str) {
    let d = default_idx.min(installs.len().saturating_sub(1));
    if let Some(v) = auto {
        if let Some(i) = houdini::match_install(installs, v) {
            let how = if installs[i].version == v {
                "版本一致"
            } else {
                "同系列"
            };
            return (Some(i), how);
        }
        if installs.is_empty() {
            return (None, "本机没有可用的 Houdini");
        }
        return (Some(d), "本机未安装该版本，改用默认");
    }
    if installs.is_empty() {
        (None, "hip 里没有版本记录，且本机没有可用 Houdini")
    } else {
        (Some(d), "hip 里没有版本记录，用默认")
    }
}

/// 单个节点在当前筛选/入队状态下的列表行；不满足筛选条件时返回 None。
/// needle 是已小写化的搜索词，循环里不要重复 to_lowercase。
fn node_row(app: &App, needle: &str, pi: usize, ni: usize) -> Option<OutNode> {
    let project = app.projects.get(pi)?;
    let n = project.nodes.get(ni)?;
    let cat = n.cat();
    if app.filter == 1 && cat != Category::Cache {
        return None;
    }
    if app.filter == 2 && cat != Category::Render {
        return None;
    }
    if !needle.is_empty() {
        let hay = format!(
            "{} {} {}",
            n.path,
            n.ntype,
            n.output.clone().unwrap_or_default()
        )
        .to_lowercase();
        if !hay.contains(needle) {
            return None;
        }
    }
    let id = job_id(&project.key, &n.path);
    let status = app.queued.get(&id).copied();
    if app.only_unadded && status.is_some() {
        return None;
    }
    let fr = app
        .ranges
        .get(&id)
        .cloned()
        .unwrap_or_else(|| default_range(project, n));
    Some(OutNode {
        checked: status.is_some(),
        path: n.path.clone().into(),
        ntype: n.ntype.clone().into(),
        category: cat.label().into(),
        output: n
            .output
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "—".into())
            .into(),
        range: fr.label().into(),
        status: status.map(|s| s.label()).unwrap_or("未加入").into(),
        tone: status.map(|s| s.tone()).unwrap_or(0),
    })
}

/// 只改状态真变了的那几行：勾选 / 队列回推时不要再重建整张表
fn refresh_row(app: &App, idx: usize) {
    let Some(&(pi, ni)) = app.visible.get(idx) else {
        return;
    };
    let needle = app.search.to_lowercase();
    let Some(row) = node_row(app, &needle, pi, ni) else {
        return;
    };
    let same = app
        .node_rows
        .row_data(idx)
        .map(|cur| {
            cur.checked == row.checked
                && cur.status == row.status
                && cur.tone == row.tone
                && cur.range == row.range
        })
        .unwrap_or(false);
    if !same {
        app.node_rows.set_row_data(idx, row);
    }
}

/// 队列状态变化后逐行对齐（行的集合不变时用这个，别整表重建）
fn refresh_rows(app: &App) {
    let needle = app.search.to_lowercase();
    for idx in 0..app.visible.len() {
        let Some(&(pi, ni)) = app.visible.get(idx) else {
            continue;
        };
        let Some(row) = node_row(app, &needle, pi, ni) else {
            continue;
        };
        let same = app
            .node_rows
            .row_data(idx)
            .map(|cur| {
                cur.checked == row.checked
                    && cur.status == row.status
                    && cur.tone == row.tone
                    && cur.range == row.range
            })
            .unwrap_or(false);
        if !same {
            app.node_rows.set_row_data(idx, row);
        }
    }
}

fn rebuild_nodes(ui: &AppWindow, app: &mut App) {
    // 整表重建会换掉所有行实例：草稿里的行下标立刻失效，直接丢弃（别把脏值提交到别的行）
    app.draft = None;
    ui.set_range_invalid(-1);
    let cur = (ui.get_current_shot().max(0) as usize).min(app.projects.len().saturating_sub(1));
    app.visible.clear();

    let Some(project) = app.projects.get(cur).cloned() else {
        app.node_rows.set_vec(Vec::<OutNode>::new());
        ui.set_shot_title("输出节点".into());
        ui.set_node_empty_hint("还没有导入工程".into());
        ui.set_status_hip("".into());
        ui.set_status_meta("".into());
        ui.set_version_options(ModelRc::new(VecModel::from(
            Vec::<slint::SharedString>::new(),
        )));
        return;
    };

    // 每个 hip 一个版本下拉：默认「自动」，跟随 hip 里记录的制作版本
    let (ver_opts, ver_idx) = version_options(app, &project);
    ui.set_version_options(ModelRc::new(VecModel::from(ver_opts)));
    ui.set_version_index(ver_idx);
    ui.set_version_pinned(project.pinned);

    let needle = app.search.to_lowercase();
    let mut rows: Vec<OutNode> = Vec::new();
    let mut hits: Vec<(usize, OutNode)> = Vec::new();
    for ni in 0..project.nodes.len() {
        if let Some(row) = node_row(app, &needle, cur, ni) {
            hits.push((ni, row));
        }
    }
    for (ni, row) in hits {
        app.visible.push((cur, ni));
        rows.push(row);
    }

    let title = if project.loading {
        format!("{} · 正在解析…", project.shot)
    } else {
        format!("{} · 输出节点 {}", project.shot, project.nodes.len())
    };
    ui.set_shot_title(title.into());
    app.node_rows.set_vec(rows);

    // 底部状态栏：完整 hip 路径 + 版本/节点统计
    let caches = project
        .nodes
        .iter()
        .filter(|n| n.cat() == Category::Cache)
        .count();
    let meta = if project.loading {
        format!("v{} · 解析中…", project.houdini)
    } else {
        format!(
            "v{} · 节点 {}（缓存 {} / 渲染 {}）",
            project.houdini,
            project.nodes.len(),
            caches,
            project.nodes.len() - caches
        )
    };
    ui.set_status_hip(project.hip.display().to_string().into());
    ui.set_status_meta(meta.into());
    let hint: String = if project.loading {
        "正在用 hython 解析工程，请稍候…".into()
    } else if let Some(e) = &project.error {
        format!(
            "解析失败：{}\n\n可以点上方「重新解析」重试，详细输出见下方日志",
            e.lines().next().unwrap_or("")
        )
    } else if project.nodes.is_empty() {
        "工程里没有找到可输出的缓存或渲染节点".into()
    } else {
        "没有符合筛选条件的节点".into()
    };
    ui.set_node_empty_hint(hint.into());
}

fn qtask_of(v: &JobView) -> QTask {
    QTask {
        nid: v.id.clone().into(),
        title: v.title.clone().into(),
        shot: v.shot.clone().into(),
        category: v.category.clone().into(),
        range: v.range.clone().into(),
        status: v.status.clone().into(),
        progress: v.progress,
        frames: v.frames.clone().into(),
        tone: v.tone,
    }
}

fn rebuild_queue(ui: &AppWindow, app: &App) {
    let model = ui.get_queue();
    if model.row_count() == app.views.len() {
        for (i, v) in app.views.iter().enumerate() {
            model.set_row_data(i, qtask_of(v));
        }
    } else {
        let rows: Vec<QTask> = app.views.iter().map(qtask_of).collect();
        ui.set_queue(ModelRc::new(VecModel::from(rows)));
    }
    ui.set_queue_count(app.views.len().to_string().into());

    let running = app
        .views
        .iter()
        .filter(|v| v.status == JobStatus::Running.label())
        .count();
    let queued = app
        .views
        .iter()
        .filter(|v| v.status == JobStatus::Queued.label())
        .count();
    let failed = app
        .views
        .iter()
        .filter(|v| v.status == JobStatus::Failed.label())
        .count();
    let (mut st, mut tone) = if running > 0 {
        (format!("运行中 · {} 执行 / {} 排队", running, queued), 1)
    } else if queued > 0 {
        // 只是排队、还没点开始，不该写「运行中」
        (format!("待执行 · {} 排队", queued), 4)
    } else {
        ("空闲".to_string(), 0)
    };
    if failed > 0 {
        st.push_str(&format!(" · {} 失败", failed));
        tone = 3;
    }
    ui.set_status_text(st.into());
    // 顶栏状态灯跟着走：灰=空闲，蓝=运行，琥珀=待执行，红=有失败
    ui.set_status_tone(tone);
}

fn signature(views: &[JobView]) -> String {
    let mut s = String::new();
    for v in views {
        s.push_str(&v.id);
        s.push('=');
        s.push_str(&v.status);
        s.push(';');
    }
    s
}

// ───────────────────────── 队列增删 ─────────────────────────

fn default_range(p: &Project, n: &NodeInfo) -> FrameSet {
    let a = n.fstart.unwrap_or(p.global.0);
    let b = n.fend.unwrap_or(p.global.1);
    FrameSet::span(a, b)
}

fn make_spec(app: &App, pi: usize, ni: usize) -> Option<JobSpec> {
    let p = app.projects.get(pi)?;
    let n = p.nodes.get(ni)?;
    let id = job_id(&p.key, &n.path);
    let fr = app
        .ranges
        .get(&id)
        .cloned()
        .unwrap_or_else(|| default_range(p, n));
    Some(JobSpec {
        id,
        project_key: p.key.clone(),
        hip: p.hip.clone(),
        hython: p.hython.clone(),
        houdini: p.houdini.clone(),
        shot: p.shot.clone(),
        node_path: n.path.clone(),
        node_type: n.ntype.clone(),
        category: n.cat(),
        output: n.output.clone().unwrap_or_default(),
        frames: fr,
    })
}

/// 插入任务：缓存永远排在渲染之前（自动依赖排序，之后可手动上下调）
fn insert_job(app: &mut App, spec: JobSpec) {
    if app.jobs.iter().any(|j| j.id == spec.id) {
        return;
    }
    // 状态立即转给界面，不等引擎回环（回环要等下一轮调度）
    app.queued.insert(spec.id.clone(), JobStatus::Queued);
    let pos = if spec.category == Category::Cache {
        app.jobs
            .iter()
            .rposition(|j| j.category == Category::Cache)
            .map(|i| i + 1)
            .unwrap_or(0)
    } else {
        app.jobs.len()
    };
    let title = spec.title();
    app.jobs.insert(pos, spec);
    app.log_dirty = true;
    app.log.push(format!(
        "{}  已加入执行区：{}",
        pyproc::now_hms(),
        title
    ));
}

fn remove_job(app: &mut App, id: &str) {
    app.jobs.retain(|j| j.id != id);
    app.queued.remove(id);
}

fn queue_everything(app: &mut App) {
    let n = app.projects.len();
    for pi in 0..n {
        let count = app.projects[pi].nodes.len();
        for ni in 0..count {
            if let Some(spec) = make_spec(app, pi, ni) {
                insert_job(app, spec);
            }
        }
    }
    sync_jobs(app);
}

fn sync_jobs(app: &App) {
    app.engine.send(Cmd::SetJobs(app.jobs.clone()));
}

// ───────────────────────── 交互动作 ─────────────────────────

fn toggle_node(ui: &AppWindow, app: &mut App, idx: usize) {
    // 入队要读帧范围：先把还没提交的编辑落地，别让新任务带着旧范围进去
    flush_draft(ui, app);
    let Some(&(pi, ni)) = app.visible.get(idx) else {
        return;
    };
    let Some(spec) = make_spec(app, pi, ni) else {
        return;
    };
    if app.queued.contains_key(&spec.id) {
        let id = spec.id.clone();
        remove_job(app, &id);
    } else {
        insert_job(app, spec);
    }
    sync_jobs(app);
    // 「只看未加入」下勾选会让这一行消失，只能整表重建；平时只刷这一行
    if app.only_unadded {
        rebuild_nodes(ui, app);
    } else {
        refresh_row(app, idx);
    }
}

fn select_all(ui: &AppWindow, app: &mut App, on: bool) {
    flush_draft(ui, app);
    let list = app.visible.clone();
    for (pi, ni) in list {
        let Some(spec) = make_spec(app, pi, ni) else {
            continue;
        };
        if on {
            insert_job(app, spec);
        } else {
            let id = spec.id.clone();
            remove_job(app, &id);
        }
    }
    sync_jobs(app);
    rebuild_nodes(ui, app);
}

/// 从工程列表里移除一个工程：连带撤掉它在执行区里的任务、帧范围覆盖和数据库记录。
fn remove_project(ui: &AppWindow, app: &mut App, idx: usize) {
    if idx >= app.projects.len() {
        return;
    }
    let p = app.projects.remove(idx);

    let ids: Vec<String> = app
        .jobs
        .iter()
        .filter(|j| j.project_key == p.key)
        .map(|j| j.id.clone())
        .collect();
    app.jobs.retain(|j| j.project_key != p.key);
    for id in &ids {
        app.queued.remove(id);
        app.ui_db.delete_job(id);
    }

    let prefix = format!("{}::", p.key);
    app.ranges.retain(|k, _| !k.starts_with(&prefix));
    save_ranges(app);
    app.ui_db.delete_project(&p.key);

    // 引擎那边：还在跑的任务会收到停止请求，结束后自行清理
    sync_jobs(app);
    app.views.retain(|v| v.key != p.key);

    let n = app.projects.len();
    let cur = if n == 0 {
        0
    } else {
        (ui.get_current_shot().max(0) as usize).min(n - 1) as i32
    };
    ui.set_current_shot(cur);

    push_log(
        app,
        format!(
            "{}  已移除工程 {}（连带 {} 个执行区任务）",
            pyproc::now_hms(),
            p.shot,
            ids.len()
        ),
    );
    rebuild_shots(ui, app);
    rebuild_nodes(ui, app);
    rebuild_queue(ui, app);
}

// ───────────────────────── 帧范围编辑 ─────────────────────────
// 「输入过程」和「生效」必须分开。TextInput 每敲一个字符都会触发 edited，
// 而 1001-1050 改成 1001-1060 的路上必然经过 「1001-」/「1001-106」 这种中间态，
// 它们在语义上一定非法（结束帧还没打完 / 比起始帧小）。所以：
//   edited    → 只记草稿、只决定描边颜色；不写库、不重建、不报错、不覆盖输入
//   committed → 回车或失焦才解析提交；成功写库，失败保留文本 + 红框 + 一条日志
// 这样用户打字时不会被自己的中间态打断，也不会因为一次输入往队列里灌进 1-1、10-10 这类脏范围。

/// 只判断「这串字符还像不像个帧集合」，用于打字时的红框。
/// 空串、只打了半截（`1001-`）、末尾多一个逗号都不算错 —— 那是还没输完。
/// `1001-106` 也不算错，倒序要到提交时才有结论。
fn range_draft_bad(s: &str) -> bool {
    for tok in s.split([',', '，', ';', '；', '、']) {
        let t: String = tok
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| match c {
                '–' | '—' | '~' | '～' | '至' | '到' => '-',
                c => c,
            })
            .collect();
        if t.is_empty() {
            continue;
        }
        let mut dashes = 0;
        for c in t.chars() {
            match c {
                '0'..='9' => {}
                '-' => {
                    dashes += 1;
                    if dashes > 1 {
                        return true;
                    }
                }
                // 字母、小数点之类根本不像帧范围，立刻标红
                _ => return true,
            }
        }
    }
    false
}

fn parse_frames(s: &str) -> Result<FrameSet, FrameErr> {
    FrameSet::parse(s)
}

/// 打字中：只存草稿 + 决定红框，模型和库都不动
fn edit_range_draft(ui: &AppWindow, app: &mut App, idx: usize, text: String) {
    let bad = range_draft_bad(&text);
    app.draft = Some((idx, text));
    ui.set_range_invalid(if bad { idx as i32 } else { -1 });
}

/// 回车 / 失焦 / 点按钮前的强制提交：只有这里会真正改队列和库
fn commit_range(ui: &AppWindow, app: &mut App, idx: usize, text: String) {
    app.draft = None;
    let Some(&(pi, ni)) = app.visible.get(idx) else {
        ui.set_range_invalid(-1);
        return;
    };
    let (pkey, npath, defr) = {
        let Some(p) = app.projects.get(pi) else { return };
        let Some(n) = p.nodes.get(ni) else { return };
        (p.key.clone(), n.path.clone(), default_range(p, n))
    };
    let id = job_id(&pkey, &npath);

    let fr = match parse_frames(&text) {
        Ok(v) => v,
        Err(e) => {
            // 关键：不写库、不重建、不改写输入框。留着用户打的字 + 红框，他可以接着改
            ui.set_range_invalid(idx as i32);
            let shown = if text.trim().is_empty() {
                "（空）".to_string()
            } else {
                text.clone()
            };
            let msg = match e {
                FrameErr::Format => format!(
                    "帧集合格式不对：「{shown}」，可以写 1001-1060，也可以写 1052,1055,1065-1070"
                ),
                FrameErr::Reversed(a, b) => format!(
                    "帧集合不合法：「{shown}」，{a}-{b} 的结束帧小于起始帧"
                ),
                FrameErr::TooMany => format!(
                    "帧集合太大：「{shown}」，单个任务最多 {} 帧",
                    frames::MAX_FRAMES
                ),
            };
            if app.last_range_err.as_ref().map(|(m, _)| m == &msg) != Some(true) {
                push_log(app, format!("{}  {msg}", pyproc::now_hms()));
                app.last_range_err = Some((msg, std::time::Instant::now()));
            }
            return;
        }
    };
    ui.set_range_invalid(-1);

    let cur = app.ranges.get(&id).cloned().unwrap_or_else(|| defr.clone());
    if cur != fr {
        if fr == defr {
            app.ranges.remove(&id);
        } else {
            app.ranges.insert(id.clone(), fr.clone());
        }
        if let Some(j) = app.jobs.iter_mut().find(|j| j.id == id) {
            j.frames = fr;
            sync_jobs(app);
        }
        save_ranges(app);
    }
    // 只刷这一行的模型（勾选态/状态徽标/规范写法）。
    // 注意：输入框一旦被用户打过字，`text: node.range` 这条绑定就被 TextInput
    // 自己解掉了（Slint 语义：命令式赋值会移除绑定），所以格子里仍然显示用户
    // 亲手打的那串字；规范写法（1052,1055,1056 → 1052,1055-1056）在右侧执行区
    // 卡片上能看到，重新解析/切工程后格子里也会变成规范写法。
    if app.only_unadded {
        rebuild_nodes(ui, app);
    } else {
        refresh_row(app, idx);
    }
}

/// 点「开始」/勾选/全选之前，把没提交的草稿先落地，
/// 否则失焦和点击的先后顺序不可靠，会出现「改完点了开始，跑的还是旧范围」
fn flush_draft(ui: &AppWindow, app: &mut App) {
    if let Some((idx, text)) = app.draft.take() {
        commit_range(ui, app, idx, text);
    }
}

fn save_ranges(app: &App) {
    // 存规范写法：{"key|/obj/xxx": "1052,1055-1056,1065-1070"}
    let map: HashMap<&str, String> = app
        .ranges
        .iter()
        .map(|(k, v)| (k.as_str(), v.label()))
        .collect();
    let json = serde_json::to_string(&map).unwrap_or_else(|_| "{}".into());
    app.ui_db.set_meta("ranges", &json);
}

/// 读回帧范围覆盖。老库里是 `[起, 止]` 的数组，新的是 "1052,1055-1056" 字符串，
/// 两种都认，免得升级后用户的设置全丢
fn load_ranges(db: &Db) -> HashMap<String, FrameSet> {
    let mut out = HashMap::new();
    let Some(raw) = db.get_meta("ranges") else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return out;
    };
    let Some(obj) = v.as_object() else {
        return out;
    };
    for (k, val) in obj {
        let fr = match val {
            serde_json::Value::String(s) => FrameSet::parse(s).ok(),
            serde_json::Value::Array(a) if a.len() == 2 => match (a[0].as_i64(), a[1].as_i64()) {
                (Some(x), Some(y)) => Some(FrameSet::span(x as i32, y as i32)),
                _ => None,
            },
            _ => None,
        };
        if let Some(fr) = fr {
            out.insert(k.clone(), fr);
        }
    }
    out
}

// ───────────────────────── 导入 ─────────────────────────

fn collect_hips(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            for e in WalkDir::new(&p)
                .max_depth(10)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                let path = e.path();
                if path.is_file() && is_hip(path) {
                    out.push(path.to_path_buf());
                }
            }
        } else if is_hip(&p) {
            out.push(p);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_hip(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()),
        Some(ref e) if e == "hip" || e == "hipnc"
    )
}

/// 解析串行闸门：一次拖很多 hip 时不要同时起一堆 hython，
/// 每个 hython 都要吃一份 Houdini 许可证，并发抢不到就会闪退。
static ANALYZE_GATE: Mutex<()> = Mutex::new(());

/// 起后台线程跑 analyze.py 解析一个工程。
/// hython 偶尔会在启动阶段闪退（许可证被占、Houdini 自身崩），
/// 十几秒内就失败的再来一次——真有问题的话第二次照样失败，不会更糟。
fn start_analyze(app: &mut App, idx: usize) {
    let Some(p) = app.projects.get(idx) else {
        return;
    };
    let (key, hip, hython) = (p.key.clone(), p.hip.clone(), p.hython.clone());
    app.projects[idx].loading = true;
    app.projects[idx].error = None;

    let tx = app.evt_tx.clone();
    std::thread::spawn(move || {
        let _serial = ANALYZE_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = std::time::Instant::now();
        let mut res = analyze::run(&hython, &hip, Duration::from_secs(300));
        if res.is_err() && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(800));
            res = analyze::run(&hython, &hip, Duration::from_secs(300));
        }
        match res {
            Ok(out) => {
                let payload = AnalyzePayload {
                    houdini: out
                        .data
                        .houdini
                        .clone()
                        .unwrap_or_else(|| hython.display().to_string()),
                    global: out.data.global.default_range(),
                    nodes: out.data.nodes.clone(),
                };
                let _ = tx.send(Event::Analyzed {
                    key,
                    result: Ok(payload),
                    logs: out.logs,
                });
            }
            Err(e) => {
                let _ = tx.send(Event::Analyzed {
                    key,
                    result: Err(e),
                    logs: Vec::new(),
                });
            }
        }
    });
}

/// 重解析当前选中的工程（解析失败后不必删了重加）
fn reanalyze(ui: &AppWindow, app: &mut App) {
    let idx = (ui.get_current_shot().max(0) as usize).min(app.projects.len().saturating_sub(1));
    let Some(p) = app.projects.get(idx) else {
        return;
    };
    push_log(
        app,
        format!("{}  重新解析 {} …", pyproc::now_hms(), p.shot),
    );
    start_analyze(app, idx);
    rebuild_shots(ui, app);
    rebuild_nodes(ui, app);
}

/// 切换当前工程使用的 Houdini 版本。
/// sel: 0 = 自动（跟随 hip 制作版本），i>0 = 本机第 i-1 个安装。
fn set_project_version(ui: &AppWindow, app: &mut App, sel: i32) {
    let idx = (ui.get_current_shot().max(0) as usize).min(app.projects.len().saturating_sub(1));
    let Some(p) = app.projects.get(idx) else {
        return;
    };
    let key = p.key.clone();
    let auto = p.auto_version.clone();

    let (target, how) = if sel <= 0 {
        let (ti, how) = resolve_install(&app.installs, app.hud_index, auto.as_deref());
        (ti.and_then(|i| app.installs.get(i).cloned()), how)
    } else {
        (app.installs.get(sel as usize - 1).cloned(), "手动指定")
    };
    let Some(inst) = target else {
        push_log(app, format!("{}  没有可用的 Houdini 安装", pyproc::now_hms()));
        rebuild_nodes(ui, app);
        return;
    };

    let same = app.projects[idx].hython == inst.hython;
    app.projects[idx].houdini = inst.version.clone();
    app.projects[idx].hython = inst.hython.clone();
    app.projects[idx].pinned = sel > 0;

    // 执行区里该工程的待跑任务同步换版本（正在跑的等它跑完，不受影响）
    let mut synced = 0usize;
    for j in app.jobs.iter_mut().filter(|j| j.project_key == key) {
        if j.hython != inst.hython {
            j.hython = inst.hython.clone();
            j.houdini = inst.version.clone();
            synced += 1;
        }
    }
    if synced > 0 {
        sync_jobs(app);
    }

    let p = app.projects[idx].clone();
    let _ = app.ui_db.save_project(&p);

    let mode = if sel > 0 {
        "手动锁定".to_string()
    } else {
        format!("自动 · {how}")
    };
    let mut line = format!(
        "{}  {} 改用 Houdini {}（{}）",
        pyproc::now_hms(),
        p.shot,
        inst.version,
        mode
    );
    if synced > 0 {
        line.push_str(&format!("，{} 个待执行任务已同步", synced));
    }
    if !same {
        line.push_str("，重新解析中…");
    }
    push_log(app, line);

    if !same {
        start_analyze(app, idx);
    }
    rebuild_shots(ui, app);
    rebuild_nodes(ui, app);
}

fn add_paths(ui: &AppWindow, app: &mut App, paths: Vec<PathBuf>) {
    let hips = collect_hips(paths);
    if hips.is_empty() {
        push_log(app, format!("{}  没有找到 .hip / .hipnc 文件", pyproc::now_hms()));
        return;
    }
    if app.installs.is_empty() {
        push_log(
            app,
            format!("{}  没有探测到 Houdini 安装，无法解析工程", pyproc::now_hms()),
        );
        return;
    }

    let mut added = 0usize;
    let mut first_new: Option<usize> = None;
    for hip in hips {
        if app.projects.iter().any(|p| p.hip == hip) {
            continue;
        }
        let shot = hip
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                hip.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "shot".into())
            });
        // 逐 hip 认版本：读 hip 里记录的 _HIP_SAVEVERSION，挑本机对应安装
        let auto_ver = houdini::detect_hip_version(&hip);
        let (tid, how) = resolve_install(&app.installs, app.hud_index, auto_ver.as_deref());
        let Some(ti) = tid else {
            push_log(
                app,
                format!("{}  跳过 {}：本机没有可用的 Houdini", pyproc::now_hms(), hip.display()),
            );
            continue;
        };
        let install = app.installs[ti].clone();
        let vdesc = match &auto_ver {
            Some(v) if install.version == *v => format!("hip 制作版本 {v}"),
            Some(v) => format!("hip 制作版本 {v}，{}", how),
            None => how.to_string(),
        };        let proj = Project {
            key: hip.to_string_lossy().to_string(),
            hip: hip.clone(),
            shot,
            houdini: install.version.clone(),
            hython: install.hython.clone(),
            auto_version: auto_ver,
            pinned: false,
            nodes: Vec::new(),
            global: (1, 100),
            loading: true,
            error: None,
        };
        push_log(
            app,
            format!(
                "{}  开始解析 {}（{}；使用 Houdini {}）",
                pyproc::now_hms(),
                hip.display(),
                vdesc,
                install.version
            ),
        );
        if first_new.is_none() {
            first_new = Some(app.projects.len());
        }
        app.projects.push(proj);
        added += 1;
        let idx = app.projects.len() - 1;
        start_analyze(app, idx);
    }

    rebuild_shots(ui, app);
    if added == 0 {
        push_log(app, format!("{}  文件都已在列表中", pyproc::now_hms()));
        return;
    }
    if let Some(i) = first_new {
        ui.set_current_shot(i as i32);
    }
    rebuild_nodes(ui, app);
}

// ───────────────────────── main ─────────────────────────

use slint::private_unstable_api::re_exports as sk;

/// 字体设置：第 0 项是「系统默认」，其余从本机枚举（见 fonts.rs），常用字体置顶。
/// 下拉框里让用户认中文名，真正喂给 Slint 的是 `id`（族名）。
const FONT_SIZE_LABELS: &[&str] = &["紧凑 100%", "标准 115%", "舒适 130%", "大号 145%"];
const FONT_SCALES: &[f32] = &[1.0, 1.15, 1.3, 1.45];

/// 字体落地：正文字体、等宽字体、字号倍率。所有字号在 UI 里都过 Fonts.size()。
fn apply_fonts(ui: &AppWindow, fams: &[fonts::Family], family_idx: usize, size_idx: usize) {
    let g = ui.global::<Fonts>();
    let fi = family_idx.min(fams.len().saturating_sub(1));
    let si = size_idx.min(FONT_SCALES.len() - 1);
    let body = fams.get(fi).map(|f| f.id.as_str()).unwrap_or("");
    g.set_body(body.into());
    g.set_mono("Consolas".into());
    g.set_scale(FONT_SCALES[si]);
}

/// 字体下拉的状态：全量列表、过滤后的可见项、以及真正生效的那一款。
///
/// `shown` 是「下拉下标 → 全量下标」的映射。过滤只改变下拉里显示什么，
/// 不动 `current`，所以搜了别的词再清空，界面字体还是原来那款。
struct FontPicker {
    all: Vec<fonts::Family>,
    shown: Vec<usize>,
    current: usize,
    filter: String,
}

impl FontPicker {
    fn new(all: Vec<fonts::Family>) -> Self {
        let mut p = Self {
            all,
            shown: Vec::new(),
            current: 0,
            filter: String::new(),
        };
        p.recompute();
        p
    }

    /// 按过滤词重算可见项（族名和中文显示名都参与匹配，忽略大小写）
    fn recompute(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        self.shown = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                needle.is_empty()
                    || f.label.to_lowercase().contains(&needle)
                    || f.id.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
    }

    /// 当前字体在下拉里的位置；被过滤掉时返回 -1（下拉显示空白，但字体不变）
    fn cursor(&self) -> i32 {
        self.shown
            .iter()
            .position(|&i| i == self.current)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    /// 把可见项和选中位置推给界面。先换 model 再定 index：换 model 会让
    /// ComboBox 把 current-index 重新 clamp 一遍，顺序反了会指到别的字体上
    fn refresh(&self, ui: &AppWindow) {
        let labels: Vec<slint::SharedString> = self
            .shown
            .iter()
            .map(|&i| self.all[i].label.as_str().into())
            .collect();
        ui.set_font_families(ModelRc::new(VecModel::from(labels)));
        ui.set_font_family_index(self.cursor());
    }
}

/// 首次运行的默认窗口尺寸（逻辑像素）。Slint 那边不再写死，全由这里给
const DEFAULT_WIN_SIZE: (f32, f32) = (1600.0, 820.0);
/// 最小窗口尺寸：再小四栏就互相挤到看不清了
const MIN_WIN_SIZE: (f32, f32) = (1180.0, 660.0);

/// 上次关闭时的窗口尺寸（逻辑像素）+ 是否最大化；读不到就用默认值
fn saved_win_size(app: &App) -> (f32, f32, bool) {
    let read = |k: &str| app.ui_db.get_meta(k).and_then(|s| s.parse::<f32>().ok());
    let w = read("win_w").unwrap_or(DEFAULT_WIN_SIZE.0);
    let h = read("win_h").unwrap_or(DEFAULT_WIN_SIZE.1);
    let max = app.ui_db.get_meta("win_max").map(|v| v == "1").unwrap_or(false);
    (w, h, max)
}

/// 把存下来的尺寸夹进 [最小可用尺寸, 当前屏幕工作区]：
/// 上次在 4K 上关掉、这次换 1080p 打开，不至于开出一个比屏幕还大的窗口；
/// 反过来存了个针尖大的尺寸也不会把界面压成一团
fn clamp_win_size(w: f32, h: f32, area_w: f32, area_h: f32) -> (f32, f32) {
    let min_w = MIN_WIN_SIZE.0.min(area_w).max(320.0);
    let min_h = MIN_WIN_SIZE.1.min(area_h).max(240.0);
    (
        w.clamp(min_w, area_w.max(min_w)),
        h.clamp(min_h, area_h.max(min_h)),
    )
}

/// 启动时套用上次的尺寸。超出当前显示器工作区就夹回来（上次在大屏、这次换小屏）
fn restore_win_size(ui: &AppWindow, app: &App) {
    let (w, h, max) = saved_win_size(app);
    let sf = ui.window().scale_factor().max(0.5);
    let (aw, ah) = windrop::work_area();
    let (w, h) = clamp_win_size(w, h, aw as f32 / sf, ah as f32 / sf);
    ui.window().set_size(slint::LogicalSize::new(w, h));
    if max {
        ui.window().set_maximized(true);
    }
}

/// 把当前窗口尺寸记进 meta。物理像素按 scale-factor 折回逻辑像素再存，
/// 换了 DPI 也不会存出一个偏大一倍的数字。
/// 最大化时只记「最大化」标记、不覆盖宽高，这样下次取消最大化还能回到原来的尺寸。
fn save_win_size(ui: &AppWindow, app: &App) {
    let maxed = ui.window().is_maximized();
    if !maxed {
        let sf = ui.window().scale_factor().max(0.5);
        let size = ui.window().size();
        let (w, h) = (size.width as f32 / sf, size.height as f32 / sf);
        if w >= 400.0 && h >= 300.0 {
            app.ui_db.set_meta("win_w", &format!("{w:.0}"));
            app.ui_db.set_meta("win_h", &format!("{h:.0}"));
        }
    }
    app.ui_db.set_meta("win_max", if maxed { "1" } else { "0" });
}

/// 主题落地：界面 token、标准控件配色、系统标题栏三处一起切。
fn apply_theme(ui: &AppWindow, dark: bool) {
    ui.global::<Theme>().set_dark(dark);
    // ColorScheme 只有生成代码里的私有重导出，官方没有公开路径
    ui.global::<Palette>().set_color_scheme(if dark {
        sk::ColorScheme::Dark
    } else {
        sk::ColorScheme::Light
    });
    #[cfg(target_os = "windows")]
    windrop::set_dark_titlebar(windrop::hwnd_of(&ui.window()), dark);
}

fn main() -> Result<(), slint::PlatformError> {
    // 调试用：只打印 hip 版本识别结果并退出，不起界面
    if let Some(pos) = std::env::args().position(|a| a == "--probe-version") {
        let installs = houdini::detect_installs();
        let list: Vec<String> = installs.iter().map(|i| i.version.clone()).collect();
        println!("已安装: {list:?}");
        for p in std::env::args().skip(pos + 1) {
            let path = PathBuf::from(&p);
            let auto = houdini::detect_hip_version(&path);
            let m = auto
                .as_deref()
                .and_then(|v| houdini::match_install(&installs, v));
            println!(
                "{} -> 制作版本 {:?} -> 使用 {:?}",
                path.display(),
                auto,
                m.map(|i| installs[i].version.clone())
            );
        }
        return Ok(());
    }

    // 调试：只打印枚举到的系统字体并退出（排查「某个字体选了没反应」用）
    if std::env::args().any(|a| a == "--list-fonts") {
        let fams = fonts::families();
        println!("共 {} 项（含「系统默认」）", fams.len());
        for (i, f) in fams.iter().enumerate() {
            println!("{i:>3}  {}  [{}]", f.label, f.id);
        }
        return Ok(());
    }

    let ui = AppWindow::new()?;
    // 顶栏铭牌下的版本号：单一来源是 Cargo.toml 的 version，改版本只改那一处
    ui.set_app_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());

    let installs = houdini::detect_installs();

    let ui_db = Db::open().unwrap_or_else(|e| {
        eprintln!("数据库不可用: {e}");
        panic!("{e}")
    });
    let engine_db = Db::open().unwrap_or_else(|e| {
        eprintln!("数据库不可用: {e}");
        panic!("{e}")
    });

    // 启动时逐个 hip 认一遍制作版本；没被手动锁定的工程顺手修正到正确安装
    let mut projects = ui_db.load_projects();
    for p in projects.iter_mut() {
        p.auto_version = houdini::detect_hip_version(&p.hip);
        if !p.pinned {
            if let (Some(i), _) = resolve_install(&installs, 0, p.auto_version.as_deref()) {
                let inst = &installs[i];
                if p.hython != inst.hython {
                    p.hython = inst.hython.clone();
                }
                p.houdini = inst.version.clone();
            }
        }
    }
    let jobs: Vec<JobSpec> = engine_db
        .load_jobs()
        .into_iter()
        .map(|(s, _, _)| s)
        .collect();
    let ranges = load_ranges(&ui_db);

    let (evt_tx, evt_rx) = mpsc::channel::<Event>();
    let engine = Engine::spawn(evt_tx.clone(), engine_db);
    if !jobs.is_empty() {
        engine.send(Cmd::SetJobs(jobs.clone()));
    }

    let app = Rc::new(RefCell::new(App {
        projects,
        visible: Vec::new(),
        node_rows: Rc::new(VecModel::from(Vec::<OutNode>::new())),
        jobs,
        views: Vec::new(),
        queued: HashMap::new(),
        ranges,
        draft: None,
        last_range_err: None,
        search: String::new(),
        filter: 0,
        only_unadded: false,
        log: Vec::new(),
        log_dirty: true,
        installs,
        hud_index: 0,
        concurrency: 1,
        retries: 2,
        last_sig: String::new(),
        drag_hover: false,
        engine,
        ui_db,
        evt_tx: evt_tx.clone(),
        autorun: std::env::args().any(|a| a == "--run"),
    }));

    {
        let mut a = app.borrow_mut();
        let n = a.installs.len();
        push_log(
            &mut a,
            format!("{}  就绪：探测到 {} 个 Houdini 安装", pyproc::now_hms(), n),
        );
    }

    {
        let mut a = app.borrow_mut();
        rebuild_shots(&ui, &a);
        // 节点表界面和 Rust 侧共用同一份模型，之后只改行不换模型
        ui.set_nodes(ModelRc::from(a.node_rows.clone()));
        rebuild_nodes(&ui, &mut a);
        rebuild_queue(&ui, &a);
        flush_log(&ui, &mut a);
    }

    // 主题：默认暗色，记住上次的选择
    let force_light = std::env::args().any(|a| a == "--light");
    let dark = {
        let a = app.borrow();
        let saved = a.ui_db.get_meta("theme").map(|v| v != "light").unwrap_or(true);
        if force_light { false } else { saved }
    };
    apply_theme(&ui, dark);

    // 字体列表：启动时枚举一次本机系统字体（上百款），下拉框和回调共用这一份
    let picker = Rc::new(RefCell::new(FontPicker::new(fonts::families())));

    // 字体设置：从库里恢复，下拉框与全局 Fonts 一起初始化
    {
        let (fi, si) = {
            let a = app.borrow();
            let read = |k: &str, default: usize| {
                a.ui_db
                    .get_meta(k)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(default)
            };
            // 字体按族名恢复：枚举结果的条数因机器而异，存下标换了机器就会指到别的字体上。
            // 老库只有 font_family_index，读不到族名时退回下标（随后的写回就是迁移）
            let by_name = a.ui_db.get_meta("font_family_name").and_then(|n| {
                picker
                    .borrow()
                    .all
                    .iter()
                    .position(|f| f.id.eq_ignore_ascii_case(&n))
            });
            let family = by_name.unwrap_or_else(|| read("font_family_index", 0));
            (
                family.min(picker.borrow().all.len() - 1),
                read("font_size_index", 1).min(FONT_SCALES.len() - 1),
            )
        };
        let sizes: Vec<slint::SharedString> = FONT_SIZE_LABELS.iter().map(|s| (*s).into()).collect();
        ui.set_font_size_labels(ModelRc::new(VecModel::from(sizes)));
        ui.set_font_size_index(si as i32);
        {
            let mut p = picker.borrow_mut();
            p.current = fi;
            p.refresh(&ui);
        }
        apply_fonts(&ui, &picker.borrow().all, fi, si);
        // 迁移：这次解析出的族名写回库，下次就不必再猜下标
        {
            let a = app.borrow();
            let p = picker.borrow();
            a.ui_db.set_meta("font_family_name", &p.all[fi].id);
            a.ui_db.set_meta("font_family_index", &fi.to_string());
        }
        // 调试：--set-font <字体序号> <字号序号> 直接切换并落库（序号同 --list-fonts）
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--set-font") {
            let mut p = picker.borrow_mut();
            let nf = args
                .get(pos + 1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(fi)
                .min(p.all.len() - 1);
            let ns = args
                .get(pos + 2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(si)
                .min(FONT_SCALES.len() - 1);
            p.current = nf;
            p.refresh(&ui);
            ui.set_font_size_index(ns as i32);
            apply_fonts(&ui, &p.all, nf, ns);
            let a = app.borrow();
            a.ui_db.set_meta("font_family_name", &p.all[nf].id);
            a.ui_db.set_meta("font_family_index", &nf.to_string());
            a.ui_db.set_meta("font_size_index", &ns.to_string());
        }
        if std::env::args().any(|a| a == "--font-panel") {
            ui.set_font_panel_open(true);
        }
        // 调试：--font-filter <词> 预置字体搜索词（验证过滤逻辑，省得手点输入框）
        if let Some(pos) = args.iter().position(|a| a == "--font-filter") {
            if let Some(word) = args.get(pos + 1) {
                let mut p = picker.borrow_mut();
                p.filter = word.clone();
                p.recompute();
                ui.set_font_filter(word.as_str().into());
                p.refresh(&ui);
            }
        }
    }

    // 命令行导入：--add <hip 或目录>
    let auto: Vec<PathBuf> = {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut v = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--add" && i + 1 < args.len() {
                v.push(PathBuf::from(&args[i + 1]));
                i += 2;
            } else {
                i += 1;
            }
        }
        v
    };
    if !auto.is_empty() {
        let mut a = app.borrow_mut();
        add_paths(&ui, &mut a, auto);
        flush_log(&ui, &mut a);
    }
    // 调试用：--set-version <下拉下标> 直接切换当前工程的 Houdini 版本
    if let Some(pos) = std::env::args().position(|a| a == "--set-version") {
        if let Some(sel) = std::env::args()
            .nth(pos + 1)
            .and_then(|s| s.parse::<i32>().ok())
        {
            let mut a = app.borrow_mut();
            set_project_version(&ui, &mut a, sel);
            flush_log(&ui, &mut a);
        }
    }
    if app.borrow().autorun && app.borrow().projects.iter().all(|p| !p.loading) {
        let mut a = app.borrow_mut();
        a.autorun = false;
        queue_everything(&mut a);
        a.engine.send(Cmd::Start);
        flush_log(&ui, &mut a);
    }

    // ── 回调接线 ──

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_add_files(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let picked = rfd::FileDialog::new()
                .add_filter("Houdini 工程", &["hip", "hipnc"])
                .set_title("选择 hip 文件（文件夹请直接拖进窗口）")
                .pick_files();
            if let Some(files) = picked {
                let mut a = app.borrow_mut();
                add_paths(&ui, &mut a, files);
                flush_log(&ui, &mut a);
            }
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_drop_transfer(move |data| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let paths: Vec<PathBuf> = data
                .file_paths()
                .map(|it| it.map(|p| p.to_path_buf()).collect())
                .unwrap_or_default();
            if paths.is_empty() {
                let mut a = app.borrow_mut();
                push_log(&mut a, format!("{}  拖入的内容里没有文件路径", pyproc::now_hms()));
                flush_log(&ui, &mut a);
                return;
            }
            let mut a = app.borrow_mut();
            add_paths(&ui, &mut a, paths);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_shot_selected(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_current_shot(i);
            let mut a = app.borrow_mut();
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_remove_project(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            remove_project(&ui, &mut a, i.max(0) as usize);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_node(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            toggle_node(&ui, &mut a, i as usize);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_select_all(move |on| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            select_all(&ui, &mut a, on);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        // 打字中：只记草稿 + 决定红框，绝不写库/重建/报错
        ui.on_range_edited(move |i, text| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            edit_range_draft(&ui, &mut a, i as usize, text.to_string());
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        // 回车 / 失焦：到这里才真正解析、写库、进队列
        ui.on_range_committed(move |i, text| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            commit_range(&ui, &mut a, i as usize, text.to_string());
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_reanalyze(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            reanalyze(&ui, &mut a);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_search_changed(move |t| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            a.search = t.to_string();
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_filter_changed(move |f| {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_filter_index(f);
            let mut a = app.borrow_mut();
            a.filter = f;
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_unadded(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            a.only_unadded = !a.only_unadded;
            let v = a.only_unadded;
            ui.set_only_unadded(v);
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_start_all(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            // 改了帧范围又直接点「开始」：先把草稿提交，再下发 Start
            flush_draft(&ui, &mut a);
            flush_log(&ui, &mut a);
            a.engine.send(Cmd::Start);
        });
    }
    {
        let app = app.clone();
        ui.on_pause_all(move || {
            let a = app.borrow();
            a.engine.send(Cmd::Pause);
        });
    }
    {
        let app = app.clone();
        ui.on_cancel_all(move || {
            let a = app.borrow();
            a.engine.send(Cmd::CancelAll);
        });
    }
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_finished(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            // 以引擎推回来的快照为准：把已结束（完成/失败/跳过/取消）的任务
            // 同时从本地任务表里摘掉，再整体下发给引擎。
            // 之前只摘本地列表、不发 SetJobs，引擎那边还留着这些任务，
            // 界面上看不出任何变化 —— 这就是「清空完成」点了没反应的原因。
            let finished: std::collections::HashSet<String> = a
                .views
                .iter()
                .filter(|v| JobStatus::parse(&v.status).is_finished())
                .map(|v| v.id.clone())
                .collect();
            let before = a.jobs.len();
            a.jobs.retain(|j| !finished.contains(&j.id));
            let removed = before - a.jobs.len();
            if removed > 0 {
                sync_jobs(&a);
                push_log(&mut a, format!("已清空 {removed} 个已结束的任务"));
            } else {
                push_log(&mut a, "执行区里没有已结束的任务".into());
            }
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_restart_all(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            if a.jobs.is_empty() {
                push_log(&mut a, "执行区是空的，没有可重跑的任务".into());
            } else {
                // 帧进度、重试次数由引擎一起清掉，清完立刻开始跑
                a.engine.send(Cmd::RestartAll);
            }
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_task_cancel(move |id| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            let id = id.to_string();
            let running = a
                .views
                .iter()
                .find(|v| v.id == id)
                .map(|v| v.status == JobStatus::Running.label())
                .unwrap_or(false);
            if running {
                a.engine.send(Cmd::CancelOne(id));
            } else {
                remove_job(&mut a, &id);
                sync_jobs(&a);
                rebuild_nodes(&ui, &mut a);
            }
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_task_up(move |id| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            let id = id.to_string();
            if let Some(i) = a.jobs.iter().position(|j| j.id == id) {
                if i > 0 {
                    a.jobs.swap(i, i - 1);
                    sync_jobs(&a);
                }
            }
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_task_down(move |id| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            let id = id.to_string();
            if let Some(i) = a.jobs.iter().position(|j| j.id == id) {
                if i + 1 < a.jobs.len() {
                    a.jobs.swap(i, i + 1);
                    sync_jobs(&a);
                }
            }
            rebuild_nodes(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_export_log(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let body = {
                let a = app.borrow();
                a.log.join("\r\n")
            };
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("houdini-render-queue.log")
                .add_filter("日志", &["log", "txt"])
                .save_file()
            {
                let ok = std::fs::write(&path, body).is_ok();
                let mut a = app.borrow_mut();
                push_log(
                    &mut a,
                    format!(
                        "{}  日志{}：{}",
                        pyproc::now_hms(),
                        if ok { "已导出" } else { "导出失败" },
                        path.display()
                    ),
                );
                rebuild_shots(&ui, &a);
                flush_log(&ui, &mut a);
            }
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_version_changed(move |sel| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            set_project_version(&ui, &mut a, sel);
            flush_log(&ui, &mut a);
        });
    }

    {
        let app = app.clone();
        ui.on_concurrency_changed(move |n| {
            let mut a = app.borrow_mut();
            // 界面传的是真实并发数（不是下拉下标），1/4/6/8 这类不连续的值才对得上
            a.concurrency = (n.max(0) as usize).max(1);
            a.engine.send(Cmd::SetConcurrency(a.concurrency));
        });
    }

    {
        let app = app.clone();
        ui.on_retry_changed(move |i| {
            let mut a = app.borrow_mut();
            a.retries = i.max(0) as u32;
            a.engine.send(Cmd::SetRetries(a.retries));
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_theme(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let dark = !ui.global::<Theme>().get_dark();
            apply_theme(&ui, dark);
            let a = app.borrow();
            a.ui_db.set_meta("theme", if dark { "dark" } else { "light" });
        });
    }

    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        let picker = picker.clone();
        ui.on_font_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let si = (ui.get_font_size_index().max(0) as usize).min(FONT_SCALES.len() - 1);
            let mut p = picker.borrow_mut();
            // 下拉里没选中项（过滤后为空）时沿用原来的字体，别把界面清成默认
            let real = usize::try_from(ui.get_font_family_index())
                .ok()
                .and_then(|i| p.shown.get(i).copied())
                .unwrap_or(p.current);
            p.current = real;
            apply_fonts(&ui, &p.all, real, si);
            let a = app.borrow();
            a.ui_db.set_meta("font_family_name", &p.all[real].id);
            a.ui_db.set_meta("font_family_index", &real.to_string());
            a.ui_db.set_meta("font_size_index", &si.to_string());
        });
    }

    {
        let ui_weak = ui.as_weak();
        let picker = picker.clone();
        ui.on_font_filter_changed(move |f| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut p = picker.borrow_mut();
            p.filter = f.to_string();
            p.recompute();
            p.refresh(&ui);
        });
    }

    // ── 事件泵 ──
    let timer = slint::Timer::default();
    {
        let app = app.clone();
        let ui_weak = ui.as_weak();
        let rx = evt_rx;
        let mut drop_tried = false;
        let mut drop_ready = false;
        // 双击计数器：坐标相同的两次双击也要各自触发一次刷新
        let mut dbl_tick: i32 = 0;
        // 自检：--drop-test <路径> 会在挂好接收器后自己投一次拖放
        let mut drop_test: Option<PathBuf> = std::env::args()
            .skip_while(|a| a != "--drop-test")
            .nth(1)
            .map(PathBuf::from);
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let mut a = app.borrow_mut();

                // 窗口建好后挂上接收器（HWND 在 show 之后才可用）。只尝试一次，失败就明确报出来
                if !drop_tried {
                    let hwnd = windrop::hwnd_of(&ui.window());
                    if hwnd != 0 {
                        drop_tried = true;
                        let rep = windrop::install(hwnd);
                        drop_ready = rep.ok();
                        // 窗口刚建好，补一次标题栏配色
                        windrop::set_dark_titlebar(hwnd, ui.global::<Theme>().get_dark());
                        // 大图标（任务栏/Alt+Tab）winit 不管，自己从 exe 资源补上
                        windrop::set_big_icon(hwnd);
                        if drop_ready {
                            push_log(
                                &mut a,
                                format!("{}  文件拖放已就绪 · {}", pyproc::now_hms(), rep.describe()),
                            );
                        } else {
                            push_log(
                                &mut a,
                                format!(
                                    "{}  文件拖放没装上 · {}（可用 + 按钮或命令行 --add 导入）",
                                    pyproc::now_hms(),
                                    rep.describe()
                                ),
                            );
                        }
                        if drop_ready {
                            if let Some(p) = drop_test.take() {
                                let ok = windrop::post_drop(hwnd, &[p.clone()]);
                                push_log(
                                    &mut a,
                                    format!(
                                        "{}  自检：模拟拖入 {} → {}",
                                        pyproc::now_hms(),
                                        p.display(),
                                        if ok { "已投递" } else { "投递失败" }
                                    ),
                                );
                            }
                        }
                    }
                }

                // 拖入悬停提示
                let hov = windrop::is_hovering();
                if hov != a.drag_hover {
                    a.drag_hover = hov;
                    ui.set_drag_hover(hov);
                }

                // 原生拖入的路径（Slint 的 DropArea 收不到系统拖放，见 windrop 模块说明）
                let dropped = windrop::take_dropped();
                if !dropped.is_empty() {
                    add_paths(&ui, &mut a, dropped);
                }

                // 双击输入框 → 整段选中。TextInput 原生双击只按词选（"1-240" 会被拆成两段），
                // 所以窗口过程自己数双击，这里把物理坐标折成逻辑像素再喂给界面
                if let Some((px, py)) = windrop::take_double_click() {
                    let s = ui.window().scale_factor();
                    let s = if s > 0.1 { s } else { 1.0 };
                    let fa = ui.global::<FieldActions>();
                    fa.set_x((px as f32 / s).round() as i32);
                    fa.set_y((py as f32 / s).round() as i32);
                    dbl_tick = dbl_tick.wrapping_add(1);
                    fa.set_tick(dbl_tick);
                }

                let mut queue_changed = false;
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        Event::Log(l) => push_log(&mut a, l),
                        Event::Queue(views) => {
                            let sig = signature(&views);
                            a.queued = views
                                .iter()
                                .map(|v| (v.id.clone(), JobStatus::parse(&v.status)))
                                .collect();
                            a.views = views;
                            queue_changed = true;
                            let node_state_changed = sig != a.last_sig;
                            a.last_sig = sig;
                            rebuild_queue(&ui, &a);
                            if node_state_changed {
                                // 行集合不变时逐行对齐就够了；几千个节点整表重建会卡住界面
                                if a.only_unadded {
                                    rebuild_nodes(&ui, &mut a);
                                } else {
                                    refresh_rows(&a);
                                }
                            }
                        }
                        Event::Analyzed { key, result, logs } => {
                            for l in logs {
                                push_log(&mut a, l);
                            }
                            if let Some(i) = a.projects.iter().position(|p| p.key == key) {
                                a.projects[i].loading = false;
                                let summary = match result {
                                    Ok(payload) => {
                                        let actual = payload.houdini.clone();
                                        // 自动模式以 hython 实际版本为准；用户手动指定过就保留选择
                                        if !a.projects[i].pinned {
                                            a.projects[i].houdini = actual.clone();
                                        }
                                        a.projects[i].global = payload.global;
                                        a.projects[i].nodes = payload.nodes;
                                        a.projects[i].error = None;
                                        let caches = a.projects[i]
                                            .nodes
                                            .iter()
                                            .filter(|n| n.cat() == Category::Cache)
                                            .count();
                                        let total = a.projects[i].nodes.len();
                                        let mut s = format!(
                                            "{}  解析完成：{} 个输出节点（缓存 {} / 渲染 {}）· Houdini {}",
                                            pyproc::now_hms(),
                                            total,
                                            caches,
                                            total - caches,
                                            a.projects[i].houdini
                                        );
                                        if a.projects[i].pinned
                                            && !actual.eq_ignore_ascii_case(&a.projects[i].houdini)
                                        {
                                            s.push_str(&format!("，hython 实际为 {actual}"));
                                        }
                                        s
                                    }
                                    Err(e) => {
                                        a.projects[i].error = Some(e.clone());
                                        format!("{}  解析失败：{}", pyproc::now_hms(), e)
                                    }
                                };
                                push_log(&mut a, summary);
                                let p = a.projects[i].clone();
                                let _ = a.ui_db.save_project(&p);
                            }
                            rebuild_shots(&ui, &a);
                            rebuild_nodes(&ui, &mut a);
                            if a.autorun && a.projects.iter().all(|p| !p.loading) {
                                a.autorun = false;
                                queue_everything(&mut a);
                                a.engine.send(Cmd::Start);
                            }
                        }
                    }
                }
                if queue_changed || a.log_dirty {
                    flush_log(&ui, &mut a);
                }
            },
        );
    }

    // 调试用：--click-node <行号> 直接触发一次勾选回调，绕开鼠标命中测试
    if let Some(pos) = std::env::args().position(|a| a == "--click-node") {
        if let Some(i) = std::env::args()
            .nth(pos + 1)
            .and_then(|s| s.parse::<i32>().ok())
        {
            let ui_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(1500), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.invoke_toggle_node(i);
                }
            });
        }
    }

    // 调试用：--debug-invoke clear-finished|restart-all 直接触发一次按钮回调，
    // 绕开鼠标命中测试，方便用快照做回归
    if let Some(pos) = std::env::args().position(|a| a == "--debug-invoke") {
        if let Some(name) = std::env::args().nth(pos + 1) {
            let ui_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(1500), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    match name.as_str() {
                        "clear-finished" => ui.invoke_clear_finished(),
                        "restart-all" => ui.invoke_restart_all(),
                        other => eprintln!("未知的调试回调: {other}"),
                    }
                }
            });
        }
    }

    // 快照验收：--snapshot <bmp 路径>，--snapshot-delay <毫秒> 可推迟（自动化点击测试用）
    if let Some(pos) = std::env::args().position(|a| a == "--snapshot") {
        if let Some(path) = std::env::args().nth(pos + 1) {
            let delay = std::env::args()
                .position(|a| a == "--snapshot-delay")
                .and_then(|i| std::env::args().nth(i + 1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(2600);
            let ui_weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(delay), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    match ui.window().take_snapshot() {
                        Ok(buf) => {
                            let p = PathBuf::from(&path);
                            match snap::save_bmp(&p, buf.as_bytes(), buf.width(), buf.height()) {
                                Ok(()) => eprintln!("快照已保存: {}", p.display()),
                                Err(e) => eprintln!("快照写入失败: {e}"),
                            }
                        }
                        Err(e) => eprintln!("快照失败: {e}"),
                    }
                }
            });
        }
    }

    // 窗口尺寸：恢复上次关闭时的尺寸（首次运行 = 1600x820）
    restore_win_size(&ui, &app.borrow());

    // 正常关闭时落库
    {
        let ui_weak = ui.as_weak();
        let app_close = app.clone();
        ui.window().on_close_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                save_win_size(&ui, &app_close.borrow());
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 兜底自动保存：拖完窗口被强杀（或崩溃）也不会丢尺寸。
    // 1 秒一轮、值变了才写库，开销可以忽略
    let size_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let app_size = app.clone();
        let mut last = (0u32, 0u32, false);
        size_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(1000),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let s = ui.window().size();
                let maxed = ui.window().is_maximized();
                if (s.width, s.height, maxed) != last {
                    last = (s.width, s.height, maxed);
                    save_win_size(&ui, &app_size.borrow());
                }
            },
        );
    }

    // 先建窗口，好让拖放接收器拿到 HWND
    ui.show()?;
    // 调试用：--probe-window 打印组件根元素的 layout-info。
    // 窗口「拖不动 / 没有最大化按钮」时先看这里：min == max 就说明根元素尺寸被写死了，
    // Slint 的 winit 后端会据此把窗口设成不可缩放（WS_THICKFRAME 都不会有）
    if std::env::args().any(|a| a == "--probe-window") {
        use slint::private_unstable_api::re_exports as re;
        let inner = re::WindowInner::from_pub(&ui.window());
        let comp = inner.component();
        let comp = re::vtable::VRc::borrow_pin(&comp);
        let h = comp.as_ref().layout_info(re::Orientation::Horizontal);
        let v = comp.as_ref().layout_info(re::Orientation::Vertical);
        eprintln!("H: {h:?}\nV: {v:?}\nsize={:?}", ui.window().size());
    }
    let code = ui.run();
    let _ = &timer;
    let _ = &size_timer;
    code
}

#[cfg(test)]
mod font_picker_tests {
    use super::*;

    fn fam(id: &str, label: &str) -> fonts::Family {
        fonts::Family {
            id: id.into(),
            label: label.into(),
        }
    }

    fn picker() -> FontPicker {
        FontPicker::new(vec![
            fam("", "系统默认"),
            fam("Microsoft YaHei", "微软雅黑"),
            fam("SimSun", "宋体"),
            fam("Noto Sans SC", "Noto Sans SC"),
        ])
    }

    /// 过滤同时认中文显示名和英文族名，英文不分大小写
    #[test]
    fn filter_matches_label_and_id() {
        let mut p = picker();
        p.filter = "宋".into();
        p.recompute();
        assert_eq!(p.shown, vec![2]);

        p.current = 3;
        p.filter = "noto".into();
        p.recompute();
        assert_eq!(p.shown, vec![3], "英文族名应该忽略大小写匹配");
        assert_eq!(p.cursor(), 0, "noto 只剩一项，当前字体正好是它");
        assert_eq!(p.current, 3, "过滤不该改掉已选字体");
    }

    /// 清空过滤词要能回到完整列表，且选中项回到原位
    #[test]
    fn clearing_filter_restores_selection() {
        let mut p = picker();
        p.current = 2;
        p.filter = "yahei".into();
        p.recompute();
        assert_eq!(p.cursor(), -1);
        p.filter = String::new();
        p.recompute();
        assert_eq!(p.shown.len(), 4);
        assert_eq!(p.cursor(), 2);
    }

    /// 匹配不到的过滤词给出空列表，而不是悄悄回到全量（否则用户以为过滤没生效）
    #[test]
    fn no_match_yields_empty_list() {
        let mut p = picker();
        p.filter = "zzz-no-such-font".into();
        p.recompute();
        assert!(p.shown.is_empty());
        assert_eq!(p.cursor(), -1);
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    /// 用户从 1001-1050 改到 1001-1060，路上必经的中间态都必须「不算错」，
    /// 否则打字打到一半就被打断（就是踩过的坑）
    #[test]
    fn typing_intermediate_states_are_not_errors() {
        for s in [
            "1001-", "1001-1", "1001-10", "1001-106", "", "10", "1001",
            // 不连续帧输入过程中的中间态
            "1052,", "1052,1055,", "1052,1055,1065-", "1052,1055,1065-1070",
            "1052，", "1052;;",
        ] {
            assert!(!range_draft_bad(s), "「{s}」不该在打字时标红");
        }
    }

    /// 真不像帧范围的才立刻标红
    #[test]
    fn garbage_is_flagged_while_typing() {
        for s in [
            "1001-1a", "1.5", "abc", "1-2-3", "1001-1060帧", "1052,1065-1070-1080",
            "1052,abc",
        ] {
            assert!(range_draft_bad(s), "「{s}」应该标红");
        }
    }

    #[test]
    fn commit_parses_full_range() {
        assert_eq!(parse_frames("1001-1060").unwrap().label(), "1001-1060");
        assert_eq!(parse_frames(" 1001 - 1060 ").unwrap().label(), "1001-1060");
        assert_eq!(parse_frames("1001~1060").unwrap().label(), "1001-1060");
        assert_eq!(parse_frames("1001–1060").unwrap().label(), "1001-1060");
        assert_eq!(parse_frames("1050").unwrap().label(), "1050");
    }

    /// 核心需求：不连续的帧
    #[test]
    fn commit_parses_discrete_frames() {
        let f = parse_frames("1052,1055,1056,1065-1070").unwrap();
        assert_eq!(f.len(), 9);
        assert_eq!(f.label(), "1052,1055-1056,1065-1070");
        assert_eq!(
            f.expand(),
            vec![1052, 1055, 1056, 1065, 1066, 1067, 1068, 1069, 1070]
        );
    }

    /// 没打完才提交（用户输完就点了别处）要明确报错，而不是悄悄当成 1001-106
    #[test]
    fn commit_rejects_half_typed_and_reversed() {
        assert_eq!(parse_frames("1001-"), Err(FrameErr::Format));
        assert_eq!(parse_frames("1001-106"), Err(FrameErr::Reversed(1001, 106)));
        assert_eq!(parse_frames("1052,1070-1065"), Err(FrameErr::Reversed(1070, 1065)));
        assert_eq!(parse_frames(""), Err(FrameErr::Format));
        assert_eq!(parse_frames("-"), Err(FrameErr::Format));
        assert_eq!(parse_frames("1052,"), Ok(FrameSet::single(1052)));
    }
}

#[cfg(test)]
mod concurrency_tests {
    /// 取 `名字: [ ... ]` 方括号里的内容（第一次出现的位置就是声明处）
    fn array_after<'a>(src: &'a str, name: &str) -> &'a str {
        let at = src.find(name).unwrap_or_else(|| panic!("ui/app.slint 里找不到 {name}"));
        let rest = &src[at..];
        let lb = rest.find('[').expect("选项没写在方括号里");
        let rb = rest.find(']').expect("方括号没闭合");
        &rest[lb + 1..rb]
    }

    /// 并发下拉的文案和真实并发值必须一一对应。
    /// 文案是 1/2/3/4/6/8 这种不连续整数，一旦有人再拿「下标 + 1」当并发数，
    /// 就会出现选「6」实际跑 5 的错位——而编译期完全发现不了，只能靠这条守着。
    #[test]
    fn concurrency_labels_match_values() {
        let src = include_str!("../ui/app.slint");

        let labels: Vec<usize> = array_after(src, "concurrency-options")
            .split(',')
            .filter_map(|s| s.trim().trim_matches('"').parse().ok())
            .collect();
        let values: Vec<usize> = array_after(src, "concurrency-values")
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        assert!(!values.is_empty(), "没解析到 concurrency-values");
        assert_eq!(
            labels, values,
            "并发下拉的文案与真实值对不上，下拉下标不能直接当并发数用"
        );
    }
}

#[cfg(test)]
mod win_size_tests {
    use super::*;

    #[test]
    fn keeps_normal_size() {
        assert_eq!(clamp_win_size(1600.0, 820.0, 2560.0, 1408.0), (1600.0, 820.0));
    }

    #[test]
    fn clamps_tiny_size_up_to_minimum() {
        let (w, h) = clamp_win_size(200.0, 120.0, 2560.0, 1408.0);
        assert_eq!((w, h), MIN_WIN_SIZE);
    }

    #[test]
    fn clamps_huge_size_to_work_area() {
        // 上次在 4K 上关掉、这次 1080p 打开：不能开出比屏幕还大的窗口
        let (w, h) = clamp_win_size(3800.0, 2100.0, 1920.0, 1040.0);
        assert_eq!((w, h), (1920.0, 1040.0));
    }

    #[test]
    fn screen_smaller_than_minimum_does_not_invert() {
        // 极小屏（比如远程桌面）上工作区比最小尺寸还小，也不能让 clamp 反过来变成
        // 「下限 > 上限」而 panic
        let (w, h) = clamp_win_size(1600.0, 820.0, 800.0, 600.0);
        assert!(w >= 320.0 && h >= 240.0, "{w}x{h} 不该小于硬下限");
    }
}
