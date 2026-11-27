use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use rusqlite::{params, Connection};

use crate::frames::FrameSet;
use crate::model::{Category, JobSpec, JobStatus, NodeInfo, Project};

pub struct Db {
    conn: Connection,
}

pub fn db_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("HoudiniRenderQueue");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("state.db")
}

impl Db {
    pub fn open() -> Result<Db, String> {
        let path = db_path();
        let conn = Connection::open(&path).map_err(|e| format!("打开数据库失败: {e}"))?;
        let db = Db { conn };
        db.init().map_err(|e| format!("初始化数据库失败: {e}"))?;
        Ok(db)
    }

    fn init(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS projects (
                key        TEXT PRIMARY KEY,
                hip        TEXT NOT NULL,
                shot       TEXT NOT NULL,
                houdini    TEXT NOT NULL,
                hython     TEXT NOT NULL,
                nodes_json TEXT NOT NULL,
                gstart     INTEGER NOT NULL,
                gend       INTEGER NOT NULL,
                pinned     INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS jobs (
                id         TEXT PRIMARY KEY,
                ord        INTEGER NOT NULL,
                project_key TEXT NOT NULL,
                hip        TEXT NOT NULL,
                hython     TEXT NOT NULL,
                houdini    TEXT NOT NULL,
                shot       TEXT NOT NULL,
                node_path  TEXT NOT NULL,
                node_type  TEXT NOT NULL,
                category   TEXT NOT NULL,
                output     TEXT NOT NULL,
                fstart     INTEGER NOT NULL,
                fend       INTEGER NOT NULL,
                status     TEXT NOT NULL,
                attempts   INTEGER NOT NULL,
                -- 帧集合的规范写法（1052,1055-1056,1065-1070）；老库补列后为空串，
                -- 读的时候为空就退回 fstart..fend 当连续区间
                frames     TEXT NOT NULL DEFAULT '',
                -- 顺序执行：1 = 不允许多机分片（跨帧依赖的解算序列）
                sequential INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS frames (
                job_id TEXT NOT NULL,
                frame  INTEGER NOT NULL,
                PRIMARY KEY (job_id, frame)
            );
            CREATE TABLE IF NOT EXISTS meta (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );
            "#,
        )?;
        // 老库补列：pinned（用户手动指定版本）是后加的字段
        if !self.has_column("projects", "pinned") {
            let _ = self
                .conn
                .execute_batch("ALTER TABLE projects ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0");
        }
        // 老库补列：jobs.frames（不连续帧集合）也是后加的
        if !self.has_column("jobs", "frames") {
            let _ = self.conn.execute_batch(
                "ALTER TABLE jobs ADD COLUMN frames TEXT NOT NULL DEFAULT ''",
            );
        }
        // 老库补列：jobs.sequential（顺序执行标记）
        if !self.has_column("jobs", "sequential") {
            let _ = self.conn.execute_batch(
                "ALTER TABLE jobs ADD COLUMN sequential INTEGER NOT NULL DEFAULT 0",
            );
        }
        Ok(())
    }

    fn has_column(&self, table: &str, col: &str) -> bool {
        self.conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut s| {
                let names: Vec<String> = s
                    .query_map([], |r| r.get::<_, String>(1))?
                    .flatten()
                    .collect();
                Ok(names.iter().any(|n| n == col))
            })
            .unwrap_or(true)
    }

    // ── 工程 ──

    pub fn save_project(&self, p: &Project) -> Result<(), String> {
        let nodes_json = serde_json::to_string(&p.nodes).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO projects (key, hip, shot, houdini, hython, nodes_json, gstart, gend, pinned)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(key) DO UPDATE SET
                    hip=excluded.hip, shot=excluded.shot, houdini=excluded.houdini,
                    hython=excluded.hython, nodes_json=excluded.nodes_json,
                    gstart=excluded.gstart, gend=excluded.gend, pinned=excluded.pinned",
                params![
                    p.key,
                    p.hip.to_string_lossy(),
                    p.shot,
                    p.houdini,
                    p.hython.to_string_lossy(),
                    nodes_json,
                    p.global.0,
                    p.global.1,
                    p.pinned as i32
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_projects(&self) -> Vec<Project> {
        let mut out = Vec::new();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT key,hip,shot,houdini,hython,nodes_json,gstart,gend,pinned FROM projects")
        else {
            return out;
        };
        let rows = stmt.query_map([], |r| {
            let nodes_json: String = r.get(5)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                nodes_json,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        });
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (key, hip, shot, houdini, hython, nodes_json, gstart, gend, pinned) = row;
                let nodes: Vec<NodeInfo> = serde_json::from_str(&nodes_json).unwrap_or_default();
                let hip = PathBuf::from(hip);
                if !hip.exists() {
                    continue;
                }
                out.push(Project {
                    key,
                    hip,
                    shot,
                    houdini,
                    hython: PathBuf::from(hython),
                    auto_version: None,
                    pinned: pinned != 0,
                    nodes,
                    global: (gstart as i32, gend as i32),
                    loading: false,
                    error: None,
                });
            }
        }
        out
    }

    pub fn delete_project(&self, key: &str) {
        let _ = self
            .conn
            .execute("DELETE FROM projects WHERE key=?1", params![key]);
    }

    // ── 任务 ──

