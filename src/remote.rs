//! 局域网多机执行（P1）：NDJSON 行协议 + master 侧 Hub + worker 角色。
//!
//! 设计文档见 `docs/multi-machine.md`。这里只实现「能跑通」的最小闭环：
//!
//! - master（就是主界面进程）监听 TCP，维护在线 worker 池，按**帧块**派发
//! - worker 是同一个 exe 加 `--worker --connect host:port`，不初始化界面、不碰数据库
//! - 真正的渲染仍然由 worker 本机的 hython 跑 `python/render.py`，输出标记行原样回传
//!
//! 为什么是帧块而不是逐帧：`render.py` 一次加载 hip 能连跑多帧，逐帧派发会让
//! 「加载 3 分钟、渲染 10 秒」的开销压倒计算。块内仍是一个 hython 进程，所以
//! Python 侧一行都不用改。
//!
//! 线程模型（刻意不引入 async runtime，2-3 台机器用不上）：
//!
//! ```text
//! master: listener 线程 ──accept──> 每连接 1 读线程 + 1 写线程
//!                                      └─ HubEvent ─> 引擎线程（调度）
//! worker: 主线程读 socket ──Assign──> 执行线程（起 hython）
//!                                      └─ 行 ─> 写线程 ─> master
//! ```

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::houdini;
use crate::pyproc;

/// 协议版本。两边对不上直接断开并说清楚，别静默半死不活。
pub const PROTO: u32 = 1;

/// 默认监听端口（可在 `--listen` 覆盖）。
pub const DEFAULT_PORT: u16 = 7878;

/// 心跳间隔 / 判定离线阈值。
const BEAT_EVERY: Duration = Duration::from_secs(5);
const OFFLINE_AFTER: Duration = Duration::from_secs(15);

// ───────────────────────── 协议 ─────────────────────────

/// 一行一个 JSON（NDJSON）。`t` 是消息类型，`seq` 只用于请求-应答配对。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// worker → master：报到
    Hello {
        seq: u64,
        ver: u32,
        host: String,
        slots: u32,
        houdini: Vec<String>,
    },
    /// master → worker：接受报到
    Welcome {
        seq: u64,
        master: String,
        proto: u32,
        poll_ms: u64,
    },
    /// worker → master：我空着，有活给我
    Claim { seq: u64, busy: u32, free: u32 },
    /// master → worker：给你一块
    Assign {
        seq: u64,
        job: String,
        chunk: String,
        hip: String,
        node: String,
        houdini: String,
        output: String,
        frames: Vec<i32>,
    },
    Accept { seq: u64, job: String, chunk: String },
    Reject {
        seq: u64,
        job: String,
        chunk: String,
        why: String,
    },
    /// worker → master：hython 的一行输出（只回传标记行，闲话留在本地）
    Line {
        job: String,
        chunk: String,
        data: String,
    },
    /// master → worker：请在该帧结束后停下
    Cancel { job: String, chunk: String },
    /// master → worker：调整本机并发槽数（设备面板上 +/-）
    Slots { seq: u64, slots: u32 },
    /// worker → master：这块结束了
    Result {
        job: String,
        chunk: String,
        kind: String,
        detail: String,
        code: i32,
        /// 从起进程到 `##READY` 的耗时。主控用它算「这块切多大才不亏」
        #[serde(default)]
        boot_ms: u64,
        /// 该块平均单帧耗时。同上
        #[serde(default)]
        frame_ms: u64,
    },
    /// worker → master：心跳
    Beat {
        busy: u32,
        job: String,
        chunk: String,
        frame: i32,
        mem_mb: u64,
    },
}

