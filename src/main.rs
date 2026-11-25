// release 构建使用 Windows 图形子系统，双击运行不再弹出黑色控制台窗口；
// debug 构建仍保留控制台，方便查看 eprintln 调试日志。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

slint::include_modules!();

mod analyze;
mod db;
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
    /// 节点帧范围覆盖：job_id -> (起, 止)
    ranges: HashMap<String, (i32, i32)>,
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
    let (a, b) = app
        .ranges
        .get(&id)
        .copied()
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
        range: format!("{a}-{b}").into(),
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

fn default_range(p: &Project, n: &NodeInfo) -> (i32, i32) {
    let a = n.fstart.unwrap_or(p.global.0);
    let b = n.fend.unwrap_or(p.global.1);
    if b < a { (a, a) } else { (a, b) }
}

fn make_spec(app: &App, pi: usize, ni: usize) -> Option<JobSpec> {
    let p = app.projects.get(pi)?;
    let n = p.nodes.get(ni)?;
    let id = job_id(&p.key, &n.path);
    let (a, b) = app
        .ranges
        .get(&id)
        .copied()
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
        fstart: a,
        fend: b,
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

fn edit_range(ui: &AppWindow, app: &mut App, idx: usize, text: String) {
    let Some(&(pi, ni)) = app.visible.get(idx) else {
        return;
    };
    let (pkey, npath, defr) = {
        let Some(p) = app.projects.get(pi) else { return };
        let Some(n) = p.nodes.get(ni) else { return };
        (p.key.clone(), n.path.clone(), default_range(p, n))
    };
    let id = job_id(&pkey, &npath);
    let Some((a, b)) = parse_range(&text) else {
        push_log(app, format!("{}  帧范围格式不对：「{}」，用 1-120 这种写法", pyproc::now_hms(), text));
        rebuild_nodes(ui, app);
        return;
    };
    if (a, b) == defr {
        app.ranges.remove(&id);
    } else {
        app.ranges.insert(id.clone(), (a, b));
    }
    if let Some(j) = app.jobs.iter_mut().find(|j| j.id == id) {
        j.fstart = a;
        j.fend = b;
        sync_jobs(app);
    }
    save_ranges(app);
    // 生效后只刷这一行（输入框里的文字不用被模型覆盖回去）
    if app.only_unadded {
        rebuild_nodes(ui, app);
    } else {
        refresh_row(app, idx);
    }
}

fn parse_range(s: &str) -> Option<(i32, i32)> {
    let t: String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '–' | '~' | '至' => '-',
            c => c,
        })
        .collect();
    if t.is_empty() {
        return None;
    }
    if let Some((x, y)) = t.split_once('-') {
        let a: i32 = x.parse().ok()?;
        let b: i32 = y.parse().ok()?;
        if b < a {
            return None;
        }
        return Some((a, b));
    }
    let a: i32 = t.parse().ok()?;
    Some((a, a))
}

fn save_ranges(app: &App) {
    let json = serde_json::to_string(&app.ranges).unwrap_or_else(|_| "{}".into());
    app.ui_db.set_meta("ranges", &json);
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

/// 字体设置：第 0 项是「系统默认」（空串交给字体后端决定）
const FONT_FAMILIES: &[&str] = &[
    "",
    "Microsoft YaHei",
    "Microsoft YaHei UI",
    "DengXian",
    "SimHei",
    "SimSun",
    "Consolas",
];
/// 下拉框显示名（第 0 项不能直接显示空串）
const FONT_FAMILY_LABELS: &[&str] = &[
    "系统默认",
    "微软雅黑",
    "微软雅黑 UI",
    "等线",
    "黑体",
    "宋体",
    "Consolas",
];
const FONT_SIZE_LABELS: &[&str] = &["紧凑 100%", "标准 115%", "舒适 130%", "大号 145%"];
const FONT_SCALES: &[f32] = &[1.0, 1.15, 1.3, 1.45];

/// 字体落地：正文字体、等宽字体、字号倍率。所有字号在 UI 里都过 Fonts.size()。
fn apply_fonts(ui: &AppWindow, family_idx: usize, size_idx: usize) {
    let g = ui.global::<Fonts>();
    let fi = family_idx.min(FONT_FAMILIES.len() - 1);
    let si = size_idx.min(FONT_SCALES.len() - 1);
    g.set_body(FONT_FAMILIES[fi].into());
    g.set_mono("Consolas".into());
    g.set_scale(FONT_SCALES[si]);
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

    let ui = AppWindow::new()?;

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
    let ranges: HashMap<String, (i32, i32)> = ui_db
        .get_meta("ranges")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

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
            (read("font_family_index", 0), read("font_size_index", 1))
        };
        let fam: Vec<slint::SharedString> = FONT_FAMILY_LABELS.iter().map(|s| (*s).into()).collect();
        let sizes: Vec<slint::SharedString> = FONT_SIZE_LABELS.iter().map(|s| (*s).into()).collect();
        ui.set_font_families(ModelRc::new(VecModel::from(fam)));
        ui.set_font_size_labels(ModelRc::new(VecModel::from(sizes)));
        ui.set_font_family_index(fi as i32);
        ui.set_font_size_index(si as i32);
        apply_fonts(&ui, fi, si);
        // 调试：--set-font <字体序号> <字号序号> 直接切换并落库
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--set-font") {
            let nf = args
                .get(pos + 1)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(fi);
            let ns = args
                .get(pos + 2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(si);
            ui.set_font_family_index(nf as i32);
            ui.set_font_size_index(ns as i32);
            apply_fonts(&ui, nf, ns);
            let a = app.borrow();
            a.ui_db.set_meta("font_family_index", &nf.to_string());
            a.ui_db.set_meta("font_size_index", &ns.to_string());
        }
        if std::env::args().any(|a| a == "--font-panel") {
            ui.set_font_panel_open(true);
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
        ui.on_range_edited(move |i, text| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut a = app.borrow_mut();
            edit_range(&ui, &mut a, i as usize, text.to_string());
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
        ui.on_start_all(move || {
            let a = app.borrow();
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
        ui.on_concurrency_changed(move |i| {
            let mut a = app.borrow_mut();
            a.concurrency = (i.max(0) as usize) + 1;
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
        ui.on_font_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let fi = ui.get_font_family_index().max(0) as usize;
            let si = ui.get_font_size_index().max(0) as usize;
            apply_fonts(&ui, fi, si);
            let a = app.borrow();
            a.ui_db.set_meta("font_family_index", &fi.to_string());
            a.ui_db.set_meta("font_size_index", &si.to_string());
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

    // 先建窗口，好让拖放接收器拿到 HWND
    ui.show()?;
    let code = ui.run();
    let _ = &timer;
    code
}