    pub fn save_job(&self, spec: &JobSpec, ord: i64, status: JobStatus, attempts: u32) {
        let _ = self.conn.execute(
            "INSERT INTO jobs (id, ord, project_key, hip, hython, houdini, shot, node_path,
                               node_type, category, output, fstart, fend, status, attempts, frames,
                               sequential)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(id) DO UPDATE SET
                ord=excluded.ord, status=excluded.status, attempts=excluded.attempts,
                fstart=excluded.fstart, fend=excluded.fend, frames=excluded.frames,
                sequential=excluded.sequential",
            params![
                spec.id,
                ord,
                spec.project_key,
                spec.hip.to_string_lossy(),
                spec.hython.to_string_lossy(),
                spec.houdini,
                spec.shot,
                spec.node_path,
                spec.node_type,
                spec.category.as_str(),
                spec.output,
                spec.frames.start(),
                spec.frames.end(),
                status.as_str(),
                attempts,
                spec.frames.label(),
                spec.sequential as i32
            ],
        );
    }

    /// 批量落库：一次事务写完。逐个 save_job 各自提交时，几千个任务要等好几秒
    pub fn save_jobs(&self, jobs: &[(&JobSpec, i64, JobStatus, u32)]) {
        if jobs.is_empty() {
            return;
        }
        let Ok(tx) = self.conn.unchecked_transaction() else {
            return;
        };
        for (spec, ord, status, attempts) in jobs {
            let _ = tx.execute(
                "INSERT INTO jobs (id, ord, project_key, hip, hython, houdini, shot, node_path,
                                   node_type, category, output, fstart, fend, status, attempts, frames,
                                   sequential)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(id) DO UPDATE SET
                    ord=excluded.ord, status=excluded.status, attempts=excluded.attempts,
                    fstart=excluded.fstart, fend=excluded.fend, frames=excluded.frames,
                    sequential=excluded.sequential",
                params![
                    spec.id,
                    ord,
                    spec.project_key,
                    spec.hip.to_string_lossy(),
                    spec.hython.to_string_lossy(),
                    spec.houdini,
                    spec.shot,
                    spec.node_path,
                    spec.node_type,
                    spec.category.as_str(),
                    spec.output,
                    spec.frames.start(),
                    spec.frames.end(),
                    status.as_str(),
                    attempts,
                    spec.frames.label(),
                    spec.sequential as i32
                ],
            );
        }
        let _ = tx.commit();
    }

    pub fn delete_job(&self, id: &str) {
        let _ = self.conn.execute("DELETE FROM jobs WHERE id=?1", params![id]);
        let _ = self
            .conn
            .execute("DELETE FROM frames WHERE job_id=?1", params![id]);
    }

    pub fn mark_frame(&self, id: &str, frame: i32) {
        let _ = self.conn.execute(
            "INSERT OR IGNORE INTO frames (job_id, frame) VALUES (?1,?2)",
            params![id, frame],
        );
    }

    pub fn clear_frames(&self, id: &str) {
        let _ = self
            .conn
            .execute("DELETE FROM frames WHERE job_id=?1", params![id]);
    }

    pub fn load_done_frames(&self) -> HashMap<String, BTreeSet<i32>> {
        let mut map: HashMap<String, BTreeSet<i32>> = HashMap::new();
        let Ok(mut stmt) = self.conn.prepare("SELECT job_id, frame FROM frames") else {
            return map;
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
        }) {
            for (id, f) in rows.flatten() {
                map.entry(id).or_default().insert(f);
            }
        }
        map
    }

    pub fn load_jobs(&self) -> Vec<(JobSpec, JobStatus, u32)> {
        let mut out = Vec::new();
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT id,project_key,hip,hython,houdini,shot,node_path,node_type,category,
                    output,fstart,fend,status,attempts,frames,sequential
             FROM jobs ORDER BY ord ASC",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, i32>(10)?,
                r.get::<_, i32>(11)?,
                r.get::<_, String>(12)?,
                r.get::<_, i64>(13)?,
                r.get::<_, String>(14)?,
                r.get::<_, i64>(15)?,
            ))
        }) {
            for row in rows.flatten() {
                let (
                    id,
                    project_key,
                    hip,
                    hython,
                    houdini,
                    shot,
                    node_path,
                    node_type,
                    category,
                    output,
                    fstart,
                    fend,
                    status,
                    attempts,
                    frames,
                    sequential,
                ) = row;
                // frames 列是后加的：老库里是空串，退回 fstart..fend 当连续区间
                let frames = FrameSet::parse(&frames).unwrap_or_else(|_| FrameSet::span(fstart, fend));
                out.push((
                    JobSpec {
                        id,
                        project_key,
                        hip: PathBuf::from(hip),
                        hython: PathBuf::from(hython),
                        houdini,
                        shot,
                        node_path,
                        node_type,
                        category: Category::parse(&category),
                        output,
                        frames,
                        sequential: sequential != 0,
                    },
                    JobStatus::parse(&status),
                    attempts as u32,
                ));
            }
        }
        out
    }

    // ── 设置 ──

    pub fn set_meta(&self, k: &str, v: &str) {
        let _ = self.conn.execute(
            "INSERT INTO meta (k,v) VALUES (?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![k, v],
        );
    }

    pub fn get_meta(&self, k: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k=?1", params![k], |r| r.get(0))
            .ok()
    }
}
