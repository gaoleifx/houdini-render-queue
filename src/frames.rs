//! 帧集合：既要支持连续区间，也要支持不连续帧。
//!
//! 输入语法（用户直接打字）：`1001-1060`、`1052,1055,1056,1065-1070`、`1050`。
//! 分隔符认中英文逗号 / 分号 / 顿号；区间分隔认 `-` `–` `~` `至`（都归一成 `-`）。
//! 内部只存「已排序、已合并、互不相邻」的闭区间，所以
//! `1055,1056` 与 `1055-1056` 是同一个东西，比较相等直接比结构。

use std::collections::BTreeSet;

/// 单个任务允许的最大帧数：防止手滑打 `1-99999999` 把内存/日志撑爆
pub const MAX_FRAMES: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FrameSet {
    /// 已排序、已合并、互不相邻的闭区间（不存在 a > b 的项）
    ranges: Vec<(i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameErr {
    /// 不是帧范围（空串、半截、字母、小数点…）
    Format,
    /// 某一段结束帧小于起始帧
    Reversed(i32, i32),
    /// 帧数超过 MAX_FRAMES
    TooMany,
}

/// 区间/分隔符统一：全角、波浪号、`至` 都当成 `-`，方便逐字符判断
fn norm_char(c: char) -> char {
    match c {
        '–' | '—' | '~' | '～' | '至' | '到' => '-',
        c => c,
    }
}

fn is_sep(c: char) -> bool {
    matches!(c, ',' | '，' | ';' | '；' | '、')
}

/// 解析「一段」：`1052` 或 `1065-1070`（半截的 `1065-` 算格式错）
fn parse_token(tok: &str) -> Result<(i32, i32), FrameErr> {
    let t: String = tok.chars().filter(|c| !c.is_whitespace()).map(norm_char).collect();
    if t.is_empty() {
        return Err(FrameErr::Format);
    }
    match t.split_once('-') {
        Some((x, y)) => {
            if x.is_empty() || y.is_empty() || y.contains('-') {
                return Err(FrameErr::Format);
            }
            let a: i32 = x.parse().map_err(|_| FrameErr::Format)?;
            let b: i32 = y.parse().map_err(|_| FrameErr::Format)?;
            Ok((a, b))
        }
        None => {
            let a: i32 = t.parse().map_err(|_| FrameErr::Format)?;
            Ok((a, a))
        }
    }
}

impl FrameSet {
    pub fn single(f: i32) -> FrameSet {
        FrameSet {
            ranges: vec![(f, f)],
        }
    }

    /// 连续区间；a > b 时按单帧处理（和旧版 `default_range` 的兜底一致）
    pub fn span(a: i32, b: i32) -> FrameSet {
        if b < a {
            FrameSet::single(a)
        } else {
            FrameSet {
                ranges: vec![(a, b)],
            }
        }
    }

    /// 从已排好序的区间列表建集合：合并重叠与相邻（1055 与 1056 合成 1055-1056）
    fn normalized(mut rs: Vec<(i32, i32)>) -> FrameSet {
        rs.sort_unstable();
        let mut out: Vec<(i32, i32)> = Vec::with_capacity(rs.len());
        for (a, b) in rs {
            match out.last_mut() {
                // 用 i64 比，避免 b == i32::MAX 时 +1 溢出
                Some(last) if a as i64 <= last.1 as i64 + 1 => {
                    if b > last.1 {
                        last.1 = b;
                    }
                }
                _ => out.push((a, b)),
            }
        }
        FrameSet { ranges: out }
    }

    pub fn parse(s: &str) -> Result<FrameSet, FrameErr> {
        let mut rs: Vec<(i32, i32)> = Vec::new();
        let mut total: i64 = 0;
        for tok in s.split(is_sep) {
            if tok.chars().all(|c| c.is_whitespace()) || tok.is_empty() {
                // 尾逗号、连打两个逗号：当没写，别拿它拦用户
                continue;
            }
            let (a, b) = parse_token(tok)?;
            if b < a {
                return Err(FrameErr::Reversed(a, b));
            }
            total += (b as i64 - a as i64) + 1;
            if total > MAX_FRAMES as i64 {
                return Err(FrameErr::TooMany);
            }
            rs.push((a, b));
        }
        if rs.is_empty() {
            return Err(FrameErr::Format);
        }
        Ok(FrameSet::normalized(rs))
    }

    /// 帧数（离散帧逐个算，不是 max-min+1）
    pub fn len(&self) -> usize {
        self.ranges
            .iter()
            .map(|&(a, b)| (b as i64 - a as i64 + 1) as usize)
            .sum()
    }

    pub fn start(&self) -> i32 {
        self.ranges.first().map(|r| r.0).unwrap_or(0)
    }

    pub fn end(&self) -> i32 {
        self.ranges.last().map(|r| r.1).unwrap_or(0)
    }

    /// 展开成逐帧列表（交给 render.py 的 plan.json）
    pub fn expand(&self) -> Vec<i32> {
        let mut out = Vec::with_capacity(self.len());
        for &(a, b) in &self.ranges {
            out.extend(a..=b);
        }
        out
    }

    /// 已经跑完的帧里有几帧属于本集合（断点续渲时算进度用）
    pub fn count_done(&self, done: &BTreeSet<i32>) -> usize {
        self.ranges
            .iter()
            .map(|&(a, b)| done.range(a..=b).count())
            .sum()
    }

    /// 规范写法：`1052,1055-1056,1065-1070`（手写的 1055,1056 会被并成 1055-1056）
    pub fn label(&self) -> String {
        self.ranges
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 展示用：区间用 en dash，太长就截断加省略号（队列卡片、日志那种一行放不下的地方）
    pub fn display(&self, max_chars: usize) -> String {
        let full = self
            .ranges
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}–{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        if full.chars().count() <= max_chars {
            return full;
        }
        let mut out: String = full.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(s: &str) -> FrameSet {
        FrameSet::parse(s).expect("应能解析")
    }

    #[test]
    fn parses_discrete_frames() {
        let f = fs("1052,1055,1056,1065-1070");
        assert_eq!(f.len(), 9);
        assert_eq!(f.start(), 1052);
        assert_eq!(f.end(), 1070);
        // 相邻的 1055、1056 自动并成一段
        assert_eq!(f.label(), "1052,1055-1056,1065-1070");
        assert_eq!(
            f.expand(),
            vec![1052, 1055, 1056, 1065, 1066, 1067, 1068, 1069, 1070]
        );
    }

    #[test]
    fn single_and_span_shapes() {
        assert_eq!(fs("1050").label(), "1050");
        assert_eq!(fs("1001-1060").label(), "1001-1060");
        assert_eq!(fs(" 1001 - 1060 ").label(), "1001-1060");
        assert_eq!(fs("1001~1060").label(), "1001-1060");
        assert_eq!(fs("1001–1060").label(), "1001-1060");
        assert_eq!(fs("1001至1060").label(), "1001-1060");
        // 全角逗号 / 分号 / 顿号都认
        assert_eq!(fs("1052，1055；1070").label(), "1052,1055,1070");
        assert_eq!(fs("1052、1055").label(), "1052,1055");
    }

    #[test]
    fn merges_overlap_and_adjacent() {
        assert_eq!(fs("1070-1080,1001-1020").label(), "1001-1020,1070-1080");
        assert_eq!(fs("1001-1010,1005-1020").label(), "1001-1020");
        assert_eq!(fs("1001-1005,1006-1010").label(), "1001-1010");
        assert_eq!(fs("1001,1001,1001").label(), "1001");
    }

    #[test]
    fn tolerates_trailing_and_double_separators() {
        assert_eq!(fs("1052,1055,").label(), "1052,1055");
        assert_eq!(fs("1052,,1055").label(), "1052,1055");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(FrameSet::parse(""), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse(","), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("1052-"), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("-"), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("1052-1070-1080"), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("1.5"), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("abc"), Err(FrameErr::Format));
        assert_eq!(FrameSet::parse("1060-1001"), Err(FrameErr::Reversed(1060, 1001)));
    }

    #[test]
    fn caps_absurd_frame_counts() {
        assert_eq!(FrameSet::parse("1-99999999"), Err(FrameErr::TooMany));
        assert!(FrameSet::parse("1-100000").is_ok());
    }

    #[test]
    fn counts_done_within_set_only() {
        let f = fs("1052,1055,1056");
        let done: BTreeSet<i32> = [1052, 1053, 1056, 9999].into_iter().collect();
        // 1053 不在集合里，9999 也不在，只数 1052 与 1056
        assert_eq!(f.count_done(&done), 2);
    }

    #[test]
    fn display_truncates_long_lists() {
        let f = fs("1001,1003,1005,1007,1009,1011,1013,1015,1017,1019");
        assert!(f.display(12).chars().count() <= 12);
        assert!(f.display(12).ends_with('…'));
        assert_eq!(fs("1001-1060").display(40), "1001–1060");
    }

    #[test]
    fn equality_is_shape_based() {
        assert_eq!(fs("1055,1056"), fs("1055-1056"));
        assert_ne!(fs("1055,1057"), fs("1055-1057"));
    }
}