impl Msg {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

fn write_msg(out: &Sender<String>, m: &Msg) {
    let _ = out.send(m.to_line());
}

// ───────────────────────── master：Hub ─────────────────────────

/// 派发结果：给了谁。
#[derive(Clone, Debug)]
pub struct Ticket {
    pub worker: usize,
    pub host: String,
    pub chunk: String,
}

/// 引擎线程消费的 worker 事件。
#[derive(Debug)]
pub enum HubEvent {
    Online(WorkerView),
    Offline { worker: usize, host: String, running: Vec<(String, String)> },
    /// 某块跑完了一帧
    Frame { job: String, chunk: String, frame: i32 },
    /// 某块结束了
    Done {
        job: String,
        chunk: String,
        worker: usize,
        host: String,
        kind: DoneKind,
        detail: String,
        /// 该块实测：加载耗时 / 单帧耗时（0 = 没测到）
        boot_ms: u64,
        frame_ms: u64,
    },
    Log(String),
    /// 池子变了（上下线 / 槽位 / 心跳），界面的设备面板要刷
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneKind {
    Ok,
    Preflight,
    Fail,
    Canceled,
}

impl DoneKind {
    pub fn parse(s: &str) -> DoneKind {
        match s {
            "ok" => DoneKind::Ok,
            "preflight" => DoneKind::Preflight,
            "canceled" => DoneKind::Canceled,
            _ => DoneKind::Fail,
        }
    }
}

/// 给界面看的 worker 快照。
#[derive(Clone, Debug, Default)]
pub struct WorkerView {
    pub id: usize,
    pub host: String,
    pub online: bool,
    pub slots: u32,
    pub busy: u32,
    pub houdini: String,
    pub job: String,
    pub chunk: String,
    pub frame: i32,
    pub mem_mb: u64,
    /// 被主控停用：连接还在，但不派新活（有人要拿这台机器干别的）
    pub paused: bool,
}

/// 一次派发请求（引擎侧填）。
pub struct AssignReq {
    pub job: String,
    pub chunk: String,
    pub hip: String,
    pub node: String,
    pub houdini: String,
    pub output: String,
    pub frames: Vec<i32>,
    /// 本次不要派给这些 worker（刚在这台上失败过）。空 = 不限制。
    pub exclude: Vec<usize>,
}

struct Worker {
    id: usize,
    host: String,
    slots: u32,
    busy: u32,
    vers: Vec<String>,
    out: Sender<String>,
    last_beat: Instant,
    online: bool,
    paused: bool,
    job: String,
    chunk: String,
    frame: i32,
    mem_mb: u64,
    /// 这台机器拒绝过的块（路径不可见 / 版本缺失）。同一个块不再往它身上派。
    rejected: HashSet<String>,
    /// 已经派出去、还没回报的块。掉线时要把这份交回引擎，否则那些帧
    /// 既不会完成也不会重派，整个任务永远停在「执行中」。
    inflight: Vec<(String, String)>,
}

struct Pool {
    workers: Vec<Worker>,
    next_seq: u64,
}

pub struct Hub {
    pool: Mutex<Pool>,
    tx: Sender<HubEvent>,
    port: u16,
}

impl Hub {
    /// 起监听。返回的 Receiver 必须交给引擎线程独占消费。
    pub fn start(port: u16) -> std::io::Result<(Arc<Hub>, Receiver<HubEvent>)> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let (tx, rx) = mpsc::channel::<HubEvent>();
        let hub = Arc::new(Hub {
            pool: Mutex::new(Pool {
                workers: Vec::new(),
                next_seq: 1,
            }),
            tx: tx.clone(),
            port,
        });

