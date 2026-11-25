use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ───────────────────────── 分类 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Category {
    #[default]
    Cache,
    Render,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Cache => "缓存",
            Category::Render => "渲染",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Cache => "cache",
            Category::Render => "render",
        }
    }
    pub fn parse(s: &str) -> Category {
        if s.eq_ignore_ascii_case("render") {
            Category::Render
        } else {
            Category::Cache
        }
    }
}

// ───────────────────────── 任务状态 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum JobStatus {
    #[default]
    Queued,
    Running,
    Success,
    Failed,
    Skipped,
    Canceled,
}

impl JobStatus {
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Queued => "排队",
            JobStatus::Running => "执行中",
            JobStatus::Success => "成功",
            JobStatus::Failed => "失败",
            JobStatus::Skipped => "跳过",
            JobStatus::Canceled => "取消",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Success => "success",
            JobStatus::Failed => "failed",
            JobStatus::Skipped => "skipped",
            JobStatus::Canceled => "canceled",
        }
    }
    /// 同时认库里的英文键（queued/success…）和界面快照里的中文标签（排队/成功…）：
    /// 快照里 status 存的是 label()，只按英文匹配会一律落到 Queued
    pub fn parse(s: &str) -> JobStatus {
        match s {
            "running" | "执行中" => JobStatus::Running,
            "success" | "成功" => JobStatus::Success,
            "failed" | "失败" => JobStatus::Failed,
            "skipped" | "跳过" => JobStatus::Skipped,
            "canceled" | "取消" => JobStatus::Canceled,
            _ => JobStatus::Queued,
        }
    }
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            JobStatus::Success | JobStatus::Failed | JobStatus::Skipped | JobStatus::Canceled
        )
    }
    /// 界面上状态徽标的色调：0 灰 / 1 蓝 / 2 绿 / 3 红 / 4 琥珀
    pub fn tone(self) -> i32 {
        match self {
            JobStatus::Queued => 0,
            JobStatus::Running => 1,
            JobStatus::Success => 2,
            JobStatus::Failed => 3,
            JobStatus::Skipped => 4,
            JobStatus::Canceled => 0,
        }
    }
}

// ───────────────────────── 解析结果 ─────────────────────────

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NodeInfo {
    pub path: String,
    #[serde(rename = "type", default)]
    pub ntype: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub output_parm: Option<String>,
    #[serde(default)]
    pub fstart: Option<i32>,
    #[serde(default)]
    pub fend: Option<i32>,
}

impl NodeInfo {
    pub fn cat(&self) -> Category {
        Category::parse(&self.category)
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct GlobalRange {
    #[serde(default)]
    pub fstart: Option<i32>,
    #[serde(default)]
    pub fend: Option<i32>,
    #[serde(default)]
    pub rfstart: Option<i32>,
    #[serde(default)]
    pub rfend: Option<i32>,
}

impl GlobalRange {
    /// 默认帧范围：$RFSTART~$RFEND 优先，回退 $FSTART~$FEND
    pub fn default_range(&self) -> (i32, i32) {
        let a = self.rfstart.or(self.fstart).unwrap_or(1);
        let b = self.rfend.or(self.fend).unwrap_or(100);
        if b < a {
            (a, a)
        } else {
            (a, b)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AnalyzeFile {
    #[serde(default)]
    pub hip: Option<String>,
    #[serde(default)]
    pub houdini: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub traceback: Option<String>,
    #[serde(default)]
    pub global: GlobalRange,
    #[serde(default)]
    pub nodes: Vec<NodeInfo>,
}

// ───────────────────────── 工程 ─────────────────────────

#[derive(Clone, Debug)]
pub struct Project {
    pub key: String,
    pub hip: PathBuf,
    pub shot: String,
    pub houdini: String,
    pub hython: PathBuf,
    /// hip 里记录的 Houdini 制作版本（读不到就是 None）
    pub auto_version: Option<String>,
    /// 用户手动指定过版本（此时不再跟随自动识别 / 默认版本）
    pub pinned: bool,
    pub nodes: Vec<NodeInfo>,
    pub global: (i32, i32),
    pub loading: bool,
    pub error: Option<String>,
}

impl Project {
    pub fn file_name(&self) -> String {
        self.hip
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

// ───────────────────────── 队列任务 ─────────────────────────

/// Rust 侧的任务定义（引擎的真实输入）
#[derive(Clone, Debug)]
pub struct JobSpec {
    pub id: String,
    pub project_key: String,
    pub hip: PathBuf,
    pub hython: PathBuf,
    pub houdini: String,
    pub shot: String,
    pub node_path: String,
    pub node_type: String,
    pub category: Category,
    pub output: String,
    pub fstart: i32,
    pub fend: i32,
}

impl JobSpec {
    pub fn frames(&self) -> Vec<i32> {
        if self.fend < self.fstart {
            return vec![self.fstart];
        }
        (self.fstart..=self.fend).collect()
    }
    pub fn title(&self) -> String {
        self.node_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.node_path)
            .to_string()
    }
}

/// 引擎推给界面的任务快照
#[derive(Clone, Debug, Default)]
pub struct JobView {
    pub id: String,
    pub key: String,
    pub node_path: String,
    pub title: String,
    pub shot: String,
    pub category: String,
    pub range: String,
    pub status: String,
    pub progress: i32,
    pub frames: String,
    pub tone: i32,
    pub message: String,
}

// ───────────────────────── 事件 ─────────────────────────

pub struct AnalyzePayload {
    pub houdini: String,
    pub global: (i32, i32),
    pub nodes: Vec<NodeInfo>,
}

pub enum Event {
    Queue(Vec<JobView>),
    Log(String),
    Analyzed {
        key: String,
        result: Result<AnalyzePayload, String>,
        logs: Vec<String>,
    },
}

