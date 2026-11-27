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
use crate::remote::{AssignReq, DoneKind, Hub, HubEvent};

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
    /// 主界面起好监听后把 Hub 交给引擎（之后才可能出现远端块）
    AttachHub(Arc<Hub>, Receiver<HubEvent>),
    /// 设备面板打开时主动要一份快照
    RefreshWorkers,
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
    /// 已派给远端 worker、还没回报的帧（调度本地时要排掉）
    remote: BTreeSet<i32>,
    /// 已经交给本地线程、但还没完成的帧。不排掉的话远端会把同一帧再派一遍，
    /// 两个进程同时写同一帧的输出。
    local: BTreeSet<i32>,
    /// 在跑的远端块 → 该块的帧，块结束时整块回滚或确认
    blocks: HashMap<String, Vec<i32>>,
    /// 已经为这个任务做过一次「对半分」。只在第一次生效：否则本地会
    /// 100→50→25→13→6 一路对半切下去，每段都要重开一次 hython，加载开销爆炸。
    paced: bool,
}

impl Rt {
    fn new(spec: JobSpec, status: JobStatus, attempts: u32, done: BTreeSet<i32>) -> Rt {
        Rt {
            spec,
            status,
            attempts,
            done,
            cancel: Arc::new(AtomicBool::new(false)),
            running: false,
            message: String::new(),
            detached: false,
            remote: BTreeSet::new(),
            local: BTreeSet::new(),
            blocks: HashMap::new(),
            paced: false,
        }
    }

    /// 还没人认领的帧（没完成、没派给远端、也没交给本地线程）。
    fn free_frames(&self) -> Vec<i32> {
        self.spec
            .frames
            .expand()
            .into_iter()
            .filter(|f| {
                !self.done.contains(f) && !self.remote.contains(f) && !self.local.contains(f)
            })
            .collect()
    }

    /// 全部帧的完成情况：done 覆盖了帧集合就算收工。
    fn all_done(&self) -> bool {
        let all = self.spec.frames.expand();
        !all.is_empty() && all.iter().all(|f| self.done.contains(f))
    }
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

    /// 取一个可以跨线程/跨闭包持有的发送端（定时器里发指令用）。
    pub fn sender(&self) -> Sender<Cmd> {
        self.tx.clone()
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
        rt.push(Rt::new(spec, status, attempts, done));
    }