        // accept 循环
        {
            let hub = hub.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(stream) = conn else { continue };
                    let hub = hub.clone();
                    let tx = tx.clone();
                    thread::spawn(move || hub.serve_conn(stream, tx));
                }
            });
        }

        // 心跳超时巡检
        {
            let hub = hub.clone();
            let tx = tx.clone();
            thread::spawn(move || loop {
                thread::sleep(Duration::from_secs(3));
                let mut p = hub.pool.lock().unwrap();
                let now = Instant::now();
                let mut changed = false;
                for w in p.workers.iter_mut() {
                    if w.online && now.duration_since(w.last_beat) > OFFLINE_AFTER {
                        w.online = false;
                        w.busy = 0;
                        changed = true;
                        let lost = std::mem::take(&mut w.inflight);
                        let _ = tx.send(HubEvent::Offline {
                            worker: w.id,
                            host: w.host.clone(),
                            running: lost,
                        });
                        let _ = tx.send(HubEvent::Log(format!(
                            "worker {} 心跳超时，已标记离线",
                            w.host
                        )));
                    }
                }
                if changed {
                    let _ = tx.send(HubEvent::Changed);
                }
            });
        }

        let _ = tx.send(HubEvent::Log(format!(
            "多机执行已监听 0.0.0.0:{}（worker 运行：--worker --connect <本机IP>:{})",
            port, port
        )));
        Ok((hub, rx))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn serve_conn(self: Arc<Self>, stream: TcpStream, tx: Sender<HubEvent>) {
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let (out_tx, out_rx) = mpsc::channel::<String>();

        // 写线程：所有上行消息都从这里走，避免半行交错
        let mut wr = stream;
        thread::spawn(move || {
            for line in out_rx {
                if wr.write_all(line.as_bytes()).is_err() || wr.write_all(b"\n").is_err() {
                    break;
                }
                let _ = wr.flush();
            }
        });

        let mut worker_id: Option<usize> = None;
        let mut host = String::from("?");
        let reader = BufReader::new(read_half);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Msg>(&line) else {
                let _ = tx.send(HubEvent::Log(format!("收到无法解析的消息：{line}")));
                continue;
            };
            match msg {
                Msg::Hello {
                    seq,
                    ver,
                    host: h,
                    slots,
                    houdini,
                } => {
                    host = h;
                    if ver != PROTO {
                        let _ = tx.send(HubEvent::Log(format!(
                            "{host} 协议版本 {ver}，主控是 {PROTO}，拒绝接入（两端 exe 要同版本）"
                        )));
                        break;
                    }
                    let mut p = self.pool.lock().unwrap();
                    p.next_seq += 1;
                    let id = p.next_seq as usize;
                    p.workers.push(Worker {
                        id,
                        host: host.clone(),
                        slots: slots.max(1),
                        busy: 0,
                        vers: houdini.clone(),
                        out: out_tx.clone(),
                        last_beat: Instant::now(),
                        online: true,
                        paused: false,
                        job: String::new(),
                        chunk: String::new(),
                        frame: 0,
                        mem_mb: 0,
                        rejected: HashSet::new(),
                        inflight: Vec::new(),
                    });
                    worker_id = Some(id);
                    let wv = p.workers.last().map(view_of).unwrap_or_default();
                    drop(p);
                    write_msg(
                        &out_tx,
                        &Msg::Welcome {
                            seq,
                            master: host_of_me(),
                            proto: PROTO,
                            poll_ms: 1500,
                        },
                    );
                    let _ = tx.send(HubEvent::Online(wv));
                    let _ = tx.send(HubEvent::Log(format!(
                        "{host} 已接入（{} 个槽位，Houdini {:?}）",
                        slots,
                        houdini
                    )));
                    let _ = tx.send(HubEvent::Changed);
                }
                Msg::Claim { .. } => {
                    // worker 说「我空出来了」。忙数由主控自己记账（派发 +1 / 回报 -1），
                    // 不采信对端的数字：心跳与派发交叉时，过期的 claim 会把刚派出去的
                    // 槽位又报成空闲，主控就会往一个 1 槽的机器上压第二块。
                    if let Some(w) = self
                        .pool
                        .lock()
                        .unwrap()
                        .workers
                        .iter_mut()
                        .find(|w| Some(w.id) == worker_id)
                    {
                        w.last_beat = Instant::now();
                        w.online = true;
                    }
                }
                Msg::Accept { job, chunk, .. } => {
                    let _ = tx.send(HubEvent::Log(format!("{host} 接受 {job} [{chunk}]")));
                }
                Msg::Reject {
                    job, chunk, why, ..
                } => {
                    let mut p = self.pool.lock().unwrap();
                    if let Some(w) = p.workers.iter_mut().find(|w| Some(w.id) == worker_id) {
                        w.busy = w.busy.saturating_sub(1);
                        w.job.clear();
                        w.chunk.clear();
                        w.rejected.insert(chunk_key(&job, &chunk));
                        w.inflight.retain(|(j, c)| !(j == &job && c == &chunk));
                    }
                    drop(p);
                    let _ = tx.send(HubEvent::Log(format!(
                        "{host} 拒绝 {job} [{chunk}]：{why}"
                    )));
                    // 让引擎把这块重新排队
                    let _ = tx.send(HubEvent::Done {
                        job,
                        chunk,
                        worker: worker_id.unwrap_or(0),
                        host: host.clone(),
                        kind: DoneKind::Fail,
                        detail: format!("{host} 拒绝：{why}"),
                        boot_ms: 0,
                        frame_ms: 0,
                    });
                    let _ = tx.send(HubEvent::Changed);
                }
                Msg::Line { job, chunk, data } => {
                    if let Some(rest) = data.strip_prefix("##DONE ") {
                        if let Ok(f) = rest.trim().parse::<i32>() {
                            let _ = tx.send(HubEvent::Frame {
                                job: job.clone(),
                                chunk: chunk.clone(),
                                frame: f,
                            });
                        }
                    }
                    if let Some(w) = self
                        .pool
                        .lock()
                        .unwrap()
                        .workers
                        .iter_mut()
                        .find(|w| Some(w.id) == worker_id)
                    {
                        w.last_beat = Instant::now();
                    }
                    // 标记行交给引擎按现有规则解析；普通行也转成日志（worker 已过滤）
                    let _ = tx.send(HubEvent::Log(format!("[{host}] {data}")));
                }
                Msg::Result {
                    job,
                    chunk,
                    kind,
                    detail,
                    code: _,
                    boot_ms,
                    frame_ms,
                } => {
                    let mut p = self.pool.lock().unwrap();
                    if let Some(w) = p.workers.iter_mut().find(|w| Some(w.id) == worker_id) {
                        w.busy = w.busy.saturating_sub(1);
                        w.job.clear();
                        w.chunk.clear();
                        w.frame = 0;
                        w.last_beat = Instant::now();
                        w.inflight.retain(|(j, c)| !(j == &job && c == &chunk));
                    }
                    drop(p);
                    let _ = tx.send(HubEvent::Done {
                        job,
                        chunk,
                        worker: worker_id.unwrap_or(0),
                        host: host.clone(),
                        kind: DoneKind::parse(&kind),
                        detail,
                        boot_ms,
                        frame_ms,
                    });
                    let _ = tx.send(HubEvent::Changed);
                }
                Msg::Beat {
                    busy: _rep,
                    job,
                    chunk,
                    frame,
                    mem_mb,
                } => {
                    let mut p = self.pool.lock().unwrap();
                    if let Some(w) = p.workers.iter_mut().find(|w| Some(w.id) == worker_id) {
                        w.last_beat = Instant::now();
                        w.online = true;
                        // 不用对端报的 busy 覆盖本地记账（见 Claim 处的说明）
                        w.job = job;
                        w.chunk = chunk;
                        w.frame = frame;
                        w.mem_mb = mem_mb;
                    }
                }
                _ => {}
            }
        }

        // 连接断了
        if let Some(id) = worker_id {
            let mut p = self.pool.lock().unwrap();
            if let Some(w) = p.workers.iter_mut().find(|w| w.id == id) {
                w.online = false;
                w.busy = 0;
                let lost = std::mem::take(&mut w.inflight);
                let _ = tx.send(HubEvent::Offline {
                    worker: id,
                    host: w.host.clone(),
                    running: lost,
                });
                let _ = tx.send(HubEvent::Log(format!("{} 连接断开", w.host)));
            }
            drop(p);
            let _ = tx.send(HubEvent::Changed);
        }
    }

    /// 找一个能接活的 worker 并立即把 assign 推过去。
    ///
    /// 乐观占用：发出去就把槽算成忙，收到 `Reject` 再退回来。这样不用维护
    /// 「已发未回」的状态机，代价是 reject 会浪费一次往返——2-3 台机器完全划算。
    pub fn dispatch(&self, req: &AssignReq) -> Option<Ticket> {
        let mut p = self.pool.lock().unwrap();
        let key = chunk_key(&req.job, &req.chunk);

        // 先找版本对得上的；一个都没有就退到任意空闲机器。
        // 版本最终由 worker 自己判断（找不到匹配的 hython 会 reject），
        // 这里硬过滤会让「工程记录的版本号跟本机目录名不一致」的情况永远派不出去。
        let mut exact: Option<usize> = None;
        let mut loose: Option<usize> = None;
        for (i, w) in p.workers.iter().enumerate() {
            if !w.online
                || w.paused
                || w.busy >= w.slots
                || w.rejected.contains(&key)
                || req.exclude.contains(&w.id)
            {
                continue;
            }
            if loose.is_none() {
                loose = Some(i);
            }
            let ver_ok = req.houdini.is_empty()
                || w.vers.is_empty()
                || w.vers.iter().any(|v| v == &req.houdini);
            if ver_ok {
                exact = Some(i);
                break;
            }
        }
        let idx = exact.or(loose)?;

        p.next_seq += 1;
        let seq = p.next_seq;
        let w = &mut p.workers[idx];
        w.busy += 1;
        w.job = req.job.clone();
        w.chunk = req.chunk.clone();
        w.inflight.push((req.job.clone(), req.chunk.clone()));
        write_msg(
            &w.out,
            &Msg::Assign {
                seq,
                job: req.job.clone(),
                chunk: req.chunk.clone(),
                hip: req.hip.clone(),
                node: req.node.clone(),
                houdini: req.houdini.clone(),
                output: req.output.clone(),
                frames: req.frames.clone(),
            },
        );
        Some(Ticket {
            worker: w.id,
            host: w.host.clone(),
            chunk: req.chunk.clone(),
        })
    }

    /// 取消某块（worker 会在当前帧结束后停）。
    pub fn cancel(&self, job: &str, chunk: &str) {
        let p = self.pool.lock().unwrap();
        for w in p.workers.iter().filter(|w| w.online) {
            if w.job == job && w.chunk == chunk {
                write_msg(
                    &w.out,
                    &Msg::Cancel {
                        job: job.to_string(),
                        chunk: chunk.to_string(),
                    },
                );
            }
        }
    }

    pub fn views(&self) -> Vec<WorkerView> {
        let p = self.pool.lock().unwrap();
        p.workers.iter().map(view_of).collect()
    }

    /// 有空槽的 worker 数（引擎用来决定要不要切块派发）。停用的不算。
    pub fn idle_slots(&self) -> usize {
        let p = self.pool.lock().unwrap();
        p.workers
            .iter()
            .filter(|w| w.online && !w.paused)
            .map(|w| (w.slots.saturating_sub(w.busy)) as usize)
            .sum()
    }

    pub fn online_count(&self) -> usize {
        let p = self.pool.lock().unwrap();
        p.workers.iter().filter(|w| w.online).count()
    }

    /// 调整某台 worker 的并发槽。只发通知，本地立刻生效；worker 收到后改自己的上限。
    pub fn set_slots(&self, id: usize, slots: u32) {
        let slots = slots.clamp(1, 16);
        let mut p = self.pool.lock().unwrap();
        p.next_seq += 1;
        let seq = p.next_seq;
        if let Some(w) = p.workers.iter_mut().find(|w| w.id == id) {
            w.slots = slots;
            write_msg(&w.out, &Msg::Slots { seq, slots });
        }
    }

    /// 停用 / 启用一台 worker。停用只是不再派活，连接保持——
    /// 直接踢掉的话它会 1 秒后重连上来，等于没停。
    pub fn set_paused(&self, id: usize, paused: bool) {
        let mut p = self.pool.lock().unwrap();
        if let Some(w) = p.workers.iter_mut().find(|w| w.id == id) {
            w.paused = paused;
        }
    }

    /// worker 恢复后清掉它的拒绝记录（路径挂载修好了能再次接单）。
    pub fn forget_rejects(&self) {
        let mut p = self.pool.lock().unwrap();
        for w in p.workers.iter_mut() {
            w.rejected.clear();
        }
    }
}

