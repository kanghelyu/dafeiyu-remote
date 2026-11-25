// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Dafeiyu Remote contributors

//! 解码一个 Annex-B 的 `.h264` 文件，用于排查解码链路（对比 ffmpeg 产出的参考码流）。
//!
//! ```text
//! cargo run -p dfy-codec --example decode_file -- <文件路径>
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use dfy_codec::{available_encoders, create_decoder, Codec};

fn main() -> Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("用法：decode_file <文件路径>")?;
    let data = std::fs::read(&path).with_context(|| format!("读取文件失败：{}", path.display()))?;
    println!("输入文件：{}（{} 字节）", path.display(), data.len());
    for info in available_encoders() {
        println!(
            "编码器：{} codec={} 硬件={} 可用={}",
            info.name, info.codec, info.hardware, info.available
        );
    }
    let mut dec = create_decoder(Codec::H264, false).context("创建解码器失败")?;
    let mut frames = dec.decode(&data).context("解码失败")?;
    println!("送入全部数据后解出帧数：{}", frames.len());
    let tail = dec.flush().context("冲刷解码器失败")?;
    println!("冲刷出帧数：{}", tail.len());
    frames.extend(tail);
    println!("合计解出帧数：{}", frames.len());
    if let Some(f) = frames.first() {
        let luma = f
            .data
            .iter()
            .take((f.stride as usize).saturating_mul(f.height as usize))
            .map(|b| *b as u64)
            .sum::<u64>() as f64
            / (f.stride as f64 * f.height as f64);
        println!(
            "首帧：{}x{} stride={} format={} pts={}us 亮度均值 {:.1}",
            f.width, f.height, f.stride, f.format, f.pts_us, luma
        );
    }
    Ok(())
}
