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

/// 把 Windows 上「HOME 被设成用户主目录」这个坑抹平。
///
/// 有些安装器/工具会往系统里写 `HOME=%USERPROFILE%`（本机就是 `C:\Users\Gaole`）。
/// hython 拿 HOME 当 Houdini 家目录，偏好目录就解析成 `C:\Users\Gaole\houdini21.0`，
/// 于是 `Documents\houdini21.0\packages` 里的包（Axiom 等）一个都不加载，
/// 打开用到 `axiom_solver_core::3.3` 的生产 hip 直接 `Bad node type`。
/// GUI 的 houdini.exe 自己会把 HOME 改成 `%USERPROFILE%\Documents`，所以只有我们起的
/// hython 会中招。
///
/// 处理方式：HOME 指到用户主目录（等价于没设）且用户没显式给 HOUDINI_USER_PREF_DIR 时，
/// 把 HOME 摘掉，让 Houdini 自己回落到 `Documents\houdiniXX.Y`——和 GUI 一致。
fn align_home_env(cmd: &mut Command) {
    let home = std::env::var_os("HOME");
    let profile = std::env::var_os("USERPROFILE");
    let pref = std::env::var_os("HOUDINI_USER_PREF_DIR");
    if should_drop_home(home.as_deref(), profile.as_deref(), pref.as_deref()) {
        cmd.env_remove("HOME");
    }
}

/// `align_home_env` 的判定部分（拆出来好写单测）。
fn should_drop_home(
    home: Option<&std::ffi::OsStr>,
    profile: Option<&std::ffi::OsStr>,
    pref_dir: Option<&std::ffi::OsStr>,
) -> bool {
    fn norm(p: &std::ffi::OsStr) -> String {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }

    // 用户自己指定了偏好目录，就别插手
    if pref_dir.is_some() {
        return false;
    }
    match (home, profile) {
        (Some(h), Some(p)) => norm(h) == norm(p),
        _ => false,
    }
}

/// 构造一个不弹控制台的 Command（Windows）。
pub fn hidden(hython: &std::path::Path) -> Command {
    let mut cmd = Command::new(hython);
    align_home_env(&mut cmd);
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

#[cfg(test)]
mod tests {
    use super::should_drop_home;
    use std::ffi::OsStr;

    fn o(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn home_equals_profile_dropped() {
        assert!(should_drop_home(
            Some(o(r"C:/Users/Gaole")),
            Some(o(r"C:/Users/Gaole")),
            None
        ));
    }

    #[test]
    fn slash_and_case_insensitive() {
        assert!(should_drop_home(
            Some(o("c:/users/gaole/")),
            Some(o(r"C:/Users/Gaole")),
            None
        ));
    }

    #[test]
    fn custom_home_kept() {
        // 用户把 HOME 指到别处（比如自定义偏好目录），别动
        assert!(!should_drop_home(
            Some(o(r"D:/prefs")),
            Some(o(r"C:/Users/Gaole")),
            None
        ));
    }

    #[test]
    fn explicit_pref_dir_wins() {
        assert!(!should_drop_home(
            Some(o(r"C:/Users/Gaole")),
            Some(o(r"C:/Users/Gaole")),
            Some(o(r"C:/Users/Gaole/Documents/houdini21.0"))
        ));
    }

    #[test]
    fn missing_vars_kept() {
        assert!(!should_drop_home(None, Some(o(r"C:/Users/Gaole")), None));
        assert!(!should_drop_home(Some(o(r"C:/Users/Gaole")), None, None));
    }
}
