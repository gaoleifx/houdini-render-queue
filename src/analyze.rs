use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::AnalyzeFile;
use crate::pyproc;

pub struct AnalyzeOut {
    pub data: AnalyzeFile,
    pub logs: Vec<String>,
}

/// 取最后 n 行，缩进一下便于嵌进日志
fn tail_of(lines: &[String], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    if lines.is_empty() {
        return "    （hython 没有任何输出）".into();
    }
    lines[start..]
        .iter()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 在 hython 中运行 analyze.py，带超时保护。
pub fn run(hython: &Path, hip: &Path, timeout: Duration) -> Result<AnalyzeOut, String> {
    let script = pyproc::script_path("analyze.py");
    if !script.exists() {
        return Err(format!("找不到 analyze.py: {}", script.display()));
    }
    if !hython.exists() {
        return Err(format!("hython 不存在: {}", hython.display()));
    }

    let out_file = std::env::temp_dir().join(format!(
        "hrq_analyze_{}_{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&out_file);

    let mut cmd = pyproc::hidden(hython);
    cmd.arg(&script)
        .arg(hip)
        .arg("--out")
        .arg(&out_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 hython 失败: {e}"))?;

    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for pipe in [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let logs = logs.clone();
        readers.push(thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if line.trim().is_empty() || line.trim_start().starts_with("##") {
                    continue;
                }
                if let Ok(mut v) = logs.lock() {
                    if v.len() < 400 {
                        v.push(line);
                    }
                }
            }
        }));
    }

    let start = Instant::now();
    let mut timed_out = false;
    let mut status: Option<std::process::ExitStatus> = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待解析进程失败: {e}"));
            }
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(120));
    }
    let elapsed = start.elapsed();
    for r in readers {
        let _ = r.join();
    }

    let collected = logs.lock().map(|v| v.clone()).unwrap_or_default();

    if timed_out {
        return Err(format!(
            "解析超时（{} 秒）。如果 Houdini 主程序正在渲染，可能是许可证被占用。\n最后输出：\n{}",
            timeout.as_secs(),
            tail_of(&collected, 8)
        ));
    }

    let text = std::fs::read_to_string(&out_file).map_err(|e| {
        // 进程没留下结果：把退出码和原始输出都摊开，别让失败悄无声息
        let how = match status {
            Some(s) => match s.code() {
                Some(c) => format!("退出码 0x{:08X}", c as u32),
                None => "进程被系统终止（崩溃）".to_string(),
            },
            None => "进程状态未知".to_string(),
        };
        format!(
            "解析没有产出结果（{how}，耗时 {:.1}s，{e}）\n最后输出：\n{}\n（常见原因：Houdini 许可证被占用 / hython 启动异常；可点中栏「重新解析」再试）",
            elapsed.as_secs_f32(),
            tail_of(&collected, 12)
        )
    })?;
    let _ = std::fs::remove_file(&out_file);

    let data: AnalyzeFile =
        serde_json::from_str(&text).map_err(|e| format!("解析结果格式错误: {e}"))?;

    if let Some(err) = &data.error {
        return Err(format!("hython 报错: {err}"));
    }

    Ok(AnalyzeOut {
        data,
        logs: collected,
    })
}
