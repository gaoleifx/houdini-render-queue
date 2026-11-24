//! 把窗口快照写成 BMP，用于肉眼验收界面（Slint 的 Image 没有公开的保存接口）。

use std::io::Write;
use std::path::Path;

/// 32 位 BMP，自下而上存行（BMP 的常规方向）。
pub fn save_bmp(path: &Path, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    let row_bytes = (w * 4) as usize;
    let image_size = (row_bytes * h as usize) as u32;
    let file_size = 54 + image_size;

    let mut out = Vec::with_capacity(file_size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());

    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&image_size.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..h as usize).rev() {
        let row = &rgba[y * row_bytes..(y + 1) * row_bytes];
        for px in row.chunks_exact(4) {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
            out.push(255);
        }
    }

    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)
}
