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

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
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
///
/// v2 起带上 `token`（防误连）与 `resume`（主控重启后接管在跑的块），
/// 与 v1 的 worker 互不兼容——两端 exe 必须同版本。
pub const PROTO: u32 = 2;

/// 默认监听端口（可在 `--listen` 覆盖）。
pub const DEFAULT_PORT: u16 = 7878;

/// 自动发现的广播端口。worker 用 `--connect auto` 时监听这个端口等主控点名。
pub const DISCOVERY_PORT: u16 = 7877;

/// 心跳间隔 / 判定离线阈值 / 广播间隔。
const BEAT_EVERY: Duration = Duration::from_secs(5);
const OFFLINE_AFTER: Duration = Duration::from_secs(15);
const BEACON_EVERY: Duration = Duration::from_secs(2);

// ───────────────────────── 协议 ─────────────────────────

/// 主控重启后，worker 把自己手上还在跑的块报上来，让新主控接住这些帧。
/// 没有这一步的话，重连期间渲完的帧会被新主控当成「还没人做」再派一遍。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResumeChunk {
    pub job: String,
    pub chunk: String,
    #[serde(default)]
    pub frames: Vec<i32>,
}

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
        /// 接入口令。主控没设口令时随便填（老行为），设了就逐字比对
        #[serde(default)]
        token: String,
        /// 本机还在跑的块（断线重连 / 主控重启后重连时非空）
        #[serde(default)]
        resume: Vec<ResumeChunk>,
    },
    /// master → worker：接受报到
    Welcome {
        seq: u64,
        master: String,
        proto: u32,
        poll_ms: u64,
        /// 主控接管了几块（仅供参考，worker 不用管）
        #[serde(default)]
        resumed: u32,
    },
    /// master → worker：不让你进（协议版本 / 口令不对）。worker 收到就停下，别无限重连
    Denied { seq: u64, why: String },
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
    /// 主控重启 / 重连后，worker 交回它在跑的块：这些帧仍然是它的，
    /// 引擎要把它们记进 `remote`，否则会本地再渲一遍（两个进程抢同一帧的输出）
    Resumed {
        job: String,
        chunk: String,
        frames: Vec<i32>,
        host: String,
    },
    /// 多机被关掉（UI 开关）：所有在跑的远端块作废，帧退回队列
    Stopped,
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
    /// 本次不要派给这些机器（刚在这台上失败过）。空 = 不限制。
    /// 用**主机名**而不是连接 id：worker 断线重连会拿到新 id，按 id 记的话
    /// 「换机重试」在它重连后就失效了。
    pub exclude: Vec<String>,
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
    /// 接入口令。空串 = 不校验（只在内网调试时这么用）
    token: String,
    stop: Arc<AtomicBool>,
}

