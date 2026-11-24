use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct HoudiniInstall {
    pub version: String,
    pub hython: PathBuf,
}

/// 扫描标准安装目录，列出本机所有 Houdini 版本的 hython。
/// 环境变量 HRQ_HYTHON 可额外指定 hython（多个用 `;` 隔开），排在列表最前当默认，
/// 用于非标准安装路径或自定义包装脚本。
pub fn detect_installs() -> Vec<HoudiniInstall> {
    let mut custom: Vec<HoudiniInstall> = Vec::new();
    if let Ok(raw) = std::env::var("HRQ_HYTHON") {
        for part in raw.split(';') {
            let s = part.trim();
            if s.is_empty() {
                continue;
            }
            let p = PathBuf::from(s);
            if !p.exists() {
                continue;
            }
            let version = guess_version(&p)
                .unwrap_or_else(|| format!("自定义{}", custom.len() + 1));
            custom.push(HoudiniInstall { version, hython: p });
        }
    }

    let mut out = Vec::new();
    let roots = [
        r"C:\Program Files\Side Effects Software",
        r"C:\Program Files\Side Effects Software Inc",
    ];
    for root in roots {
        let rd = match std::fs::read_dir(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            let hython = dir.join("bin").join("hython.exe");
            if !hython.exists() {
                continue;
            }
            let name = dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            // 目录名形如 "Houdini 20.5.684"
            let version = name
                .strip_prefix("Houdini ")
                .unwrap_or(&name)
                .trim()
                .to_string();
            if version.is_empty() {
                continue;
            }
            out.push(HoudiniInstall { version, hython });
        }
    }
    // 版本号倒序（新的在前）
    out.sort_by(|a, b| {
        let ka: Vec<u32> = a.version.split('.').filter_map(|x| x.parse().ok()).collect();
        let kb: Vec<u32> = b.version.split('.').filter_map(|x| x.parse().ok()).collect();
        kb.cmp(&ka)
    });
    out.dedup_by(|a, b| a.version == b.version);
    // 外部指定的排最前，同版本只留第一个
    custom.extend(out);
    let mut seen = std::collections::HashSet::new();
    custom.retain(|i| seen.insert(i.version.clone()));
    custom
}

/// 从路径里猜版本号：任何一层目录形如 21.0.596 就取它
fn guess_version(p: &Path) -> Option<String> {
    for c in p.components() {
        let s = c.as_os_str().to_string_lossy();
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() >= 2
            && parts.len() <= 3
            && parts.iter().all(|x| !x.is_empty() && x.parse::<u32>().is_ok())
            && parts[0].len() <= 2
        {
            return Some(s.to_string());
        }
    }
    None
}

// ───────────────────────── hip 制作版本识别 ─────────────────────────

/// hip 文件开头会写一行 `set -g _HIP_SAVEVERSION = '21.0.596'`，
/// 就是保存该文件的 Houdini 版本。只扫前 128KB（这一行在第一屏内），
/// 网络盘上也不会卡；读不到返回 None，交给默认版本兜底。
pub fn detect_hip_version(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut n = 0usize;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => break,
        }
    }
    buf.truncate(n);
    parse_save_version(&buf)
}

fn parse_save_version(buf: &[u8]) -> Option<String> {
    let needle = b"_HIP_SAVEVERSION";
    let at = buf
        .windows(needle.len())
        .position(|w| w == needle)?;
    let rest = &buf[at..];
    // 形如 ... = '21.0.596'
    let q1 = rest.iter().position(|&c| c == b'\'')?;
    let after = &rest[q1 + 1..];
    let q2 = after.iter().position(|&c| c == b'\'')?;
    let v = std::str::from_utf8(&after[..q2]).ok()?.trim().to_string();
    // 只接受 21.0.596 这种纯版本号，避免把变量名之类的东西认成版本
    let ok = !v.is_empty()
        && v.len() <= 24
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c.is_ascii_alphabetic());
    if ok { Some(v) } else { None }
}

/// 版本号的主次版本，用来做同系列兜底比较（21.0.596 vs 21.0.700）
fn series(v: &str) -> String {
    let mut it = v.split('.');
    match (it.next(), it.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        (Some(a), None) => a.to_string(),
        _ => String::new(),
    }
}

/// 把 hip 的制作版本映射到本机实际安装的版本下标。
/// 1) 完全一致；2) 同主次版本（补丁号不同也兼容）；都没有返回 None。
pub fn match_install(installs: &[HoudiniInstall], want: &str) -> Option<usize> {
    if want.is_empty() {
        return None;
    }
    if let Some(i) = installs.iter().position(|x| x.version == want) {
        return Some(i);
    }
    let s = series(want);
    installs.iter().position(|x| series(&x.version) == s)
}
