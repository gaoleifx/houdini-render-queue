use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// 关掉多机：在跑的远端块作废，帧退回队列（UI 开关用）
    DetachHub,
    /// 设备面板打开时主动要一份快照
    RefreshWorkers,
    /// 单台 worker 调并发槽
    WorkerSlots(usize, u32),
    /// 单台 worker 停用 / 启用（停用后不派新活，连接保持）
    WorkerPause(usize, bool),
    /// 切某个任务的分片策略：true = 顺序执行（整包派一台）
    SetJobSequential(String, bool),
    Shutdown,
}

enum WResult {
    Ok,
    Preflight(String),
    Fail(String),
    Canceled,
}

/// 一块的实测耗时：加载（起进程 → READY）与平均单帧。
///
/// 分片不是为了「分得越细越快」——每片都要重开一次 hython、重读一遍 hip。
/// 只有 `帧数 × 单帧耗时` 明显大于加载耗时时，切开才是赚的。这两个数就是判据。
#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Perf {
    #[serde(default)]
    boot_ms: u64,
    #[serde(default)]
    frame_ms: u64,
}

impl Perf {
    /// 值得单独起一个进程的最小帧数。加载开销要能被这块的计算量盖过去
    /// （按「加载 ≤ 计算量的 2/3」算，即 1.5 倍）。
    fn min_frames(&self) -> usize {
        if self.boot_ms == 0 || self.frame_ms == 0 {
            return 8; // 没测到过：退回经验值
        }
        let need = (self.boot_ms as f64 * 1.5 / self.frame_ms as f64).ceil() as usize;
        need.clamp(2, 240)
    }
}

/// perf 表的两级键：`job|host` 是「这台机器跑这个任务」的实测，`job` 是泛化值。
/// 有了前者才能按机器快慢分活——同一份 hip 在 Z8 和旧机器上单帧能差三倍。
fn perf_key(job: &str, host: &str) -> String {
    format!("{job}|{host}")
}

fn frame_ms_for(perf: &HashMap<String, Perf>, job: &str, host: &str) -> Option<f64> {
    perf.get(&perf_key(job, host))
        .map(|p| p.frame_ms)
        .or_else(|| perf.get(job).map(|p| p.frame_ms))
        .filter(|v| *v > 0)
        .map(|v| v as f64)
}

/// 这个任务「值得单独开一次 hython」的门槛。优先用实测；
/// 没有 job 级数据时取各机器里**最大**的那个——门槛取小会让块切得更碎，
/// 一不小心就变成「算 3 帧、加载 60 秒」，所以宁可保守。
fn min_chunk_for(perf: &HashMap<String, Perf>, job: &str) -> usize {
    if let Some(p) = perf.get(job) {
        return p.min_frames();
    }
    let prefix = format!("{job}|");
    perf.iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, p)| p.min_frames())
        .max()
        .unwrap_or(8)
}

/// 各机器的速度权重（越大越快，拿到的帧越多）。
///
/// 只有一台有实测时没法比较，一律平均分——单台机器上「它的权重是 1」和
/// 「它的权重是 0.7」对结果没区别，而跟没有实测的机器混着比会凭空压低它。
fn speed_weights(perf: &HashMap<String, Perf>, job: &str, hosts: &[String]) -> Vec<f64> {
    let known: Vec<f64> = hosts
        .iter()
        .filter_map(|h| frame_ms_for(perf, job, h))
        .collect();
    if known.len() < 2 || known.len() != hosts.len() {
        return vec![1.0; hosts.len()];
    }
    let mut sorted = known.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    hosts
        .iter()
        .filter_map(|h| frame_ms_for(perf, job, h))
        // 差 4 倍封顶：机器型号差异不该让一台机器独占整个任务
        .map(|ms| (median / ms).clamp(0.25, 4.0))
        .collect()
}

