use std::path::PathBuf;
use std::process::Command;

fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI 编译失败");

    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.rc");

    #[cfg(windows)]
    embed_icon();
}

/// 用 rc.exe 把 assets/app.rc（图标）编成 .res，再交给链接器塞进 exe。
/// 资源编译器随 Windows SDK 安装、通常不在 PATH，这里自行定位；
/// 找不到就只警告、不中断构建（没有图标也能正常跑）。
#[cfg(windows)]
fn embed_icon() {
    let rc = match find_rc() {
        Some(p) => p,
        None => {
            println!("cargo:warning=未找到 rc.exe（Windows SDK），本次构建不带图标");
            return;
        }
    };

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR 缺失"));
    let res = out_dir.join("app.res");

    let status = Command::new(&rc)
        .current_dir("assets")
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg("app.rc")
        .status();

    match status {
        Ok(s) if s.success() && res.exists() => {
            // .res 可以直接交给 link.exe。
            // 注意：link.exe 不接受带引号的参数（会当成文件名的一部分），
            // 所以工程路径里有空格时改用临时目录里的无空格路径。
            let res = if res.to_string_lossy().contains(' ') {
                let flat = std::env::temp_dir().join("houdini-render-queue-app.res");
                match std::fs::copy(&res, &flat) {
                    Ok(_) => flat,
                    Err(e) => {
                        println!("cargo:warning=复制 app.res 失败：{e}，本次构建不带图标");
                        return;
                    }
                }
            } else {
                res
            };
            println!("cargo:rustc-link-arg={}", res.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe 退出码 {s}，本次构建不带图标"),
        Err(e) => println!("cargo:warning=调用 rc.exe 失败：{e}，本次构建不带图标"),
    }
}

/// 依次尝试：RC 环境变量 → PATH → Windows SDK 目录。
#[cfg(windows)]
fn find_rc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["rc.exe", "llvm-rc.exe"] {
                let c = dir.join(name);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }

    // Windows SDK：...\Windows Kits\10\bin\10.0.22621.0\x64\rc.exe
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("WindowsSdkDir") {
        roots.push(PathBuf::from(p).join("bin"));
    }
    for key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(p) = std::env::var_os(key) {
            roots.push(PathBuf::from(p).join(r"Windows Kits\10\bin"));
        }
    }

    for root in roots {
        let mut versions: Vec<PathBuf> = match std::fs::read_dir(&root) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
            Err(_) => continue,
        };
        // 版本目录名按字典序倒排，新的在前
        versions.sort();
        versions.reverse();
        for v in versions {
            for arch in ["x64", "x86", "arm64"] {
                let c = v.join(arch).join("rc.exe");
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }

    None
}