fn view_of(w: &Worker) -> WorkerView {
    WorkerView {
        id: w.id,
        host: w.host.clone(),
        online: w.online,
        slots: w.slots,
        busy: w.busy,
        houdini: w.vers.join(" / "),
        job: w.job.clone(),
        chunk: w.chunk.clone(),
        frame: w.frame,
        mem_mb: w.mem_mb,
        paused: w.paused,
    }
}

pub fn chunk_key(job: &str, chunk: &str) -> String {
    format!("{job}#{chunk}")
}

fn host_of_me() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "master".into())
}

// ───────────────────────── worker 角色 ─────────────────────────

/// worker 主循环：拨号 → 干活 → 断线重连。永不返回。
pub fn run_worker_role(addr: &str, slots: u32) -> ! {
    ensure_console();
    wlog(&format!("[HRQ worker] 连接 {addr}，槽位 {slots}"));
    let mut backoff = 1u64;
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                backoff = 1;
                if let Err(e) = serve_as_worker(stream, slots) {
                    wlog(&format!("[HRQ worker] 连接结束：{e}"));
                }
            }
            Err(e) => wlog(&format!("[HRQ worker] 连不上 {addr}：{e}")),
        }
        wlog(&format!("[HRQ worker] {backoff} 秒后重试"));
        thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(15);
    }
}