enum WMsg {
    Frame(String, i32),
    Done(String, WResult, Perf),
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
    /// 刚在这个任务上失败的机器（主机名）。重试时避开它们——不然同一块会被派回同一台，
    /// 一路重试一路失败，直到 attempts 用完。
    ///
    /// 记主机名而不是连接 id：worker 断线重连会拿到新 id，按 id 记的话「换机重试」
    /// 在它重连后立刻失效。
    avoid: HashSet<String>,
    /// 跑过这个任务的机器名（本机也记）。纯展示用，卡片上那枚标签
    hosts: BTreeSet<String>,
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
            avoid: HashSet::new(),
            hosts: BTreeSet::new(),
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
    // 每个任务的实测加载/单帧耗时（上次跑出来的），决定这次切多大的块
    let mut perf: HashMap<String, Perf> = db
        .get_meta("perf")
        .and_then(|s| serde_json::from_str::<HashMap<String, Perf>>(&s).ok())
        .unwrap_or_default();
    // 上次是哪几台机器跑的：卡片上的机器标签要跨重启活着（否则历史任务看不出执行机器）
    {
        let saved = db.load_job_hosts();
        for r in rt.iter_mut() {
            if let Some(h) = saved.get(&r.spec.id) {
                for part in h.split(',').filter(|p| !p.is_empty()) {
                    r.hosts.insert(part.to_string());
                }
            }
        }
    }

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
                            // 上一轮拉黑的机器不作数了，新一轮重新给每台机会
                            r.avoid.clear();
                            r.hosts.clear();
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
                            r.avoid.clear();
                            r.hosts.clear();
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
                Cmd::DetachHub => {
                    // 多机被关掉：在跑的块全部退回队列（worker 那头很快会发现连接断了，
                    // 它会自己重连，但主控已经不接了）
                    let mut back = 0usize;
                    for r in rt.iter_mut() {
                        if r.blocks.is_empty() {
                            continue;
                        }
                        let blocks = std::mem::take(&mut r.blocks);
                        for frames in blocks.values() {
                            for f in frames {
                                r.remote.remove(f);
                            }
                            back += frames.len();
                            if !r.detached
                                && matches!(r.status, JobStatus::Queued | JobStatus::Running)
                            {
                                r.status = JobStatus::Queued;
                                r.message = format!("多机已关闭，{} 帧退回队列", frames.len());
                            }
                        }
                    }
                    hub = None;
                    hub_rx = None;
                    let _ = evt.send(Event::Workers(Vec::new()));
                    let _ = evt.send(Event::Queue(snapshot(&rt)));
                    if back > 0 {
                        log(&evt, format!("多机已关闭，{back} 帧退回本地队列"));
                    }
                }
                Cmd::RefreshWorkers => {
                    if let Some(h) = &hub {
                        let _ = evt.send(Event::Workers(h.views()));
                    }
                }
                Cmd::WorkerSlots(id, n) => {
                    if let Some(h) = &hub {
                        h.set_slots(id, n);
                        let _ = evt.send(Event::Workers(h.views()));
                    }
                }
                Cmd::WorkerPause(id, paused) => {
                    if let Some(h) = &hub {
                        h.set_paused(id, paused);
                        let _ = evt.send(Event::Workers(h.views()));
                        let host = h
                            .views()
                            .iter()
                            .find(|v| v.id == id)
                            .map(|v| v.host.clone())
                            .unwrap_or_default();
                        log(
                            &evt,
                            format!(
                                "{host} 已{}",
                                if paused { "停用（不再派新活）" } else { "启用" }
                            ),
                        );
                    }
                }
                Cmd::SetJobSequential(id, seq) => {
                    let ord = rt.iter().position(|x| x.spec.id == id).unwrap_or(0) as i64;
                    if let Some(r) = rt.iter_mut().find(|r| r.spec.id == id) {
                        r.spec.sequential = seq;
                        db.save_job(&r.spec, ord, r.status, r.attempts);
                        log(
                            &evt,
                            format!(
                                "[{}] 改为{}",
                                r.spec.title(),
                                if seq {
                                    "顺序执行（整包派一台机器，不分片）"
                                } else {
                                    "允许分片（多台机器各跑一段帧）"
                                }
                            ),
                        );
                    }
                    let _ = evt.send(Event::Queue(snapshot(&rt)));
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
                Ok(WMsg::Done(id, res, p)) => {
                    if let Some(r) = rt.iter_mut().find(|r| r.spec.id == id) {
                        r.running = false;
                        // 本地线程退出：它认领过的帧全部释放（没跑完的回到可调度池）
                        r.local.clear();
                        if p.frame_ms > 0 {
                            // 本地跑的也是同一个 hip 同样的帧，实测数一样能用来算块大小
                            perf.insert(id.clone(), p);
                            save_perf(&db, &perf);
                        }
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
                        worker: _,
                        host,
                        kind,
                        detail,
                        boot_ms,
                        frame_ms,
                    } => {
                        if let Some(r) = rt.iter_mut().find(|r| r.spec.id == job) {
                            let frames = r.blocks.remove(&chunk).unwrap_or_default();
                            for f in &frames {
                                r.remote.remove(f);
                            }
                            let title = r.spec.title();
                            r.hosts.insert(host.clone());
                            persist_hosts(&db, &r.spec.id, &r.hosts);
                            if boot_ms > 0 && frame_ms > 0 {
                                // 两处都记：`job|host` 用来按机器快慢分活，
                                // `job` 保持泛化值（老库里只有这一种）
                                perf.insert(perf_key(&job, &host), Perf { boot_ms, frame_ms });
                                perf.insert(job.clone(), Perf { boot_ms, frame_ms });
                                save_perf(&db, &perf);
                            }
                            if r.detached {
                                r.status = JobStatus::Canceled;
                                r.message.clear();
                            } else {
                                match kind {
                                    DoneKind::Ok => {
                                        // 这台上过了就没问题，之前拉黑的记录作废
                                        r.avoid.remove(&host);
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
                                        // 下次重试别再派回这台：同一块在同一台反复失败
                                        // 是 attempts 被白白烧完的最常见原因
                                        if !host.is_empty() {
                                            r.avoid.insert(host.clone());
                                        }
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
                    HubEvent::Offline {
                        worker: _,
                        host,
                        running,
                    } => {
                        // 掉线的机器手上还有块：那些帧既不会完成也不会自己重派，
                        // 必须在这里退回可调度池，否则任务永远停在「执行中」
                        for (job, chunk) in running {
                            let Some(r) = rt.iter_mut().find(|r| r.spec.id == job) else {
                                continue;
                            };
                            let frames = r.blocks.remove(&chunk).unwrap_or_default();
                            for f in &frames {
                                r.remote.remove(f);
                            }
                            if !host.is_empty() {
                                r.avoid.insert(host.clone());
                            }
                            if frames.is_empty() {
                                continue;
                            }
                            if !r.detached && matches!(r.status, JobStatus::Queued | JobStatus::Running) {
                                r.status = if r.running || !r.blocks.is_empty() {
                                    JobStatus::Running
                                } else {
                                    JobStatus::Queued
                                };
                                r.message = format!("{host} 掉线，{} 帧重新排队", frames.len());
                                log(
                                    &evt,
                                    format!(
                                        "[{}] {host} 掉线，{} 帧退回队列（这台机器本次不再派发）",
                                        r.spec.title(),
                                        frames.len()
                                    ),
                                );
                            }
                        }
                        if let Some(h) = &hub {
                            let _ = evt.send(Event::Workers(h.views()));
                        }
                        changed = true;
                    }
                    HubEvent::Resumed {
                        job,
                        chunk,
                        frames,
                        host,
                    } => {
                        // 主控重启 / worker 重连后它手上那块还在跑：这些帧必须记成
                        // 「远端已认领」，否则本地会把它们再渲一遍，两个进程抢同一个输出文件
                        if let Some(r) = rt.iter_mut().find(|r| r.spec.id == job) {
                            if !r.blocks.contains_key(&chunk) {
                                r.hosts.insert(host.clone());
                                r.remote.extend(frames.iter().copied());
                                r.blocks.insert(chunk.clone(), frames.clone());
                                if r.status == JobStatus::Queued {
                                    r.status = JobStatus::Running;
                                }
                            }
                        }
                        changed = true;
                    }
                    HubEvent::Stopped => {
                        log(&evt, "多机已关闭，正在跑的远端块交回本地".into());
                        changed = true;
                    }
                    HubEvent::Online(v) => {
                        // 重连回来的机器解除之前的拉黑：它可能刚修好挂载/许可证，
                        // 一直记着等于这台机器再也接不到这个任务
                        let mut freed = 0;
                        for r in rt.iter_mut() {
                            if r.avoid.remove(&v.host) {
                                freed += 1;
                            }
                        }
                        if freed > 0 {
                            log(
                                &evt,
                                format!("{} 重新上线，解除 {} 个任务对它的回避", v.host, freed),
                            );
                        }
                        if let Some(h) = &hub {
                            let _ = evt.send(Event::Workers(h.views()));
                        }
                        changed = true;
                    }
                    HubEvent::Changed => {
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
                    // 可分片的按份均摊；不可分片的（输出不带 $F、或勾了顺序执行）
                    // 必须整包给一台。
                    //
                    // 下限 `min_frames` 来自实测：一次 hip 加载可能要几十秒到几分钟，
                    // 切出来的块如果小到「算 3 帧、加载 60 秒」，多机反而比单机慢。
                    // 这个数一开始是经验值 8，跑过一次后就按真实耗时算了（见 Perf）。
                    let job_id = rt[idx].spec.id.clone();
                    let min_chunk = min_chunk_for(&perf, &job_id);
                    let exclude: Vec<String> = rt[idx].avoid.iter().cloned().collect();
                    let mut req = AssignReq {
                        job: job_id.clone(),
                        chunk: String::new(),
                        hip: rt[idx].spec.hip.to_string_lossy().to_string(),
                        node: rt[idx].spec.node_path.clone(),
                        houdini: rt[idx].spec.houdini.clone(),
                        output: rt[idx].spec.output.clone(),
                        frames: Vec::new(),
                        exclude,
                    };
                    // 按机器快慢分配份额：快的多拿。权重列表第 0 台正是 dispatch 会选中的那台。
                    let hosts = h.idle_hosts_for(&req);
                    let size = if is_splittable(&rt[idx].spec)
                        && shares > 1
                        && remain >= min_chunk * shares * 2
                    {
                        let ws = speed_weights(&perf, &job_id, &hosts);
                        let frac = if ws.len() >= 2 {
                            let total: f64 = ws.iter().sum();
                            if total > 0.0 { ws[0] / total } else { 1.0 / shares as f64 }
                        } else {
                            1.0 / shares as f64
                        };
                        let want = ((remain as f64) * frac).ceil() as usize;
                        want.clamp(min_chunk, remain)
                    } else {
                        remain
                    };
                    let take: Vec<i32> = free.into_iter().take(size).collect();
                    let chunk = block_name(&take);
                    req.chunk = chunk.clone();
                    req.frames = take.clone();
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
                    rt[idx].hosts.insert(t.host.clone());
                    persist_hosts(&db, &rt[idx].spec.id, &rt[idx].hosts);
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
                // 行为与旧版完全一致。切走的量同样不能低于「值得重开一次 hython」的门槛。
                let mut frames = rt[idx].free_frames();
                let remote_online = hub.as_ref().map(|h| h.online_count() > 0).unwrap_or(false);
                let min_chunk = min_chunk_for(&perf, &rt[idx].spec.id);
                if remote_online && frames.len() >= min_chunk * 2 && is_splittable(&rt[idx].spec) {
                    // 远端还空着，本地只拿一半，剩下的留在池子里给它
                    frames.truncate(frames.len().div_ceil(2));
                }

                rt[idx].running = true;
                rt[idx].status = JobStatus::Running;
                rt[idx].cancel = Arc::new(AtomicBool::new(false));
                rt[idx].hosts.insert("本机".into());
                persist_hosts(&db, &rt[idx].spec.id, &rt[idx].hosts);
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

/// 队列卡片上那枚机器标签。卡片只有 260px 宽，多台时只报台数。
fn host_label(hosts: &BTreeSet<String>) -> String {
    match hosts.len() {
        0 => String::new(),
        1 => {
            let h = hosts.iter().next().map(String::as_str).unwrap_or("");
            if h.chars().count() > 10 {
                let cut: String = h.chars().take(10).collect();
                format!("{cut}…")
            } else {
                h.to_string()
            }
        }
        n => format!("多机 {n}"),
    }
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
///
/// 注意这只是**必要条件**：每帧写不同文件不代表帧与帧无关。解算类节点分片后
/// 结果会错，程序判断不出来——所以还有 `sequential` 这个人工开关。
fn is_splittable(spec: &JobSpec) -> bool {
    if spec.sequential {
        return false;
    }
    let o = &spec.output;
    if o.is_empty() {
        return false;
    }
    let up = o.to_uppercase();
    up.contains("$F") || up.contains("${F")
}

/// 实测耗时落库（meta 里一行 JSON）。失败就失败，不影响调度。
fn save_perf(db: &Db, perf: &HashMap<String, Perf>) {
    if let Ok(s) = serde_json::to_string(perf) {
        db.set_meta("perf", &s);
    }
}

/// 机器标签落库：重启后卡片上还能看出这任务是哪几台跑的。
fn persist_hosts(db: &Db, id: &str, hosts: &BTreeSet<String>) {
    let joined: Vec<&str> = hosts.iter().map(String::as_str).collect();
    db.set_job_host(id, &joined.join(","));
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
                host: host_label(&r.hosts),
                sequential: r.spec.sequential,
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
        let _ = tx.send(WMsg::Done(
            id,
            WResult::Fail(format!("写入帧计划失败: {e}")),
            Perf::default(),
        ));
        return;
    }

    let script = pyproc::script_path("render.py");
    if !script.exists() {
        let _ = tx.send(WMsg::Done(
            id,
            WResult::Fail(format!("找不到 render.py: {}", script.display())),
            Perf::default(),
        ));
        return;
    }

    let started = Instant::now();
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
                Perf::default(),
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

    let mut ready = false;
    let mut ready_at: Option<Instant> = None;
    let mut done_frames = 0u32;
    let mut last_done: Option<Instant> = None;
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
                        ready_at.get_or_insert_with(Instant::now);
                        let _ = evt.send(Event::Log(format!(
                            "{}  [{}] 工程加载完成",
                            pyproc::now_hms(),
                            title
                        )));
                    }
                    "FRAME" => {}
                    "DONE" => {
                        done_frames += 1;
                        last_done = Some(Instant::now());
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
        if !ready && result.is_none() && started.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            result = Some(WResult::Fail(
                "启动超时 5 分钟（可能是 Houdini 许可证被占用，或工程过大）".into(),
            ));
        }

        thread::sleep(Duration::from_millis(120));
    }

    let _ = std::fs::remove_file(&plan);
    let _ = std::fs::remove_file(&cancel_file);

    // 实测耗时：本地跑的也是同一个 hip、同样的帧，照样能用来算块大小
    let mut p = Perf::default();
    if let Some(t) = ready_at {
        p.boot_ms = (t - started).as_millis() as u64;
        if let Some(last) = last_done {
            if done_frames > 0 {
                p.frame_ms = ((last - t).as_millis() as u64 / done_frames as u64).max(1);
            }
        }
    }

    let _ = tx.send(WMsg::Done(
        id,
        result.unwrap_or(WResult::Fail("未知结束状态".into())),
        p,
    ));
}

// ───────────────────────── 单测 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::FrameSet;

    fn spec(output: &str, sequential: bool) -> JobSpec {
        JobSpec {
            id: "k::/out/render".into(),
            project_key: "k".into(),
            hip: "P://sh010.hip".into(),
            hython: "hython.exe".into(),
            houdini: "21.0.596".into(),
            shot: "sh010".into(),
            node_path: "/out/render".into(),
            node_type: "karma".into(),
            category: Category::Render,
            output: output.into(),
            frames: FrameSet::span(1, 100),
            sequential,
        }
    }

    #[test]
    fn perf_min_frames_amortizes_boot() {
        // 加载 60 秒、每帧 5 秒：一块至少要 18 帧，否则加载开销盖过计算
        let p = Perf {
            boot_ms: 60_000,
            frame_ms: 5_000,
        };
        assert_eq!(p.min_frames(), 18);
    }

    #[test]
    fn perf_min_frames_falls_back_without_data() {
        assert_eq!(Perf::default().min_frames(), 8);
        // 只测到一半的数据也不能用
        let p = Perf {
            boot_ms: 30_000,
            frame_ms: 0,
        };
        assert_eq!(p.min_frames(), 8);
    }

    #[test]
    fn perf_min_frames_clamped() {
        // 加载极长 / 单帧极快：上限 240，不能把整包切到无限细
        let p = Perf {
            boot_ms: 600_000,
            frame_ms: 1,
        };
        assert_eq!(p.min_frames(), 240);
        // 加载比一帧还短：下限 2（1 帧的块没有意义）
        let p = Perf {
            boot_ms: 100,
            frame_ms: 10_000,
        };
        assert_eq!(p.min_frames(), 2);
    }

    #[test]
    fn splittable_needs_frame_var() {
        assert!(is_splittable(&spec("/out/render.$F4.exr", false)));
        assert!(is_splittable(&spec("/out/render.${F}.exr", false)));
        // 写单文件：多台同时写必坏
        assert!(!is_splittable(&spec("/out/preview.mov", false)));
        assert!(!is_splittable(&spec("", false)));
    }

    #[test]
    fn sequential_never_splits() {
        // 输出带 $F 但勾了顺序执行：跨帧依赖的节点只能整包跑
        assert!(!is_splittable(&spec("/out/render.$F4.exr", true)));
    }

    #[test]
    fn host_label_shrinks() {
        let mut s = BTreeSet::new();
        assert_eq!(host_label(&s), "");
        s.insert("WS-02".to_string());
        assert_eq!(host_label(&s), "WS-02");
        s.insert("WS-03".to_string());
        assert_eq!(host_label(&s), "多机 2");
        let mut one = BTreeSet::new();
        one.insert("RENDER-FARM-NODE-007".to_string());
        assert_eq!(host_label(&one), "RENDER-FAR…");
    }

    /// `job|host` 是「这台机器跑这个任务」的实测，`job` 是泛化值。
    /// 有前者就用前者，否则退回后者——老库里只有 `job` 一种键。
    #[test]
    fn perf_prefers_per_host_then_falls_back() {
        let mut perf = HashMap::new();
        perf.insert("j".to_string(), Perf { boot_ms: 10_000, frame_ms: 1_000 });
        assert_eq!(frame_ms_for(&perf, "j", "WS-02"), Some(1_000.0));

        perf.insert(
            perf_key("j", "WS-02"),
            Perf { boot_ms: 10_000, frame_ms: 200 },
        );
        assert_eq!(frame_ms_for(&perf, "j", "WS-02"), Some(200.0));
        // 别的机器仍然用泛化值
        assert_eq!(frame_ms_for(&perf, "j", "WS-03"), Some(1_000.0));
        assert_eq!(frame_ms_for(&perf, "别的任务", "WS-02"), None);
    }

    /// 快的机器权重大、该多拿帧；差 4 倍封顶，避免一台机器吃光整个任务。
    #[test]
    fn speed_weights_favor_faster_hosts() {
        let mut perf = HashMap::new();
        perf.insert(perf_key("j", "FAST"), Perf { boot_ms: 1_000, frame_ms: 100 });
        perf.insert(perf_key("j", "SLOW"), Perf { boot_ms: 1_000, frame_ms: 400 });
        let hosts = vec!["FAST".to_string(), "SLOW".to_string()];
        let w = speed_weights(&perf, "j", &hosts);
        assert!(w[0] > w[1], "快的应该权重大：{w:?}");
        assert!((w[0] - 4.0).abs() < 1e-6, "400/100 = 4 倍：{w:?}");

        // 差 20 倍也只给 4 倍，且权重按中位数归一
        perf.insert(perf_key("j", "TINY"), Perf { boot_ms: 1_000, frame_ms: 5 });
        let w = speed_weights(&perf, "j", &hosts);
        assert!(w[0] <= 4.0 && w[0] >= 0.25);
    }

    /// 只有一台有实测（或都没有）时，没法横向比较：平均分，别凭空压低。
    #[test]
    fn speed_weights_are_equal_without_comparison() {
        let mut perf = HashMap::new();
        perf.insert(perf_key("j", "ONLY"), Perf { boot_ms: 1_000, frame_ms: 100 });
        let hosts = vec!["ONLY".to_string(), "NEW".to_string()];
        assert_eq!(speed_weights(&perf, "j", &hosts), vec![1.0, 1.0]);
        assert_eq!(speed_weights(&perf, "j", &[]), Vec::<f64>::new());
    }

    /// 分片下限只看实测；没有 job 级数据时取各机器里最保守（最大）的那个。
    #[test]
    fn min_chunk_uses_conservative_floor() {
        let mut perf = HashMap::new();
        perf.insert(perf_key("j", "A"), Perf { boot_ms: 60_000, frame_ms: 5_000 }); // 18 帧
        perf.insert(perf_key("j", "B"), Perf { boot_ms: 30_000, frame_ms: 5_000 }); // 9 帧
        assert_eq!(min_chunk_for(&perf, "j"), 18);
        perf.insert("j".to_string(), Perf { boot_ms: 60_000, frame_ms: 6_000 }); // 15 帧
        assert_eq!(min_chunk_for(&perf, "j"), 15);
        assert_eq!(min_chunk_for(&perf, "没跑过"), 8);
    }
}
