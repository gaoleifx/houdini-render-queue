use std::path::PathBuf;
use std::process::Command;

/// 定位随程序分发的 python 脚本目录。
pub fn script_path(name: &str) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("python").join(name));
            // target/debug/app.exe → 项目根/python
            candidates.push(dir.join("..").join("..").join("python").join(name));
        }
    }
    candidates.push(PathBuf::from("python").join(name));

    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    // 都找不到就返回第一个候选，让调用方报错时能看到期望路径
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(name))
}

/// 构造一个不弹控制台的 Command（Windows）。
pub fn hidden(hython: &std::path::Path) -> Command {
    let mut cmd = Command::new(hython);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 东八区
    let local = (secs + 8 * 3600) % 86400;
    format!(
        "{:02}:{:02}:{:02}",
        local / 3600,
        (local % 3600) / 60,
        local % 60
    )
}