    let (wtx, wrx) = mpsc::channel::<WMsg>();
    let mut state = RunState::Idle;
    let mut concurrency: usize = 1;
    let mut max_retries: u32 = 2;
    // 没挂 Hub 时整条多机路径不执行，单机行为与以前完全一致
    let mut hub: Option<Arc<Hub>> = None;
    let mut hub_rx: Option<Receiver<HubEvent>> = None;

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
                            next.push(Rt::new(spec, JobStatus::Queued, 0, done));
                        }
                    }
                    // 被移出执行区的任务：还在跑的（本地线程或远端块）要求它停下，
                    // 结束后由 detached 标记负责清理；其它直接删库
                    for (id, mut r) in old {
                        if r.running || !r.blocks.is_empty() {
                            r.detached = true;
                            r.cancel.store(true, Ordering::SeqCst);
                            if let Some(h) = &hub {
                                for chunk in r.blocks.keys() {
                                    h.cancel(&id, chunk);
                                }
                            }
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
                        } else if !r.blocks.is_empty() {
                            // 帧已经派出去了，本地没有线程可控：通知 worker 收工
                            if let Some(h) = &hub {
                                for chunk in r.blocks.keys() {
                                    h.cancel(&r.spec.id, chunk);
                                }
                            }
                            r.status = JobStatus::Canceled;
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
                        } else if !r.blocks.is_empty() {
                            if let Some(h) = &hub {
                                for chunk in r.blocks.keys() {
                                    h.cancel(&id, chunk);
                                }
                            }
                            r.status = JobStatus::Canceled;
                            db.save_job(&r.spec, 0, r.status, r.attempts);
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
                            r.paced = false;
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
                Cmd::AttachHub(h, hrx) => {
                    let n = h.online_count();
                    let _ = evt.send(Event::Workers(h.views()));
                    hub = Some(h);
                    hub_rx = Some(hrx);
                    log(
                        &evt,
                        format!("多机执行已就绪（当前在线 {n} 台），可分片任务会按帧块派发"),
                    );
                }
                Cmd::RefreshWorkers => {
                    if let Some(h) = &hub {
                        let _ = evt.send(Event::Workers(h.views()));
                    }
                }
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
                        // 本地线程退出：它认领过的帧全部释放（没跑完的回到可调度池）
                        r.local.clear();
                        if r.detached {
                            // 已从执行区移除，结果不再关心：不进重试、不留卡片
                            r.status = JobStatus::Canceled;
                            r.message.clear();
                        } else {
                            match res {
                                WResult::Ok => {
                                    // 本地可能只跑了一部分帧（其余在远端 worker 上），
                                    // 还有活没完就别提前判成功
                                    if r.all_done() {
                                        r.status = JobStatus::Success;
                                        r.message.clear();
                                        log(
                                            &evt,
                                            format!(
                                                "[{}] 完成（{} 帧）",
                                                r.spec.title(),
                                                r.done.len()
                                            ),
                                        );
                                    } else {
                                        r.status = if r.blocks.is_empty() {
                                            JobStatus::Queued
                                        } else {
                                            JobStatus::Running
                                        };
                                        r.message.clear();
                                    }
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

        // ── 远端 worker 事件 ──
        if let Some(hrx) = &hub_rx {
            while let Ok(ev) = hrx.try_recv() {
                match ev {
                    HubEvent::Log(l) => log(&evt, l),
                    HubEvent::Frame { job, chunk: _, frame } => {
                        if let Some(r) = rt.iter_mut().find(|r| r.spec.id == job) {
                            if r.done.insert(frame) {
                                db.mark_frame(&job, frame);
                                changed = true;
                            }
                        }
                    }
                    HubEvent::Done {
                        job,
                        chunk,
                        host,
                        kind,
                        detail,
                        ..
                    } => {
                        if let Some(r) = rt.iter_mut().find(|r| r.spec.id == job) {
                            let frames = r.blocks.remove(&chunk).unwrap_or_default();
                            for f in &frames {
                                r.remote.remove(f);
                            }
                            let title = r.spec.title();
                            if r.detached {
                                r.status = JobStatus::Canceled;
                                r.message.clear();
                            } else {
                                match kind {
                                    DoneKind::Ok => {
                                        if r.all_done() {
                                            r.status = JobStatus::Success;
                                            r.message.clear();
                                            log(
                                                &evt,
                                                format!(
                                                    "[{}] 完成（{} 帧，含 {} 上的分片）",
                                                    title,
                                                    r.done.len(),
                                                    host
                                                ),
                                            );
                                            db.save_job(&r.spec, 0, r.status, r.attempts);
                                        }
                                    }
                                    DoneKind::Canceled => {
                                        r.status = JobStatus::Canceled;
                                        r.message.clear();
                                        log(&evt, format!("[{}] 已取消（{host}）", title));
                                        db.save_job(&r.spec, 0, r.status, r.attempts);
                                    }
                                    DoneKind::Preflight | DoneKind::Fail => {
                                        r.attempts += 1;
                                        let what = if detail.is_empty() {
                                            "分片执行失败".to_string()
                                        } else {
                                            detail.clone()
                                        };
                                        if r.attempts <= max_retries {
                                            r.status = JobStatus::Queued;
                                            r.message = format!("第 {} 次重试", r.attempts);
                                            log(
                                                &evt,
                                                format!(
                                                    "[{}] {} 上失败，准备第 {} 次重试：{}",
                                                    title, host, r.attempts, what
                                                ),
                                            );
                                        } else {
                                            r.status = JobStatus::Failed;
                                            r.message = what.clone();
                                            log(&evt, format!("[{}] 失败：{}", title, what));
                                        }
                                        db.save_job(&r.spec, 0, r.status, r.attempts);
                                    }
                                }
                            }
                        }
                        changed = true;
                    }
                    HubEvent::Online(_) | HubEvent::Offline { .. } | HubEvent::Changed => {
                        // 设备面板的数据走单独的快照，这里只驱动一次重排
                        if let Some(h) = &hub {
                            let _ = evt.send(Event::Workers(h.views()));
                        }
                        changed = true;
                    }
                }
            }
        }

        // ── 调度 ──
        if state == RunState::Running {
            // 帧都齐了的任务先收工（远端分片可能已经把最后一帧跑完）
            for r in rt.iter_mut() {
                if r.detached || r.running || !r.blocks.is_empty() {
                    continue;
                }
                if matches!(r.status, JobStatus::Queued | JobStatus::Running) && r.all_done() {
                    r.status = JobStatus::Success;
                    changed = true;
                }
            }

            // ① 空闲的远端槽先喂：按帧块切，剩下再给本地
            if let Some(h) = &hub {
                loop {
                    let idle = h.idle_slots();
                    if idle == 0 {
                        break;
                    }
                    // 本机还剩几个空槽也要算进「这次切几份」：只有一台 worker 时
                    // 如果只看 idle(=1) 就会整包派出去，本地白白闲着。
                    // `max(1)` 再给本地预留一份：本机此刻可能正忙着别的任务，
                    // 但那个任务一结束就该能接上这里剩下的帧。
                    let free_local = concurrency.saturating_sub(
                        rt.iter().filter(|r| r.running).count(),
                    );
                    let shares = idle + free_local.max(1);
                    let Some(idx) = rt.iter().position(|r| {
                        !r.detached
                            && matches!(r.status, JobStatus::Queued | JobStatus::Running)
                            && !r.free_frames().is_empty()
                    }) else {
                        break;
                    };

                    let free = rt[idx].free_frames();
                    let remain = free.len();
                    // 可分片的按份均摊；不可分片的（输出不带 $F）必须整包给一台。
                    // 帧太少时也整包：一次 hip 加载几十秒，切两半纯亏（还是 8 帧起步）。
                    // 只在任务第一次分片时切，之后谁拿都是整包剩余（见 `paced`）。
                    let size = if is_splittable(&rt[idx].spec)
                        && !rt[idx].paced
                        && remain >= 8
                        && shares > 1
                    {
                        remain.div_ceil(shares).max(2)
                    } else {
                        remain
                    };
                    let take: Vec<i32> = free.into_iter().take(size).collect();
                    let chunk = block_name(&take);
                    let req = AssignReq {
                        job: rt[idx].spec.id.clone(),
                        chunk: chunk.clone(),
                        hip: rt[idx].spec.hip.to_string_lossy().to_string(),
                        node: rt[idx].spec.node_path.clone(),
                        houdini: rt[idx].spec.houdini.clone(),
                        output: rt[idx].spec.output.clone(),
                        frames: take.clone(),
                    };
                    // dispatch 失败说明没有匹配的机器（版本不符 / 拒绝过），别死循环
                    let Some(t) = h.dispatch(&req) else { break };

                    log(
                        &evt,
                        format!(
                            "[{}] 派给 {}：{} 帧（{}）",
                            rt[idx].spec.title(),
                            t.host,
                            take.len(),
                            chunk
                        ),
                    );
                    rt[idx].remote.extend(take.iter().copied());
                    rt[idx].blocks.insert(chunk, take);
                    rt[idx].paced = true;
                    if rt[idx].status == JobStatus::Queued {
                        rt[idx].status = JobStatus::Running;
                    }
                    changed = true;
                }
            }

            // ② 本地槽位：跑还没派出去的帧
            loop {
                let active = rt.iter().filter(|r| r.running).count();
                if active >= concurrency {
                    break;
                }
                let Some(idx) = rt.iter().position(|r| {
                    !r.running
                        && !r.detached
                        && matches!(r.status, JobStatus::Queued | JobStatus::Running)
                        && !r.free_frames().is_empty()
                }) else {
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

                // 只要还有 worker 在线，本地最多先拿一半：worker 手上那点活很快结束，
                // 把帧留在池子里它才能接上。没有 worker（纯单机）时仍然整包拿，
                // 行为与旧版完全一致。
                let mut frames = rt[idx].free_frames();
                let remote_online = hub.as_ref().map(|h| h.online_count() > 0).unwrap_or(false);
                if remote_online && !rt[idx].paced && frames.len() >= 8 {
                    frames.truncate(frames.len().div_ceil(2));
                    rt[idx].paced = true;
                }

                rt[idx].running = true;
                rt[idx].status = JobStatus::Running;
                rt[idx].cancel = Arc::new(AtomicBool::new(false));
                // 这些帧已交给本地线程：远端不能再派同一帧
                rt[idx].local.extend(frames.iter().copied());

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

            let active_local = rt.iter().filter(|r| r.running).count();
            let active_remote = rt.iter().filter(|r| !r.blocks.is_empty()).count();
            let pending = rt
                .iter()
                .filter(|r| matches!(r.status, JobStatus::Queued | JobStatus::Running))
                .count();
            if active_local == 0 && active_remote == 0 && pending == 0 {
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

/// 块名：`1052-1070`。只用于标识和日志，不参与去重（去重靠 `remote` 集合）。
fn block_name(frames: &[i32]) -> String {
    match (frames.first(), frames.last()) {
        (Some(a), Some(b)) if a == b => format!("{a}"),
        (Some(a), Some(b)) => format!("{a}-{b}"),
        _ => "-".into(),
    }
}

/// 这个任务的输出能不能被多台机器同时写。
///
/// 只有输出路径里带帧变量（`$F` / `$F4` / `${F}`）时，不同帧才落到不同文件。
/// 写单文件的任务（mov、单个 usd）一旦并行就是抢同一个文件，必须整包派一台。
fn is_splittable(spec: &JobSpec) -> bool {
    let o = &spec.output;
    if o.is_empty() {
        return false;
    }
    let up = o.to_uppercase();
    up.contains("$F") || up.contains("${F")
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