/// release 构建是 windows 子系统，从 cmd 启动也不会自带控制台。
/// 先试着挂到父进程的控制台，挂不上就自己开一个——否则用户在 worker 机上
/// 双击运行会看到「什么都没发生」。
#[cfg(windows)]
fn ensure_console() {
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn AllocConsole() -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

#[cfg(not(windows))]
fn ensure_console() {}

/// worker 侧日志：既打印，也追加到 `%TEMP%\HoudiniRenderQueue\worker.log`。
/// 没有控制台的时候（双击启动）只能靠这个文件排查。
fn wlog(msg: &str) {
    println!("{msg}");
    let dir = std::env::temp_dir().join("HoudiniRenderQueue");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("worker.log"))
    {
        let _ = writeln!(f, "{}  {msg}", crate::pyproc::now_hms());
    }
}

fn serve_as_worker(stream: TcpStream, slots: u32) -> std::io::Result<()> {
    let read_half = stream.try_clone()?;
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let mut wr = stream;
    thread::spawn(move || {
        for line in out_rx {
            if wr.write_all(line.as_bytes()).is_err() || wr.write_all(b"\n").is_err() {
                break;
            }
            let _ = wr.flush();
        }
    });

    let installs = houdini::detect_installs();
    let vers: Vec<String> = installs.iter().map(|i| i.version.clone()).collect();
    // 主控可以随时调槽数，所以不能是函数参数那种一次性的值
    let slots = Arc::new(AtomicU32::new(slots.max(1)));

    write_msg(
        &out_tx,
        &Msg::Hello {
            seq: 0,
            ver: PROTO,
            host: host_of_me(),
            slots: slots.load(Ordering::SeqCst),
            houdini: vers,
        },
    );

    let cancel_flag = Arc::new(AtomicBool::new(false));
    // 当前在跑的那块，取消时比对 (job, chunk)
    let current: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    // 忙碌块数：assign 时 +1，执行线程退出时 -1
    let busy = Arc::new(AtomicU32::new(0));

    // 心跳线程
    {
        let out = out_tx.clone();
        let cur = current.clone();
        let busy = busy.clone();
        thread::spawn(move || loop {
            thread::sleep(BEAT_EVERY);
            let (j, c) = cur.lock().unwrap().clone();
            let _ = out.send(
                Msg::Beat {
                    busy: busy.load(Ordering::SeqCst),
                    job: j,
                    chunk: c,
                    frame: 0,
                    mem_mb: mem_mb(),
                }
                .to_line(),
            );
        });
    }

    // 首次 claim
    {
        let b = busy.load(Ordering::SeqCst);
        write_msg(
            &out_tx,
            &Msg::Claim {
                seq: 0,
                busy: b,
                free: slots.load(Ordering::SeqCst).saturating_sub(b),
            },
        );
    }

    let reader = BufReader::new(read_half);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Msg>(&line) else {
            continue;
        };
        match msg {
            Msg::Welcome { proto, .. } => {
                if proto != PROTO {
                    wlog(&format!("[HRQ worker] 主控协议 {proto}，本机 {PROTO}，退出"));
                    std::process::exit(2);
                }
                wlog("[HRQ worker] 已接入主控");
            }
            Msg::Assign {
                seq: _,
                job,
                chunk,
                hip,
                node,
                houdini,
                output: _,
                frames,
            } => {
                // 三件事任一不满足就 reject，让主控换台机器——别等渲到一半才炸
                let Some(inst) = pick_install(&installs, &houdini) else {
                    write_msg(
                        &out_tx,
                        &Msg::Reject {
                            seq: 0,
                            job,
                            chunk,
                            why: format!("本机没有 Houdini {houdini}"),
                        },
                    );
                    continue;
                };
                let hip_path = PathBuf::from(&hip);
                if !hip_path.exists() {
                    write_msg(
                        &out_tx,
                        &Msg::Reject {
                            seq: 0,
                            job: job.clone(),
                            chunk: chunk.clone(),
                            why: format!("工程不可见：{hip}"),
                        },
                    );
                    continue;
                }

                busy.fetch_add(1, Ordering::SeqCst);
                *current.lock().unwrap() = (job.clone(), chunk.clone());
                cancel_flag.store(false, Ordering::SeqCst);
                write_msg(
                    &out_tx,
                    &Msg::Accept {
                        seq: 0,
                        job: job.clone(),
                        chunk: chunk.clone(),
                    },
                );

                let out = out_tx.clone();
                let cancel = cancel_flag.clone();
                let cur = current.clone();
                let busy2 = busy.clone();
                let slots2 = slots.clone();
                let hython = inst.hython.clone();
                thread::spawn(move || {
                    let r = exec_chunk(&hython, &hip_path, &node, &frames, &job, &chunk, &out, &cancel);
                    write_msg(
                        &out,
                        &Msg::Result {
                            job: job.clone(),
                            chunk: chunk.clone(),
                            kind: r.kind,
                            detail: r.detail,
                            code: r.code,
                            boot_ms: r.boot_ms,
                            frame_ms: r.frame_ms,
                        },
                    );
                    // 清掉当前块，让心跳不再报它
                    let mut c = cur.lock().unwrap();
                    if c.1 == chunk {
                        c.0.clear();
                        c.1.clear();
                    }
                    drop(c);
                    let b = busy2.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
                    // 主动报到：主控靠这一句立刻知道「我又空出来了」，
                    // 不用等下一次心跳（5 秒）才发现
                    write_msg(
                        &out,
                        &Msg::Claim {
                            seq: 0,
                            busy: b,
                            free: slots2.load(Ordering::SeqCst).saturating_sub(b),
                        },
                    );
                });
            }
            Msg::Cancel { job, chunk } => {
                let cur = current.lock().unwrap();
                if cur.0 == job && cur.1 == chunk {
                    cancel_flag.store(true, Ordering::SeqCst);
                }
            }
            Msg::Slots { slots: n, .. } => {
                slots.store(n.clamp(1, 16), Ordering::SeqCst);
                wlog(&format!("[HRQ worker] 并发槽调整为 {n}"));
                let b = busy.load(Ordering::SeqCst);
                write_msg(
                    &out_tx,
                    &Msg::Claim {
                        seq: 0,
                        busy: b,
                        free: slots.load(Ordering::SeqCst).saturating_sub(b),
                    },
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn pick_install<'a>(
    installs: &'a [houdini::HoudiniInstall],
    want: &str,
) -> Option<&'a houdini::HoudiniInstall> {
    if installs.is_empty() {
        return None;
    }
    // HRQ_HYTHON 是用户显式指定的 hython（包装脚本 / 非标准安装 / 测试环境），
    // 它的版本号可能是个自定义标签，但意图很明确：就用这个。优先于版本匹配。
    if let Some(custom) = installs.iter().find(|i| i.version.starts_with("自定义")) {
        return Some(custom);
    }
    if want.is_empty() {
        return installs.first();
    }
    installs
        .iter()
        .find(|i| i.version == want)
        .or_else(|| installs.iter().find(|i| same_minor(&i.version, want)))
        .or_else(|| {
            // 只有一台机器装了单一版本时，版本号对不上也别硬拦——很多人的目录名不规范
            if installs.len() == 1 {
                installs.first()
            } else {
                None
            }
        })
}

/// 版本串必须是纯数字段组成的（`21.0.596`）。带 `-fake`、`beta` 之类后缀的一律不认，
/// 否则 `20.5.684-fake` 会被同主次版本规则匹配到真实的 `20.5.684` 上去。
fn is_clean_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn same_minor(a: &str, b: &str) -> bool {
    if !is_clean_version(a) || !is_clean_version(b) {
        return false;
    }
    let ka: Vec<&str> = a.split('.').take(2).collect();
    let kb: Vec<&str> = b.split('.').take(2).collect();
    ka == kb && ka.len() >= 2
}

/// 一块的执行结果 + 实测耗时（主控拿它算分片大小）。
struct ChunkOut {
    kind: String,
    detail: String,
    code: i32,
    /// 起进程 → `##READY`。这一段是纯开销，跟帧数无关
    boot_ms: u64,
    /// 该块平均单帧耗时
    frame_ms: u64,
}

impl ChunkOut {
    fn end(kind: &str, detail: &str, code: i32) -> ChunkOut {
        ChunkOut {
            kind: kind.into(),
            detail: detail.into(),
            code,
            boot_ms: 0,
            frame_ms: 0,
        }
    }
    fn fail(detail: impl Into<String>) -> ChunkOut {
        ChunkOut::end("fail", &detail.into(), 3)
    }
}

#[allow(clippy::too_many_arguments)]
fn exec_chunk(
    hython: &std::path::Path,
    hip: &std::path::Path,
    node: &str,
    frames: &[i32],
    job: &str,
    chunk: &str,
    out: &Sender<String>,
    cancel: &Arc<AtomicBool>,
) -> ChunkOut {
    let script = pyproc::script_path("render.py");
    if !script.exists() {
        return ChunkOut::fail(format!("找不到 render.py: {}", script.display()));
    }

    // 块文件放**本机**临时目录，文件名带块标识：同一台机器上跑同一 job 的两个块不能撞名
    let dir = std::env::temp_dir().join("HoudiniRenderQueue").join("remote");
    let _ = std::fs::create_dir_all(&dir);
    let safe = safe_name(&format!("{job}#{chunk}"));
    let plan = dir.join(format!("{safe}.plan.json"));
    let cancel_file = dir.join(format!("{safe}.cancel"));
    let _ = std::fs::remove_file(&cancel_file);

    let json = serde_json::json!({ "frames": frames });
    if let Err(e) = std::fs::write(&plan, json.to_string()) {
        return ChunkOut::fail(format!("写帧计划失败: {e}"));
    }

    let started = Instant::now();
    let mut cmd = pyproc::hidden(hython);
    cmd.arg(&script)
        .arg(hip)
        .arg("--node")
        .arg(node)
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
            let _ = std::fs::remove_file(&plan);
            return ChunkOut::fail(format!(
                "启动 hython 失败: {e}（{}）",
                hython.display()
            ));
        }
    };

    let (ltx, lrx) = mpsc::channel::<String>();
    if let Some(o) = child.stdout.take() {
        let l = ltx.clone();
        thread::spawn(move || {
            for line in BufReader::new(o).lines().map_while(Result::ok) {
                let _ = l.send(line);
            }
        });
    }
    if let Some(e) = child.stderr.take() {
        let l = ltx.clone();
        thread::spawn(move || {
            for line in BufReader::new(e).lines().map_while(Result::ok) {
                let _ = l.send(line);
            }
        });
    }

    let mut ready = false;
    let mut ready_at: Option<Instant> = None;
    let mut done_frames = 0u32;
    let mut last_done: Option<Instant> = None;
    let mut result: Option<ChunkOut> = None;
    let mut cancel_at: Option<Instant> = None;
    let mut killed = false;

    loop {
        while let Ok(line) = lrx.try_recv() {
            let l = line.trim_end();
            if l.is_empty() {
                continue;
            }
            // 只把标记行回传主控，其余（启动日志、warnings）留在本地控制台，
            // 一个 IFD 任务能吐几千行，全塞回网络会把进度挤掉
            if l.starts_with("##") {
                let _ = out.send(
                    Msg::Line {
                        job: job.to_string(),
                        chunk: chunk.to_string(),
                        data: l.to_string(),
                    }
                    .to_line(),
                );
                let rest = &l[2..];
                let (tag, arg) = match rest.split_once(' ') {
                    Some((t, a)) => (t, a.trim()),
                    None => (rest, ""),
                };
                match tag {
                    "READY" => {
                        ready = true;
                        ready_at.get_or_insert_with(Instant::now);
                    }
                    "DONE" => {
                        done_frames += 1;
                        last_done = Some(Instant::now());
                    }
                    "PREFLIGHT" => result = Some(ChunkOut::end("preflight", arg, 2)),
                    "FAIL" => result = Some(ChunkOut::end("fail", arg, 3)),
                    "CANCELED" => result = Some(ChunkOut::end("canceled", "", 4)),
                    _ => {}
                }
            } else {
                wlog(&format!("[HRQ worker] {l}"));
            }
        }

        let exited = match child.try_wait() {
            Ok(Some(st)) => {
                if result.is_none() {
                    result = Some(if st.success() {
                        ChunkOut::end("ok", "", 0)
                    } else {
                        ChunkOut::fail(format!("hython 退出码 {:?}", st.code()))
                    });
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                result = Some(ChunkOut::fail(format!("等待进程失败: {e}")));
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
        }
        if let Some(t) = cancel_at {
            if t.elapsed() > Duration::from_secs(20) && !killed {
                let _ = child.kill();
                killed = true;
                result = Some(ChunkOut::end("canceled", "", 4));
            }
        }
        if !ready && result.is_none() && started.elapsed() > Duration::from_secs(300) {
            let _ = child.kill();
            result = Some(ChunkOut::fail(
                "启动超时 5 分钟（许可证被占用，或工程过大）",
            ));
        }

        thread::sleep(Duration::from_millis(120));
    }

    let _ = std::fs::remove_file(&plan);
    let _ = std::fs::remove_file(&cancel_file);
    let mut r = result.unwrap_or_else(|| ChunkOut::fail("未知结束状态"));
    if let Some(t) = ready_at {
        r.boot_ms = (t - started).as_millis() as u64;
        if let Some(last) = last_done {
            if done_frames > 0 {
                r.frame_ms = ((last - t).as_millis() as u64 / done_frames as u64).max(1);
            }
        }
    }
    r
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c == '*' || c == '?' || c == '"' || c == '<'
                || c == '>' || c == '|'
            {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn mem_mb() -> u64 {
    // 不引第三方 crate：拿不到就报 0，设备面板只是参考
    0
}

// ───────────────────────── 单测 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_roundtrip_assign() {
        let m = Msg::Assign {
            seq: 7,
            job: "sh010:/out/render".into(),
            chunk: "1-20".into(),
            hip: r"\\NAS\proj\sh010.hip".into(),
            node: "/out/render".into(),
            houdini: "21.0.596".into(),
            output: r"\\NAS\proj\render\${F}.exr".into(),
            frames: vec![1, 2, 3],
        };
        let line = m.to_line();
        assert!(line.contains("\"t\":\"assign\""));
        let back: Msg = serde_json::from_str(&line).unwrap();
        match back {
            Msg::Assign { chunk, frames, .. } => {
                assert_eq!(chunk, "1-20");
                assert_eq!(frames, vec![1, 2, 3]);
            }
            _ => panic!("变体错了"),
        }
    }

    #[test]
    fn msg_roundtrip_hello_has_versions() {
        let m = Msg::Hello {
            seq: 1,
            ver: PROTO,
            host: "WS-02".into(),
            slots: 2,
            houdini: vec!["21.0.596".into(), "20.5.445".into()],
        };
        let back: Msg = serde_json::from_str(&m.to_line()).unwrap();
        match back {
            Msg::Hello { slots, houdini, .. } => {
                assert_eq!(slots, 2);
                assert_eq!(houdini.len(), 2);
            }
            _ => panic!("变体错了"),
        }
    }

    #[test]
    fn done_kind_parse() {
        assert_eq!(DoneKind::parse("ok"), DoneKind::Ok);
        assert_eq!(DoneKind::parse("preflight"), DoneKind::Preflight);
        assert_eq!(DoneKind::parse("canceled"), DoneKind::Canceled);
        assert_eq!(DoneKind::parse("fail"), DoneKind::Fail);
        assert_eq!(DoneKind::parse("什么鬼"), DoneKind::Fail);
    }

    #[test]
    fn same_minor_matches() {
        assert!(same_minor("21.0.596", "21.0.673"));
        assert!(!same_minor("21.0.596", "20.5.445"));
        assert!(!same_minor("21", "21.0.596"));
    }

    #[test]
    fn same_minor_rejects_dirty_versions() {
        // 带后缀的版本号不能按同主次版本兜底，否则 fake 版本会命中真实安装
        assert!(!same_minor("20.5.684", "20.5.684-fake"));
        assert!(!is_clean_version("20.5.684-fake"));
        assert!(is_clean_version("21.0.596"));
        assert!(!is_clean_version("自定义1"));
    }

    #[test]
    fn safe_name_strips_path_chars() {
        assert_eq!(safe_name("a/b:c#1-2"), "a_b_c#1-2");
    }

    #[test]
    fn chunk_key_distinct() {
        assert_ne!(chunk_key("j", "1-2"), chunk_key("j", "3-4"));
        assert_eq!(chunk_key("j", "1-2"), "j#1-2");
    }

    /// 假 worker：报完 Hello 就一直挂着读，不回应 assign。
    fn fake_worker(port: u16, slots: u32) {
        thread::spawn(move || {
            let Ok(s) = TcpStream::connect(("127.0.0.1", port)) else {
                return;
            };
            let mut w = s.try_clone().unwrap();
            let hello = Msg::Hello {
                seq: 0,
                ver: PROTO,
                host: "TEST-WS".into(),
                slots,
                houdini: vec!["21.0.596".into()],
            };
            let _ = writeln!(w, "{}", hello.to_line());
            let _ = w.flush();
            // 挂着直到连接断掉：连接一断主控会把这台标成离线，测的就不是停用了
            for _l in BufReader::new(s).lines().map_while(Result::ok) {}
        });
    }

    /// 停用与「失败换机」都需要真实连接才能验，这里用一台假 worker 走完整握手。
    #[test]
    fn paused_and_excluded_workers_get_no_work() {
        let port = 17891;
        let Ok((hub, rx)) = Hub::start(port) else {
            eprintln!("端口 {port} 被占用，跳过");
            return;
        };
        fake_worker(port, 2);

        let mut id = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(HubEvent::Online(v)) = rx.recv_timeout(Duration::from_millis(200)) {
                id = v.id;
                break;
            }
        }
        assert!(id > 0, "假 worker 没接上");

        let req = |chunk: &str| AssignReq {
            job: "j".into(),
            chunk: chunk.into(),
            hip: "h.hip".into(),
            node: "/out/render".into(),
            houdini: String::new(),
            output: "/o.$F.exr".into(),
            frames: vec![1],
            exclude: Vec::new(),
        };

        assert!(hub.dispatch(&req("1-1")).is_some(), "空闲时应该能派");
        assert_eq!(hub.idle_slots(), 1);

        // 失败换机：把这块从这台机器上排除
        let mut r2 = req("2-2");
        r2.exclude = vec![id];
        assert!(hub.dispatch(&r2).is_none(), "exclude 应该跳过这台");

        // 停用：连接保持，但不派新活（直接踢掉的话它 1 秒后会重连回来）
        hub.set_paused(id, true);
        assert_eq!(hub.idle_slots(), 0, "停用的机器不该算进空闲槽");
        assert!(hub.dispatch(&req("3-3")).is_none(), "停用后不该再接活");

        hub.set_paused(id, false);
        assert!(hub.dispatch(&req("4-4")).is_some(), "启用后应该恢复接活");
    }
}
