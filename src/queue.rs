use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::db::Db;
use crate::model::*;
use crate::pyproc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Idle,
    Running,
    Paused,
}

pub enum Cmd {
    SetJobs(Vec<JobSpec>),
    Start,
    Pause,
    CancelAll,
    CancelOne(String),
    /// 全部任务清零重跑：帧进度、重试次数一并清掉，然后立刻开始
    RestartAll,
    SetConcurrency(usize),
    SetRetries(u32),
    Shutdown,
}

enum WResult {
    Ok,
    Preflight(String),
    Fail(String),
    Canceled,
}

enum WMsg {
    Frame(String, i32),
    Done(String, WResult),
}

struct Rt {
    spec: JobSpec,
    status: JobStatus,
    attempts: u32,
    done: BTreeSet<i32>,
    cancel: Arc<AtomicBool>,
    running: bool,
    message: String,
    /// 已从执行区移除但还在跑：结束后直接从队列和库里清掉
    detached: bool,
}

pub struct Engine {
    tx: Sender<Cmd>,
}

impl Engine {
    pub fn spawn(evt: Sender<Event>, db: Db) -> Engine {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || run(rx, evt, db));
        Engine { tx }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

// ───────────────────────── 主循环 ─────────────────────────

fn run(rx: Receiver<Cmd>, evt: Sender<Event>, db: Db) {
    let mut rt: Vec<Rt> = Vec::new();
    let mut done_cache = db.load_done_frames();
    for (spec, status, attempts) in db.load_jobs() {
        // 上次退出时正在跑的，重启后回到排队（进程已经没了）
        let status = if status == JobStatus::Running {
            JobStatus::Queued
        } else {
            status
        };
        let done = done_cache.remove(&spec.id).unwrap_or_default();
        rt.push(Rt {
            spec,
            status,
            attempts,
            done,
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
            message: String::new(),
            detached: false,
        });
    }

    let (wtx, wrx) = mpsc::channel::<WMsg>();
    let mut state = RunState::Idle;
    let mut concurrency: usize = 1;
    let mut max_retries: u32 = 2;

    let _ = evt.send(Event::Queue(snapshot(&rt)));

    // 上一轮 recv_timeout 抢到的指令，留到这一轮开头处理
    let mut pending_cmd: Option<Cmd> = None;
    loop {
        // ── 指令 ──
        loop {
            let cmd = match pending_cmd.take() {
                Some(c) => c,
                None => match rx.try_recv() {
                    Ok(c) => c,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                },
            };
            match cmd {
                Cmd::Shutdown => return,
                Cmd::SetJobs(specs) => {
                    let mut old: HashMap<String, Rt> =
                        rt.drain(..).map(|r| (r.spec.id.clone(), r)).collect();
                    let mut next: Vec<Rt> = Vec::with_capacity(specs.len());
                    for spec in specs {
                        if let Some(mut r) = old.remove(&spec.id) {
                            r.spec = spec;
                            next.push(r);
                        } else {
                            let done = done_cache.remove(&spec.id).unwrap_or_default();
                            next.push(Rt {
                                spec,
                                status: JobStatus::Queued,
                                attempts: 0,
                                done,
                                cancel: Arc::new(AtomicBool::new(false)),
                                running: false,
                                message: String::new(),
                                detached: false,
                            });
                        }
                    }
                    // 被移出执行区的任务：还在跑的要求它停下（跑完当前帧），
                    // 结束后由 detached 标记负责清理；其它直接删库
                    for (id, mut r) in old {
                        if r.running {
                            r.detached = true;
                            r.cancel.store(true, Ordering::SeqCst);
                            next.push(r);
                        } else {
                            db.delete_job(&id);
                        }
                    }
                    rt = next;
                    // 一次事务写完，任务多的时候不至于卡在逐条提交上
                    let batch: Vec<(&JobSpec, i64, JobStatus, u32)> = rt
                        .iter()
                        .enumerate()
                        .map(|(i, r)| (&r.spec, i as i64, r.status, r.attempts))
                        .collect();
                    db.save_jobs(&batch);
                    // 增删任务也算状态变化，必须回传，否则界面勾选状态永远不变
                    let _ = evt.send(Event::Queue(snapshot(&rt)));
                }
                Cmd::Start => {
                    for r in rt.iter_mut() {
                        if matches!(
                            r.status,
                            JobStatus::Canceled | JobStatus::Failed | JobStatus::Skipped
                        ) {
                            r.status = JobStatus::Queued;
                            r.attempts = 0;
                            r.message.clear();
                        }
                    }
                    state = RunState::Running;
                    let pending = rt.iter().filter(|r| r.status == JobStatus::Queued).count();
                    log(&evt, format!("开始执行，待跑 {} 个任务，并发 {}", pending, concurrency));
                }
                Cmd::Pause => {
                    state = RunState::Paused;
                    log(&evt, "已暂停：不再启动新任务，正在跑的任务会继续".into());
                }
                Cmd::CancelAll => {
                    state = RunState::Idle;
                    for r in rt.iter_mut() {
                        if r.running {
                            r.cancel.store(true, Ordering::SeqCst);
                        } else if r.status == JobStatus::Queued {
                            r.status = JobStatus::Canceled;
                            db.save_job(&r.spec, 0, r.status, r.attempts);
                        }
                    }
                    log(&evt, "已取消队列（正在跑的任务会在当前帧结束后停止）".into());
                }
                Cmd::CancelOne(id) => {
                    if let Some(r) = rt.iter_mut().find(|r| r.spec.id == id) {
                        if r.running {
                            r.cancel.store(true, Ordering::SeqCst);
                        } else {
                            r.status = JobStatus::Canceled;
                            db.save_job(&r.spec, 0, r.status, r.attempts);
                        }
                    }
                }
                Cmd::RestartAll => {
                    if rt.is_empty() {
                        log(&evt, "执行区是空的，没有可重跑的任务".into());
                    } else {
                        let mut reset = 0usize;
                        for r in rt.iter_mut() {
                            // 正在跑的不动：硬重置会把已经渲到一半的帧丢掉
                            if r.running {
                                continue;
                            }
                            r.status = JobStatus::Queued;
                            r.attempts = 0;
                            r.message.clear();
                            r.done.clear();
                            db.clear_frames(&r.spec.id);
                            reset += 1;
                        }
                        if reset > 0 {
                            let batch: Vec<(&JobSpec, i64, JobStatus, u32)> = rt
                                .iter()
                                .enumerate()
                                .map(|(i, r)| (&r.spec, i as i64, r.status, r.attempts))
                                .collect();
                            db.save_jobs(&batch);
                        }
                        state = RunState::Running;
                        if reset > 0 {
                            log(
                                &evt,
                                format!("重新开始：{reset} 个任务已清零进度，回到排队"),
                            );
                        } else {
                            log(&evt, "重新开始：任务都在运行中，这里没有可重置的任务".into());
                        }
                        let _ = evt.send(Event::Queue(snapshot(&rt)));
                    }
                }
                Cmd::SetConcurrency(n) => concurrency = n.max(1),
                Cmd::SetRetries(n) => max_retries = n,
            }
        }

        // ── 工作线程回报 ──
        let mut changed = false;
        loop {
            match wrx.try_recv() {
                Ok(WMsg::Frame(id, f)) => {
                    if let Some(r) = rt.iter_mut().find(|r| r.spec.id == id) {
                        if r.done.insert(f) {
                            db.mark_frame(&id, f);
                            changed = true;
                        }
                    }
                }
                Ok(WMsg::Done(id, res)) => {
                    if let Some(r) = rt.iter_mut().find(|r| r.spec.id == id) {
                        r.running = false;
                        if r.detached {
                            // 已从执行区移除，结果不再关心：不进重试、不留卡片
                            r.status = JobStatus::Canceled;
                            r.message.clear();
                        } else {
                            match res {
                                WResult::Ok => {
                                    r.status = JobStatus::Success;
                                    r.message.clear();
                                    log(
                                        &evt,
                                        format!("[{}] 完成（{} 帧）", r.spec.title(), r.done.len()),
                                    );
                                }
                                WResult::Preflight(m) => {
                                    r.status = JobStatus::Skipped;
                                    r.message = m.clone();
                                    log(&evt, format!("[{}] 已跳过：{}", r.spec.title(), m));
                                }
                                WResult::Canceled => {
                                    r.status = JobStatus::Canceled;
                                    log(&evt, format!("[{}] 已取消", r.spec.title()));
                                }
                                WResult::Fail(m) => {
                                    r.attempts += 1;
                                    if r.attempts <= max_retries {
                                        r.status = JobStatus::Queued;
                                        r.message = format!("第 {} 次重试", r.attempts);
                                        log(
                                            &evt,
                                            format!(
                                                "[{}] 失败，准备第 {} 次重试：{}",
                                                r.spec.title(),
                                                r.attempts,
                                                m
                                            ),
                                        );
                                    } else {
                                        r.status = JobStatus::Failed;
                                        r.message = m.clone();
                                        log(&evt, format!("[{}] 失败：{}", r.spec.title(), m));
                                    }
                                }
                            }
                            db.save_job(&r.spec, 0, r.status, r.attempts);
                        }
                    }
                    // 被移除且已经停下的任务，从队列和库里一起清掉
                    rt.retain(|r| {
                        let drop = r.detached && !r.running;
                        if drop {
                            db.delete_job(&r.spec.id);
                        }
                        !drop
                    });
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // ── 调度 ──
        if state == RunState::Running {
            loop {
                let active = rt.iter().filter(|r| r.running).count();
                if active >= concurrency {
                    break;
                }
                let Some(idx) = rt
                    .iter()
                    .position(|r| r.status == JobStatus::Queued && !r.running)
                else {
                    break;
                };

                if !rt[idx].spec.hip.exists() {
                    rt[idx].status = JobStatus::Failed;
                    rt[idx].message = "工程文件不存在".into();
                    log(
                        &evt,
                        format!(
                            "[{}] 工程文件不存在：{}",
                            rt[idx].spec.title(),
                            rt[idx].spec.hip.display()
                        ),
                    );
                    changed = true;
                    continue;
                }

                let frames: Vec<i32> = rt[idx]
                    .spec
                    .frames
                    .expand()
                    .into_iter()
                    .filter(|f| !rt[idx].done.contains(f))
                    .collect();

                if frames.is_empty() {
                    rt[idx].status = JobStatus::Success;
                    changed = true;
                    continue;
                }

                rt[idx].running = true;
                rt[idx].status = JobStatus::Running;
                rt[idx].cancel = Arc::new(AtomicBool::new(false));

                let spec = rt[idx].spec.clone();
                let cancel = rt[idx].cancel.clone();
                let tx = wtx.clone();
                let evt2 = evt.clone();
                log(
                    &evt,
                    format!(
                        "[{}] 开始：{} 帧（{}）· {}",
                        spec.title(),
                        frames.len(),
                        spec.frames.display(44),
                        spec.houdini
                    ),
                );
                thread::spawn(move || run_worker(spec, frames, cancel, tx, evt2));
                changed = true;
            }

            let active = rt.iter().filter(|r| r.running).count();
            let pending = rt.iter().filter(|r| r.status == JobStatus::Queued).count();
            if active == 0 && pending == 0 {
                state = RunState::Idle;
                log(&evt, "队列已全部结束".into());
                changed = true;
            }
        }

        if changed {
            let _ = evt.send(Event::Queue(snapshot(&rt)));
        }

        // 没有指令时才等：勾选/增删任务能立刻处理，界面不用干等 120ms
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(c) => pending_cmd = Some(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn log(evt: &Sender<Event>, msg: String) {
    let _ = evt.send(Event::Log(format!("{}  {}", pyproc::now_hms(), msg)));
}

fn snapshot(rt: &[Rt]) -> Vec<JobView> {
    rt.iter()
        .map(|r| {
            // 帧数按集合算：不连续帧的 1052,1055,1056 是 3 帧，不是 1056-1052+1
            let total = r.spec.frames.len().max(1);
            let done = r.spec.frames.count_done(&r.done) as i32;
            let progress = if total > 0 {
                ((done as f32 / total as f32) * 100.0) as i32
            } else {
                0
            };
            let frames = if !r.message.is_empty() {
                r.message.clone()
            } else if done > 0 || !r.status.is_finished() {
                // 排队时也把总帧数报出来：不连续帧光看范围数不出帧数
                format!("{} / {} 帧", done, total)
            } else {
                String::new()
            };
            JobView {
                id: r.spec.id.clone(),
                key: r.spec.project_key.clone(),
                node_path: r.spec.node_path.clone(),
                title: r.spec.title(),
                shot: r.spec.shot.clone(),
                category: r.spec.category.label().into(),
                range: r.spec.frames.display(30),
                status: r.status.label().into(),
                progress,
                frames,
                tone: r.status.tone(),
                message: r.message.clone(),
            }
        })
        .collect()
}

// ───────────────────────── 单个任务的执行 ─────────────────────────

fn run_worker(
    spec: JobSpec,
    frames: Vec<i32>,
    cancel: Arc<AtomicBool>,
    tx: Sender<WMsg>,
    evt: Sender<Event>,
) {
    let id = spec.id.clone();
    let title = spec.title();
    let dir = std::env::temp_dir().join("HoudiniRenderQueue").join("plans");
    let _ = std::fs::create_dir_all(&dir);
    let safe: String = id
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c == '*' || c == '?' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let plan = dir.join(format!("{safe}.plan.json"));
    let cancel_file = dir.join(format!("{safe}.cancel"));
    let _ = std::fs::remove_file(&cancel_file);

    let plan_json = serde_json::json!({ "frames": frames });
    if let Err(e) = std::fs::write(&plan, plan_json.to_string()) {
        let _ = tx.send(WMsg::Done(id, WResult::Fail(format!("写入帧计划失败: {e}"))));
        return;
    }

    let script = pyproc::script_path("render.py");
    if !script.exists() {
        let _ = tx.send(WMsg::Done(
            id,
            WResult::Fail(format!("找不到 render.py: {}", script.display())),
        ));
        return;
    }

    let mut cmd = pyproc::hidden(&spec.hython);
    cmd.arg(&script)
        .arg(&spec.hip)
        .arg("--node")
        .arg(&spec.node_path)
        .arg("--plan")
        .arg(&plan)
        .arg("--cancel")
        .arg(&cancel_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(WMsg::Done(
                id,
                WResult::Fail(format!("启动 hython 失败: {e}（{}）", spec.hython.display())),
            ));
            return;
        }
    };

    let (ltx, lrx) = mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        let l = ltx.clone();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = l.send(line);
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let l = ltx.clone();
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = l.send(line);
            }
        });
    }

    let start = Instant::now();
    let mut ready = false;
    let mut result: Option<WResult> = None;
    let mut cancel_at: Option<Instant> = None;
    let mut killed = false;

    loop {
        while let Ok(line) = lrx.try_recv() {
            let l = line.trim_end();
            if l.is_empty() {
                continue;
            }
            if let Some(rest) = l.strip_prefix("##") {
                let (tag, arg) = match rest.split_once(' ') {
                    Some((t, a)) => (t, a.trim()),
                    None => (rest, ""),
                };
                match tag {
                    "BOOT" => {}
                    "READY" => {
                        ready = true;
                        let _ = evt.send(Event::Log(format!(
                            "{}  [{}] 工程加载完成",
                            pyproc::now_hms(),
                            title
                        )));
                    }
                    "FRAME" => {}
                    "DONE" => {
                        if let Ok(f) = arg.parse::<i32>() {
                            let _ = tx.send(WMsg::Frame(id.clone(), f));
                            let _ = evt.send(Event::Log(format!(
                                "{}  [{}] 帧 {} 完成",
                                pyproc::now_hms(),
                                title,
                                f
                            )));
                        }
                    }
                    "NOTE" => {
                        let _ = evt.send(Event::Log(format!(
                            "{}  [{}] {}",
                            pyproc::now_hms(),
                            title,
                            arg
                        )));
                    }
                    "PREFLIGHT" => {
                        result = Some(WResult::Preflight(arg.to_string()));
                    }
                    "FAIL" => {
                        result = Some(WResult::Fail(arg.to_string()));
                    }
                    "CANCELED" => {
                        result = Some(WResult::Canceled);
                    }
                    "FINISH" => {}
                    _ => {
                        let _ = evt.send(Event::Log(format!(
                            "{}  [{}] {}",
                            pyproc::now_hms(),
                            title,
                            l
                        )));
                    }
                }
            } else {
                let _ = evt.send(Event::Log(format!(
                    "{}  [{}] {}",
                    pyproc::now_hms(),
                    title,
                    l
                )));
            }
        }

        let exited = match child.try_wait() {
            Ok(Some(status)) => {
                if result.is_none() {
                    result = Some(if status.success() {
                        WResult::Ok
                    } else {
                        WResult::Fail(format!("hython 退出码 {:?}", status.code()))
                    });
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                result = Some(WResult::Fail(format!("等待进程失败: {e}")));
                let _ = child.kill();
                true
            }
        };
        if exited {
            break;
        }

        if cancel.load(Ordering::SeqCst) && cancel_at.is_none() {
            let _ = std::fs::write(&cancel_file, b"1");
            cancel_at = Some(Instant::now());
            let _ = evt.send(Event::Log(format!(
                "{}  [{}] 收到取消请求，等当前帧结束后停止…",
                pyproc::now_hms(),
                title
            )));
        }
        if let Some(t) = cancel_at {
            if t.elapsed() > Duration::from_secs(20) && !killed {
                let _ = child.kill();
                killed = true;
                result = Some(WResult::Canceled);
            }
        }
        if !ready && result.is_none() && start.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            result = Some(WResult::Fail(
                "启动超时 5 分钟（可能是 Houdini 许可证被占用，或工程过大）".into(),
            ));
        }

        thread::sleep(Duration::from_millis(120));
    }

    let _ = std::fs::remove_file(&plan);
    let _ = std::fs::remove_file(&cancel_file);
    let _ = tx.send(WMsg::Done(
        id,
        result.unwrap_or(WResult::Fail("未知结束状态".into())),
    ));
}