impl Hub {
    /// 起监听。返回的 Receiver 必须交给引擎线程独占消费。
    ///
    /// `token` 为空表示不校验口令（内网调试 / 单测）；界面里启动时会自动生成一个。
    pub fn start(port: u16, token: &str) -> std::io::Result<(Arc<Hub>, Receiver<HubEvent>)> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        // 非阻塞 + 轮询：为了能在运行时被 UI 关掉（阻塞在 incoming() 里没有干净的退出方式）
        listener.set_nonblocking(true)?;
        let (tx, rx) = mpsc::channel::<HubEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub {
            pool: Mutex::new(Pool {
                workers: Vec::new(),
                next_seq: 1,
            }),
            tx: tx.clone(),
            port,
            token: token.to_string(),
            stop: stop.clone(),
        });

        // accept 循环
        {
            let hub = hub.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                loop {
                    if hub.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_nodelay(true);
                            let hub = hub.clone();
                            let tx = tx.clone();
                            thread::spawn(move || hub.serve_conn(stream, tx));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(60));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(200)),
                    }
                }
                drop(listener);
            });
        }

        // 心跳超时巡检
        {
            let hub = hub.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                while !hub.stop.load(Ordering::SeqCst) {
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
                    drop(p);
                    if changed {
                        let _ = tx.send(HubEvent::Changed);
                    }
                }
            });
        }

        // 广播自己，让 worker 能 `--connect auto` 摸过来。只报端口和主机名，
        // **不带口令**——广播是明文且谁都能收，口令得让人手抄过去。
        {
            let hub = hub.clone();
            thread::spawn(move || {
                let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else {
                    return;
                };
                let _ = sock.set_broadcast(true);
                let beacon = Beacon {
                    hrq: PROTO,
                    port,
                    master: host_of_me(),
                    need_token: !hub.token.is_empty(),
                };
                let line = serde_json::to_vec(&beacon).unwrap_or_default();
                let targets = [
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DISCOVERY_PORT),
                ];
                while !hub.stop.load(Ordering::SeqCst) {
                    for t in &targets {
                        let _ = sock.send_to(&line, t);
                    }
                    thread::sleep(BEACON_EVERY);
                }
            });
        }

        let _ = tx.send(HubEvent::Log(format!(
            "多机执行已监听 0.0.0.0:{}（worker 运行：--worker --connect <本机IP>:{} --token <口令>）",
            port, port
        )));
        Ok((hub, rx))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 给界面显示的一行接入命令（含本机局域网 IP 与口令）。
    pub fn join_hint(&self) -> String {
        match lan_ip() {
            Some(ip) => format!(
                "--worker --connect {ip}:{} --token {}",
                self.port,
                if self.token.is_empty() { "（无）" } else { &self.token }
            ),
            None => format!(
                "--worker --connect <本机IP>:{} --token {}",
                self.port,
                if self.token.is_empty() { "（无）" } else { &self.token }
            ),
        }
    }

    /// 关掉多机：连接断开、在跑的块交回引擎退回队列。UI 开关用。
    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return; // 已经在停了
        }
        let mut p = self.pool.lock().unwrap();
        for w in p.workers.iter_mut() {
            let lost = std::mem::take(&mut w.inflight);
            w.online = false;
            w.busy = 0;
            if !lost.is_empty() {
                let _ = self.tx.send(HubEvent::Offline {
                    worker: w.id,
                    host: w.host.clone(),
                    running: lost,
                });
            }
        }
        p.workers.clear();
        drop(p);
        let _ = self.tx.send(HubEvent::Stopped);
        let _ = self.tx.send(HubEvent::Log("多机执行已关闭".into()));
        let _ = self.tx.send(HubEvent::Changed);
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
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
                    token,
                    resume,
                } => {
                    host = h;
                    if ver != PROTO {
                        let _ = tx.send(HubEvent::Log(format!(
                            "{host} 协议版本 {ver}，主控是 {PROTO}，拒绝接入（两端 exe 要同版本）"
                        )));
                        write_msg(
                            &out_tx,
                            &Msg::Denied {
                                seq,
                                why: format!("协议版本 {ver} ≠ {PROTO}，两端 exe 要同版本"),
                            },
                        );
                        thread::sleep(Duration::from_millis(200));
                        break;
                    }
                    if !self.token.is_empty() && token != self.token {
                        let _ = tx.send(HubEvent::Log(format!(
                            "{host} 口令不对，拒绝接入（设备面板里有正确口令）"
                        )));
                        write_msg(
                            &out_tx,
                            &Msg::Denied {
                                seq,
                                why: "口令不对，看主控设备面板里的 token".into(),
                            },
                        );
                        thread::sleep(Duration::from_millis(200));
                        break;
                    }
                    let resumed = resume.len() as u32;
                    let mut p = self.pool.lock().unwrap();
                    p.next_seq += 1;
                    let id = p.next_seq as usize;
                    p.workers.push(Worker {
                        id,
                        host: host.clone(),
                        slots: slots.max(1),
                        // 接管来的块算它忙：不然主控会立刻再往这台压一块，
                        // 1 槽机器就同时跑两个 hython
                        busy: resumed,
                        vers: houdini.clone(),
                        out: out_tx.clone(),
                        last_beat: Instant::now(),
                        online: true,
                        paused: false,
                        job: resume.first().map(|r| r.job.clone()).unwrap_or_default(),
                        chunk: resume.first().map(|r| r.chunk.clone()).unwrap_or_default(),
                        frame: 0,
                        mem_mb: 0,
                        rejected: HashSet::new(),
                        inflight: resume
                            .iter()
                            .map(|r| (r.job.clone(), r.chunk.clone()))
                            .collect(),
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
                            resumed,
                        },
                    );
                    let _ = tx.send(HubEvent::Online(wv));
                    let _ = tx.send(HubEvent::Log(format!(
                        "{host} 已接入（{} 个槽位，Houdini {:?}）",
                        slots, houdini
                    )));
                    // 把在跑的块交给引擎，让它把这些帧记成「远端已认领」
                    for r in &resume {
                        if r.frames.is_empty() {
                            continue;
                        }
                        let _ = tx.send(HubEvent::Log(format!(
                            "{host} 接着跑 {job}[{chunk}]（{} 帧，主控接管）",
                            r.frames.len(),
                            job = r.job,
                            chunk = r.chunk
                        )));
                        let _ = tx.send(HubEvent::Resumed {
                            job: r.job.clone(),
                            chunk: r.chunk.clone(),
                            frames: r.frames.clone(),
                            host: host.clone(),
                        });
                    }
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
        let idx = candidate_indices(&p, req).into_iter().next()?;

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

    /// 按派发优先顺序列出此刻能接活的机器。引擎拿它算「这一块切多大」：
    /// 第 0 台正是 `dispatch` 会选中的那台，所以用它的权重算份额才准。
    pub fn idle_hosts_for(&self, req: &AssignReq) -> Vec<String> {
        let p = self.pool.lock().unwrap();
        candidate_indices(&p, req)
            .into_iter()
            .map(|i| p.workers[i].host.clone())
            .collect()
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

/// 能接这块的机器（按优先顺序：版本对得上的排前面）。
///
/// 版本最终由 worker 自己判断（找不到匹配的 hython 会 reject），这里硬过滤会让
/// 「工程记录的版本号跟本机目录名不一致」的情况永远派不出去，所以只是**排序**，不排除。
fn candidate_indices(p: &Pool, req: &AssignReq) -> Vec<usize> {
    let key = chunk_key(&req.job, &req.chunk);
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    for (i, w) in p.workers.iter().enumerate() {
        if !w.online
            || w.paused
            || w.busy >= w.slots
            || w.rejected.contains(&key)
            || req.exclude.iter().any(|h| h == &w.host)
        {
            continue;
        }
        let ver_ok =
            req.houdini.is_empty() || w.vers.is_empty() || w.vers.iter().any(|v| v == &req.houdini);
        if ver_ok {
            exact.push(i);
        } else {
            loose.push(i);
        }
    }
    exact.extend(loose);
    exact
}

fn view_of(w: &Worker) -> WorkerView {    WorkerView {
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

/// 主控的广播包。只报「我在哪」，口令得人手抄——广播是谁都能收的明文。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Beacon {
    /// 协议版本，同时当魔数用（收到别的版本直接无视，握手也会被拒）
    pub hrq: u32,
    pub port: u16,
    pub master: String,
    #[serde(default)]
    pub need_token: bool,
}

/// 本机在局域网里的地址。不引第三方 crate：借一次 UDP connect 让系统选路由。
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    // 不会真的发包，只是让内核挑一张出口网卡
    s.connect(("8.8.8.8", 80)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

/// 当前连接。执行线程拿着它，主控重启 / 断线重连后消息自动走新连接——
/// 否则老连接上跑着的块渲完了也送不回去，那些帧等于白跑。
///
/// 断线期间（`cur` 为空或写通道已关）消息先压在 `pending` 里，
/// 重连后一次性补发。不这么做的话，主控重启那十几秒里渲完的帧不会被记账，
/// 重连后会被当成「没做过」再渲一遍——活白干，输出文件还要互相覆盖。
#[derive(Default)]
struct Link {
    cur: Mutex<Option<Sender<String>>>,
    pending: Mutex<Vec<String>>,
}

/// 断线缓冲上限。一个块最多几百帧的 `##DONE`，超了就是真连不上了，丢掉更省事。
const PENDING_MAX: usize = 600;

impl Link {
    fn bind(&self, tx: Sender<String>) {
        let mut cur = self.cur.lock().unwrap();
        *cur = Some(tx);
        let mut pending = self.pending.lock().unwrap();
        if let Some(tx) = cur.as_ref() {
            for line in pending.drain(..) {
                let _ = tx.send(line);
            }
        }
    }

    fn send(&self, m: &Msg) {
        let line = m.to_line();
        let mut cur = self.cur.lock().unwrap();
        if let Some(tx) = cur.as_ref() {
            if tx.send(line.clone()).is_ok() {
                return;
            }
            *cur = None; // 写线程已经走了，走缓冲
        }
        let mut pending = self.pending.lock().unwrap();
        if pending.len() < PENDING_MAX {
            pending.push(line);
        }
    }
}

/// 本机在跑的块 → 帧列表。跨连接保留：重连时整份报给主控接管。
type Running = Arc<Mutex<HashMap<(String, String), Vec<i32>>>>;

/// worker 主循环：拨号 → 干活 → 断线重连。永不返回。
///
/// `target` 为 `auto` / 空 / `discover` 时先广播找主控；找到后一直用它，
/// 连不上再重新找（主控换了 IP 也能自己回来）。
pub fn run_worker_role(target: &str, slots: u32, token: &str) -> ! {
    ensure_console();
    let link: Arc<Link> = Arc::new(Link::default());
    let running: Running = Arc::new(Mutex::new(HashMap::new()));
    let auto = target.is_empty() || target == "auto" || target == "discover";
    let mut addr = if auto { String::new() } else { target.to_string() };
    wlog(&format!(
        "[HRQ worker] 槽位 {slots}，主控 {}，口令 {}",
        if auto { "自动发现".to_string() } else { addr.clone() },
        if token.is_empty() { "（无）" } else { "已设置" }
    ));

    let mut backoff = 1u64;
    loop {
        if addr.is_empty() {
            wlog("[HRQ worker] 正在局域网里找主控…");
            match discover_master(Duration::from_secs(10)) {
                Some((a, master)) => {
                    wlog(&format!("[HRQ worker] 找到主控 {master}（{a}）"));
                    addr = a;
                }
                None => {
                    wlog(
                        "[HRQ worker] 没找到主控。确认主控已开启多机；或改用 --connect <ip:端口>",
                    );
                    thread::sleep(Duration::from_secs(3));
                    continue;
                }
            }
        }
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                backoff = 1;
                if let Err(e) = serve_as_worker(stream, slots, token, &link, &running) {
                    wlog(&format!("[HRQ worker] 连接结束：{e}"));
                }
            }
            Err(e) => wlog(&format!("[HRQ worker] 连不上 {addr}：{e}")),
        }
        wlog(&format!("[HRQ worker] {backoff} 秒后重试"));
        thread::sleep(Duration::from_secs(backoff));
        // 上限 5 秒：主控重启通常几秒就回来，退避到 15 秒会让「重启后接管」
        // 这段窗口白白错过（块都跑完了才连上）
        backoff = (backoff * 2).min(5);
    }
}

/// 等一个主控广播。返回 (地址, 主机名)。
fn discover_master(wait: Duration) -> Option<(String, String)> {
    let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(e) => {
            wlog(&format!(
                "[HRQ worker] 监听发现端口 {DISCOVERY_PORT} 失败：{e}（同机多开时只能手动 --connect）"
            ));
            return None;
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else {
            continue;
        };
        if b.hrq != PROTO {
            wlog(&format!(
                "[HRQ worker] 收到协议 {} 的主控广播，无视（本机 {PROTO}）",
                b.hrq
            ));
            continue;
        }
        let ip = from.ip();
        if ip.is_unspecified() {
            continue;
        }
        return Some((format!("{ip}:{}", b.port), b.master));
    }
    None
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

fn serve_as_worker(
    stream: TcpStream,
    slots: u32,
    token: &str,
    link: &Arc<Link>,
    running: &Running,
) -> std::io::Result<()> {
    let read_half = stream.try_clone()?;
    let (out_tx, out_rx) = mpsc::channel::<String>();
    // 换成本次连接的写通道：之后执行线程的回报都从这个口走
    link.bind(out_tx.clone());
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

    // 重连时把还在跑的块报上去，新主控才能接管这些帧
    let resume: Vec<ResumeChunk> = running
        .lock()
        .unwrap()
        .iter()
        .map(|((j, c), f)| ResumeChunk {
            job: j.clone(),
            chunk: c.clone(),
            frames: f.clone(),
        })
        .collect();
    write_msg(
        &out_tx,
        &Msg::Hello {
            seq: 0,
            ver: PROTO,
            host: host_of_me(),
            slots: slots.load(Ordering::SeqCst),
            houdini: vers,
            token: token.to_string(),
            resume,
        },
    );

    let cancel_flag = Arc::new(AtomicBool::new(false));
    // 当前在跑的那块，取消时比对 (job, chunk)
    let current: Arc<Mutex<(String, String)>> = Arc::new(Mutex::new((String::new(), String::new())));
    // 忙碌块数：assign 时 +1，执行线程退出时 -1
    let busy = Arc::new(AtomicU32::new(0));

    // 心跳线程
    {
        let link = link.clone();
        let cur = current.clone();
        let busy = busy.clone();
        thread::spawn(move || loop {
            thread::sleep(BEAT_EVERY);
            let (j, c) = cur.lock().unwrap().clone();
            link.send(&Msg::Beat {
                busy: busy.load(Ordering::SeqCst),
                job: j,
                chunk: c,
                frame: 0,
                mem_mb: mem_mb(),
            });
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
            Msg::Welcome {
                proto, resumed, ..
            } => {
                if proto != PROTO {
                    wlog(&format!("[HRQ worker] 主控协议 {proto}，本机 {PROTO}，退出"));
                    std::process::exit(2);
                }
                if resumed > 0 {
                    wlog(&format!("[HRQ worker] 已接入主控，接管 {resumed} 块"));
                } else {
                    wlog("[HRQ worker] 已接入主控");
                }
            }
            Msg::Denied { why, .. } => {
                // 版本/口令不对重连一万次也没用，直接停，别让用户以为在跑
                wlog(&format!("[HRQ worker] 主控拒绝接入：{why}"));
                thread::sleep(Duration::from_millis(300));
                std::process::exit(3);
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
                running
                    .lock()
                    .unwrap()
                    .insert((job.clone(), chunk.clone()), frames.clone());
                cancel_flag.store(false, Ordering::SeqCst);
                write_msg(
                    &out_tx,
                    &Msg::Accept {
                        seq: 0,
                        job: job.clone(),
                        chunk: chunk.clone(),
                    },
                );

                let link2 = link.clone();
                let cancel = cancel_flag.clone();
                let cur = current.clone();
                let busy2 = busy.clone();
                let slots2 = slots.clone();
                let running2 = running.clone();
                let hython = inst.hython.clone();
                thread::spawn(move || {
                    let r = exec_chunk(
                        &hython,
                        &hip_path,
                        &node,
                        &frames,
                        &job,
                        &chunk,
                        &link2,
                        &cancel,
                    );
                    link2.send(&Msg::Result {
                        job: job.clone(),
                        chunk: chunk.clone(),
                        kind: r.kind,
                        detail: r.detail,
                        code: r.code,
                        boot_ms: r.boot_ms,
                        frame_ms: r.frame_ms,
                    });
                    running2.lock().unwrap().remove(&(job, chunk.clone()));
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
                    link2.send(&Msg::Claim {
                        seq: 0,
                        busy: b,
                        free: slots2.load(Ordering::SeqCst).saturating_sub(b),
                    });
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
    link: &Arc<Link>,
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
                link.send(&Msg::Line {
                    job: job.to_string(),
                    chunk: chunk.to_string(),
                    data: l.to_string(),
                });
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
            token: "abc123".into(),
            resume: vec![ResumeChunk {
                job: "j1".into(),
                chunk: "1-10".into(),
                frames: (1..=10).collect(),
            }],
        };
        let back: Msg = serde_json::from_str(&m.to_line()).unwrap();
        match back {
            Msg::Hello {
                slots,
                houdini,
                token,
                resume,
                ..
            } => {
                assert_eq!(slots, 2);
                assert_eq!(houdini.len(), 2);
                assert_eq!(token, "abc123");
                assert_eq!(resume.len(), 1);
                assert_eq!(resume[0].frames.len(), 10);
            }
            _ => panic!("变体错了"),
        }
    }

    /// v1 的 worker 不带 token/resume 字段，解析不能炸（`#[serde(default)]` 的意义）。
    #[test]
    fn hello_without_new_fields_still_parses() {
        let line = r#"{"t":"hello","seq":0,"ver":1,"host":"OLD","slots":1,"houdini":[]}"#;
        match serde_json::from_str::<Msg>(line).unwrap() {
            Msg::Hello { token, resume, .. } => {
                assert!(token.is_empty());
                assert!(resume.is_empty());
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
    fn fake_worker(port: u16, slots: u32, token: &str) {
        let token = token.to_string();
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
                token,
                resume: Vec::new(),
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
        let Ok((hub, rx)) = Hub::start(port, "") else {
            eprintln!("端口 {port} 被占用，跳过");
            return;
        };
        fake_worker(port, 2, "");

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

        // 失败换机：按主机名排除这台
        let mut r2 = req("2-2");
        r2.exclude = vec!["TEST-WS".into()];
        assert!(hub.dispatch(&r2).is_none(), "exclude 应该跳过这台");
        assert!(
            hub.idle_hosts_for(&r2).is_empty(),
            "被排除的机器不该出现在权重列表里"
        );

        // 停用：连接保持，但不派新活（直接踢掉的话它 1 秒后会重连回来）
        hub.set_paused(id, true);
        assert_eq!(hub.idle_slots(), 0, "停用的机器不该算进空闲槽");
        assert!(hub.dispatch(&req("3-3")).is_none(), "停用后不该再接活");

        hub.set_paused(id, false);
        assert!(hub.dispatch(&req("4-4")).is_some(), "启用后应该恢复接活");
    }

    /// 口令不对必须当场拒绝，不能让它连上来白占一个槽。
    #[test]
    fn wrong_token_is_denied() {
        let port = 17892;
        let Ok((hub, rx)) = Hub::start(port, "right-token") else {
            eprintln!("端口 {port} 被占用，跳过");
            return;
        };
        fake_worker(port, 1, "wrong-token");

        // 期望：先收到 Log（拒绝），而不是 Online
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut denied = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(HubEvent::Online(_)) => panic!("口令错了不该接入"),
                Ok(HubEvent::Log(l)) if l.contains("口令不对") => {
                    denied = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(denied, "没看到拒绝日志");
        assert_eq!(hub.online_count(), 0);
    }

    /// 主控重启后，worker 带着 `resume` 重连：主控要把这些块当作「远端已认领」，
    /// 否则引擎会本地重跑这些帧，两个进程抢同一个输出文件。
    #[test]
    fn hello_resume_is_taken_over() {
        let port = 17894;
        let Ok((hub, rx)) = Hub::start(port, "") else {
            eprintln!("端口 {port} 被占用，跳过");
            return;
        };
        thread::spawn(move || {
            let Ok(s) = TcpStream::connect(("127.0.0.1", port)) else {
                return;
            };
            let mut w = s.try_clone().unwrap();
            let hello = Msg::Hello {
                seq: 0,
                ver: PROTO,
                host: "WS-RESTART".into(),
                slots: 1,
                houdini: vec![],
                token: String::new(),
                resume: vec![ResumeChunk {
                    job: "j1".into(),
                    chunk: "1-40".into(),
                    frames: (1..=40).collect(),
                }],
            };
            let _ = writeln!(w, "{}", hello.to_line());
            let _ = w.flush();
            for _l in BufReader::new(s).lines().map_while(Result::ok) {}
        });

        let mut resumed: Option<(String, String, Vec<i32>)> = None;
        let mut online_busy = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && resumed.is_none() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(HubEvent::Resumed {
                    job,
                    chunk,
                    frames,
                    ..
                }) => resumed = Some((job, chunk, frames)),
                Ok(HubEvent::Online(v)) => online_busy = v.busy,
                _ => {}
            }
        }
        let (job, chunk, frames) = resumed.expect("没收到 Resumed");
        assert_eq!((job.as_str(), chunk.as_str()), ("j1", "1-40"));
        assert_eq!(frames.len(), 40);
        assert_eq!(online_busy, 1, "接管来的块要算成忙，否则会被再压一块");
        assert_eq!(hub.idle_slots(), 0);
    }

    /// 断线期间的消息要压进缓冲，重连后补发——主控重启那十几秒里渲完的帧
    /// 全靠这个才不会被重跑。
    #[test]
    fn link_buffers_while_disconnected() {
        let link = Link::default();
        link.send(&Msg::Claim { seq: 1, busy: 0, free: 1 });
        link.send(&Msg::Claim { seq: 2, busy: 1, free: 0 });
        assert_eq!(link.pending.lock().unwrap().len(), 2);

        let (tx, rx) = mpsc::channel::<String>();
        link.bind(tx);
        assert!(link.pending.lock().unwrap().is_empty(), "重连后缓冲要清空");
        let a = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let b = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(a.contains("\"seq\":1"), "{a}");
        assert!(b.contains("\"seq\":2"), "{b}");

        // 连上之后直接走，不再进缓冲
        link.send(&Msg::Claim { seq: 3, busy: 0, free: 1 });
        assert!(link.pending.lock().unwrap().is_empty());
        let c = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(c.contains("\"seq\":3"), "{c}");
    }

    /// 关掉多机后不再接新连接，且把手上的块交回引擎。
    #[test]
    fn stop_hub_returns_inflight() {
        let port = 17893;
        let Ok((hub, rx)) = Hub::start(port, "") else {
            eprintln!("端口 {port} 被占用，跳过");
            return;
        };
        fake_worker(port, 1, "");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(HubEvent::Online(_)) = rx.recv_timeout(Duration::from_millis(200)) {
                break;
            }
        }
        let req = AssignReq {
            job: "j".into(),
            chunk: "1-5".into(),
            hip: "h.hip".into(),
            node: "/out/render".into(),
            houdini: String::new(),
            output: "/o.$F.exr".into(),
            frames: vec![1, 2, 3, 4, 5],
            exclude: Vec::new(),
        };
        assert!(hub.dispatch(&req).is_some());
        hub.stop();

        // 期望：Offline(含 1 块) + Stopped
        let mut running = Vec::new();
        let mut stopped = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !stopped {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(HubEvent::Offline { running: r, .. }) => running = r,
                Ok(HubEvent::Stopped) => stopped = true,
                _ => {}
            }
        }
        assert!(stopped, "没收到 Stopped");
        assert_eq!(running.len(), 1, "在跑的块要交回引擎");
        assert!(!hub.is_running());
    }
}
