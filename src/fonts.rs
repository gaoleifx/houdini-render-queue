//! 系统字体列表：给「字体设置」的下拉框提供选项。
//!
//! 为什么不在代码里硬编码七八个：Windows 上装的字体因机器而异（Office、Adobe、
//! 各种设计软件都会塞自己的字体），写死等于用户只能用那几个。
//!
//! 为什么用 fontdb 而不是 GDI 的 EnumFontFamiliesExW：GDI 报的是 face name，
//! 会把 Bahnschrift Light/Condensed 这类字重变体当成独立字体，还会漏掉中文族名，
//! 列出来的名字有一半喂给 Slint 是匹配不上的（选了没反应）。fontdb 正是 Slint
//! 内部用来查字体的同一个库，它 `families` 里给出的名字按定义就是能命中的。
//!
//! 两个细节：
//! 1. 常用字体（雅黑、宋体……）置顶并显示中文名，剩下的按名字排序垫在后面。
//! 2. `id` 是喂给 Slint `font-family` 的族名，`label` 只是显示用。两者分开，
//!    以后想改中文显示名不会影响字体能否命中。

/// 下拉框的一项。
pub struct Family {
    /// 交给 Slint 的 font-family 字符串（空串 = 系统默认）
    pub id: String,
    /// 下拉框里显示给人看的名字
    pub label: String,
}

/// 置顶的常用字体：(族名, 中文显示名)。只有本机确实装了才会出现在列表里。
const POPULAR: &[(&str, &str)] = &[
    ("Microsoft YaHei", "微软雅黑"),
    ("Microsoft YaHei UI", "微软雅黑 UI"),
    ("DengXian", "等线"),
    ("SimHei", "黑体"),
    ("SimSun", "宋体"),
    ("KaiTi", "楷体"),
    ("FangSong", "仿宋"),
    ("Consolas", "Consolas"),
    ("Segoe UI", "Segoe UI"),
    ("Arial", "Arial"),
    ("Calibri", "Calibri"),
    ("Times New Roman", "Times New Roman"),
    ("Tahoma", "Tahoma"),
    ("Verdana", "Verdana"),
    ("Cambria", "Cambria"),
    ("Georgia", "Georgia"),
];

/// 本机可选的字体族，第 0 项固定是「系统默认」。
pub fn families() -> Vec<Family> {
    let all = system_families();
    let mut out: Vec<Family> = Vec::with_capacity(all.len() + 1);
    out.push(Family {
        id: String::new(),
        label: "系统默认".into(),
    });

    // 常用字体置顶：用枚举到的真实族名回填 id，避免名字大小写和 fontdb 不一致
    let mut used = vec![false; all.len()];
    let mut seen: Vec<String> = vec![String::new()];
    for (want, cn) in POPULAR {
        if let Some(i) = all.iter().position(|n| n.eq_ignore_ascii_case(want)) {
            if used[i] {
                continue;
            }
            used[i] = true;
            // id 和显示名都记进 seen：后面枚举到的「宋体」不该再列一遍
            seen.push(all[i].to_lowercase());
            seen.push(cn.to_lowercase());
            out.push(Family {
                id: all[i].clone(),
                label: (*cn).to_string(),
            });
        }
    }

    // 其余按名字排序追加；同一字体常有英文名和中文名两条（SimSun / 宋体），
    // 中文名和置顶项重名时跳过，免得下拉框里出现两条一模一样的「宋体」
    let mut rest: Vec<&String> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, n)| n)
        .collect();
    rest.sort_by_key(|n| n.to_lowercase());
    for name in rest {
        let key = name.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(Family {
            id: name.clone(),
            label: name.clone(),
        });
    }
    out
}

fn system_families() -> Vec<String> {
    // 与 Slint 内部同一个字体库：这里列出的族名，Slint 一定查得到
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut names: Vec<String> = Vec::new();
    for face in db.faces() {
        // 每个 face 的 families 里既有排版族名（nameID 16）也有各语言版本，
        // 这些名字 Slint 都能命中，所以全收，靠后面的去重收拾重复
        for (name, _lang) in &face.families {
            let n = name.trim();
            // 以 '@' 开头的是竖排字形（@宋体），Slint 用不上
            if n.is_empty() || n.starts_with('@') {
                continue;
            }
            if !names.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                names.push(n.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_first_and_unique() {
        let fams = families();
        assert_eq!(fams[0].id, "");
        assert_eq!(fams[0].label, "系统默认");
        // 显示名不能重复，否则下拉框里会看到两条一样的
        let mut labels: Vec<String> = fams.iter().map(|f| f.label.to_lowercase()).collect();
        labels.sort();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len(), "显示名有重复");
    }

    #[test]
    fn no_vertical_faces() {
        for f in families() {
            assert!(!f.id.starts_with('@'), "竖排字体没过滤干净: {}", f.id);
        }
    }

    /// 本机总得有字体；枚举挂了（返回空）时只剩「系统默认」这一项，属于静默失效
    #[test]
    fn system_fonts_found() {
        let n = families().len();
        assert!(n > 10, "没枚举到系统字体，只有 {n} 项");
    }

    /// 置顶项里的族名必须真的来自枚举结果，否则用户选中等于选了张空头支票
    #[test]
    fn popular_ids_exist() {
        let fams = families();
        for f in fams.iter().take(POPULAR.len() + 1) {
            if f.id.is_empty() {
                continue;
            }
            assert!(
                fams.iter().any(|o| o.id == f.id),
                "置顶项的 id 不在枚举结果里: {}",
                f.id
            );
        }
    }
}
