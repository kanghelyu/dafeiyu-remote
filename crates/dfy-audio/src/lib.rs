// SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Dafeiyu Remote contributors

//! 跨平台 PCM 音频采集/播放（cpal：Windows WASAPI / Linux ALSA/Pulse / macOS CoreAudio）。
//!
//! v1 固定 16 kHz、单声道、s16le、20 ms 帧（320 sample = 640 B），
//! 与 `dfy-proto::encode_audio/decode_audio` 的 DATAGRAM 格式一一对应。
//! 不做编解码、回声消除、混音或时钟同步；只提供可测的采样队列与设备后端。

#[cfg(feature = "device-backend")]
use std::collections::VecDeque;
#[cfg(feature = "device-backend")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "device-backend")]
use std::time::{Duration, Instant};

#[cfg(feature = "device-backend")]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
#[cfg(feature = "device-backend")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub use dfy_proto::{
    AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_SAMPLE_RATE, CHAN_AUDIO, PERM_AUDIO_OUT, PERM_MIC_IN,
};

/// 固定音频参数；字段公开便于会话协商与诊断显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_ms: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            frame_ms: AUDIO_FRAME_MS,
        }
    }
}

impl AudioConfig {
    /// 每帧采样数（对当前固定参数为 320）。
    #[must_use]
    pub const fn samples_per_frame(&self) -> usize {
        (self.sample_rate as usize / 1000) * self.frame_ms as usize * self.channels as usize
    }

    /// 校验是否仍是 v1 支持的固定格式。
    pub fn validate(&self) -> Result<()> {
        let d = Self::default();
        if self.sample_rate != d.sample_rate
            || self.channels != d.channels
            || self.frame_ms != d.frame_ms
        {
            bail!("v1 仅支持 16kHz/单声道/20ms（收到 {:?}）", self);
        }
        Ok(())
    }
}

/// 一帧 PCM（交错 s16le，长度应为 `samples_per_frame`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmFrame {
    pub ts_us: u64,
    pub samples: Vec<i16>,
}

impl PcmFrame {
    /// 转成 DATAGRAM 字节（含通道头）。
    pub fn to_datagram(&self, seq: u32) -> Result<Vec<u8>> {
        if self.samples.len() != AudioConfig::default().samples_per_frame() {
            bail!("PCM 帧长度错误：{}", self.samples.len());
        }
        let mut pcm = Vec::with_capacity(self.samples.len() * 2);
        for sample in &self.samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        // 与视频 DATAGRAM 一致：u32 微秒截断（约 71 分钟回绕），不是饱和到 MAX。
        let ts = self.ts_us as u32;
        dfy_proto::encode_audio(seq, ts, &pcm).context("编码音频 DATAGRAM 失败")
    }

    /// 从 DATAGRAM 解码；通道号错误时返回 `None`（交给上层按通道分派）。
    pub fn from_datagram(buf: &[u8]) -> Result<Option<(u32, Self)>> {
        if buf.first() != Some(&CHAN_AUDIO) {
            return Ok(None);
        }
        let packet = dfy_proto::decode_audio(buf).context("解码音频 DATAGRAM 失败")?;
        if packet.pcm.len() % 2 != 0 {
            bail!("音频 PCM 长度不是偶数：{}", packet.pcm.len());
        }
        let samples = packet
            .pcm
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        Ok(Some((
            packet.seq,
            Self {
                ts_us: u64::from(packet.ts_us),
                samples,
            },
        )))
    }
}

/// 音频抖动缓冲：按序号重排、检测丢包/重复，并按时间戳平滑送出。
///
/// 只缓冲固定长度的 s16le 帧；`target_delay_ms` 提供固定播放延迟以吸收抖动。
#[derive(Debug)]
pub struct AudioJitterBuffer {
    frames: std::collections::BTreeMap<u32, PcmFrame>,
    target_delay_ms: u64,
    next_seq: Option<u32>,
    /// 已送出的最后一包序号；丢包在送出时按空隙确认，避免迟到包误计。
    last_played: Option<u32>,
    received: u64,
    duplicates: u64,
    reordered: u64,
    lost: u64,
    /// 因队列容量上限被淘汰的帧（静默丢弃，不计 seq 丢包）。
    evicted: u64,
    deepest: usize,
    min_delay_ms: u64,
    max_delay_ms: u64,
}

/// 抖动缓冲容量上限（约 1.2 秒 20ms 帧），只用于防止无界增长。
const JITTER_CAPACITY: usize = 64;

impl AudioJitterBuffer {
    pub fn new(target_delay_ms: u64) -> Self {
        Self {
            frames: std::collections::BTreeMap::new(),
            target_delay_ms,
            next_seq: None,
            last_played: None,
            received: 0,
            duplicates: 0,
            reordered: 0,
            lost: 0,
            evicted: 0,
            deepest: 0,
            min_delay_ms: target_delay_ms.min(20),
            max_delay_ms: target_delay_ms.max(200),
        }
    }

    /// 收一包；`now_us` 为本地到达时刻，用于计算是否到了目标延迟。
    /// 返回 `true` 表示这包带来新的可播放帧（后续调用 `pop_ready`）。
    pub fn push(&mut self, seq: u32, frame: PcmFrame, now_us: u64) -> Result<bool> {
        if frame.samples.len() != AudioConfig::default().samples_per_frame() {
            bail!("抖动缓冲收到错误长度的 PCM 帧：{}", frame.samples.len());
        }
        self.received += 1;
        let expects = match self.next_seq {
            None => {
                self.next_seq = Some(seq.wrapping_add(1));
                true
            }
            Some(_) if self.frames.contains_key(&seq) => {
                self.duplicates += 1;
                return Ok(false);
            }
            Some(next) if seq == next => {
                // 顺序到达：立即推进期望，避免把排队中的正常包误记为乱序。
                self.next_seq = Some(seq.wrapping_add(1));
                true
            }
            Some(_) => {
                self.reordered += 1;
                false
            }
        };
        // 不再按“新包时间戳”淘汰旧帧：那会把突发到达中仍应播放的帧静默丢掉。
        // 只用容量上限防止无界增长，并在统计里如实计入淘汰数。
        while self.frames.len() >= JITTER_CAPACITY {
            let oldest = *self.frames.first_key_value().expect("non-empty").0;
            self.frames.remove(&oldest);
            self.evicted += 1;
        }
        self.frames.insert(seq, frame);
        self.deepest = self.deepest.max(self.frames.len());
        // `now_us` 仅用于未来扩展（自适应延迟）；当前用固定延迟，不丢弃参数。
        let _ = now_us;
        Ok(expects || self.frames.len() > 1)
    }

    /// 取走一包到期帧；没有到期帧返回 `None`。
    pub fn pop_ready(&mut self, now_us: u64) -> Option<(u32, PcmFrame)> {
        let due = now_us.saturating_sub(self.target_delay_ms.saturating_mul(1000));
        let first = self.frames.first_key_value()?;
        if first.1.ts_us > due {
            return None;
        }
        let seq = *first.0;
        if let Some(previous) = self.last_played {
            let expected = previous.wrapping_add(1);
            if seq > expected {
                self.lost += u64::from(seq.wrapping_sub(expected));
            }
        }
        let frame = self.frames.remove(&seq)?;
        self.last_played = Some(seq);
        Some((seq, frame))
    }

    /// 按当前队列深度缓慢自适应目标延迟：积压时增大，空闲时缓慢回收。
    /// 上下限在构造时由初始 target_delay 决定（20..=200ms）。
    pub fn adapt(&mut self, depth: usize) {
        if depth > 8 {
            self.target_delay_ms = (self.target_delay_ms + 10).min(self.max_delay_ms);
        } else if depth <= 1 {
            self.target_delay_ms = self
                .target_delay_ms
                .saturating_sub(2)
                .max(self.min_delay_ms);
        }
    }

    /// 当前目标延迟（毫秒）。
    #[must_use]
    pub const fn target_delay_ms(&self) -> u64 {
        self.target_delay_ms
    }

    /// 会话/采样序号重新开始时清空队列与期望序号（保留累计统计）。
    /// 迁移后 Agent 会新建音频任务，seq 从 0 重新计数，必须调用本方法。
    pub fn reset_sequence(&mut self) {
        self.frames.clear();
        self.next_seq = None;
        self.last_played = None;
    }

    /// 队首帧时间戳（诊断用）。
    #[must_use]
    pub fn oldest_ts(&self) -> Option<u64> {
        self.frames.first_key_value().map(|(_, frame)| frame.ts_us)
    }

    /// 当前排队的帧数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 统计快照（收到/丢失/重复/乱序/峰值深度）。
    #[must_use]
    pub const fn stats(&self) -> AudioBufferStats {
        AudioBufferStats {
            received: self.received,
            lost: self.lost,
            evicted: self.evicted,
            duplicates: self.duplicates,
            reordered: self.reordered,
            deepest: self.deepest,
        }
    }
}

/// 抖动缓冲统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioBufferStats {
    pub received: u64,
    pub lost: u64,
    pub evicted: u64,
    pub duplicates: u64,
    pub reordered: u64,
    pub deepest: usize,
}

/// 采集端：产生固定长度的 PCM 帧。
pub trait AudioSource: Send {
    fn next_frame(&mut self) -> Result<Option<PcmFrame>>;
}

/// 播放端：消费一帧 PCM。
pub trait AudioSink: Send {
    fn play(&mut self, frame: &PcmFrame) -> Result<()>;
}

/// 音量/静音包装器：对所有送出的 PCM 样本应用线性增益，饱和到 i16。
///
/// 只是软件增益，不改设备音量；`volume=100` 且未静音时为恒等变换。
pub struct GainSink {
    inner: Box<dyn AudioSink>,
    volume: u8,
    muted: bool,
}

impl GainSink {
    pub fn new(inner: Box<dyn AudioSink>, volume: u8, muted: bool) -> Self {
        Self {
            inner,
            volume: volume.min(100),
            muted,
        }
    }

    pub fn set_volume(&mut self, volume: u8) {
        self.volume = volume.min(100);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    #[must_use]
    pub const fn volume(&self) -> u8 {
        self.volume
    }

    #[must_use]
    pub const fn is_muted(&self) -> bool {
        self.muted
    }

    /// 应用增益后的帧（供单测直接观察，不经过设备）。
    #[must_use]
    pub fn apply(&self, frame: &PcmFrame) -> PcmFrame {
        let gain = if self.muted {
            0.0
        } else {
            f64::from(self.volume) / 100.0
        };
        PcmFrame {
            ts_us: frame.ts_us,
            samples: frame
                .samples
                .iter()
                .map(|sample| {
                    ((f64::from(*sample) * gain).round() as i64).clamp(-32768, 32767) as i16
                })
                .collect(),
        }
    }
}

impl AudioSink for GainSink {
    fn play(&mut self, frame: &PcmFrame) -> Result<()> {
        let gained = self.apply(frame);
        self.inner.play(&gained)
    }
}

/// 不访问设备的采集器（测试/未授权降级）。
#[derive(Debug, Default)]
pub struct NullSource;

impl AudioSource for NullSource {
    fn next_frame(&mut self) -> Result<Option<PcmFrame>> {
        Ok(None)
    }
}

/// 不访问设备的播放器（测试/未授权降级）。
#[derive(Debug, Default)]
pub struct NullSink;

impl AudioSink for NullSink {
    fn play(&mut self, _frame: &PcmFrame) -> Result<()> {
        Ok(())
    }
}

/// 声道下混：把多声道交错 PCM 帧求平均压成单声道。
fn downmix_interleaved_to_mono(interleaved: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch_i64 = channels as i64;
    interleaved
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i64 = frame.iter().map(|&s| i64::from(s)).sum();
            (sum / ch_i64).clamp(-32768, 32767) as i16
        })
        .collect()
}

/// 声道上混：把单声道 PCM 复制到多声道交错输出。
fn upmix_mono_to_interleaved(mono: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return mono.to_vec();
    }
    let mut out = Vec::with_capacity(mono.len() * channels);
    for &sample in mono {
        for _ in 0..channels {
            out.push(sample);
        }
    }
    out
}

/// 连续分块单声道线性重采样器（供 48 kHz CoreAudio/WASAPI 设备与 16 kHz 线格式互通）。
struct MonoResampler {
    src_rate: u32,
    dst_rate: u32,
    step: f64,
    phase: f64,
    prev: Option<i16>,
}

impl MonoResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        let step = if src_rate == 0 || dst_rate == 0 {
            1.0
        } else {
            f64::from(src_rate) / f64::from(dst_rate)
        };
        Self {
            src_rate,
            dst_rate,
            step,
            phase: 0.0,
            prev: None,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.src_rate == self.dst_rate || self.src_rate == 0 || self.dst_rate == 0 {
            return input.to_vec();
        }
        let scratch;
        let samples: &[i16] = if let Some(prev) = self.prev {
            let mut buf = Vec::with_capacity(input.len() + 1);
            buf.push(prev);
            buf.extend_from_slice(input);
            scratch = buf;
            &scratch
        } else {
            input
        };
        if samples.len() < 2 {
            self.prev = samples.last().copied();
            return Vec::new();
        }
        let limit = samples.len() - 1;
        let est = ((limit as f64 / self.step).ceil() as usize).saturating_add(2);
        let mut out = Vec::with_capacity(est);
        let mut k = 0usize;
        let src_f = f64::from(self.src_rate);
        let dst_f = f64::from(self.dst_rate);
        loop {
            let pos = self.phase + ((k as f64) * src_f) / dst_f;
            if pos >= limit as f64 {
                self.phase = (pos - limit as f64).max(0.0);
                break;
            }
            let idx = pos.floor() as usize;
            if idx >= limit {
                self.phase = 0.0;
                break;
            }
            let frac = pos - idx as f64;
            let s0 = f64::from(samples[idx]);
            let s1 = f64::from(samples[idx + 1]);
            out.push((s0 + (s1 - s0) * frac).round().clamp(-32768.0, 32767.0) as i16);
            k += 1;
        }
        self.prev = input.last().copied();
        out
    }
}

#[cfg(feature = "device-backend")]
struct CaptureBuffer {
    queue: VecDeque<i16>,
    resampler: MonoResampler,
    channels: usize,
    max_queue: usize,
}

/// 从任意 cpal 样本类型转成 16kHz 单声道 i16 并追加到队列。
#[cfg(feature = "device-backend")]
fn append_samples<T: cpal::Sample>(data: &[T], state: &Mutex<CaptureBuffer>)
where
    i16: cpal::FromSample<T>,
{
    let raw: Vec<i16> = data
        .iter()
        .map(|&sample| cpal::Sample::to_sample::<i16>(sample))
        .collect();
    if let Ok(mut guard) = state.lock() {
        let mono = downmix_interleaved_to_mono(&raw, guard.channels);
        let resampled = guard.resampler.process(&mono);
        guard.queue.extend(resampled);
        while guard.queue.len() > guard.max_queue {
            guard.queue.pop_front();
        }
    }
}

/// cpal 默认输入设备采集器。回调只做下混/重采样并入队，不阻塞设备线程。
#[cfg(feature = "device-backend")]
pub struct DeviceSource {
    _stream: cpal::Stream,
    state: Arc<Mutex<CaptureBuffer>>,
    per_frame: usize,
    wait: Duration,
}

#[cfg(feature = "device-backend")]
impl DeviceSource {
    /// 打开默认输入设备；无设备或格式不可用时返回明确错误。
    pub fn open(config: AudioConfig) -> Result<Self> {
        config.validate()?;
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("没有可用的默认音频输入设备"))?;
        let device_config = device
            .default_input_config()
            .context("读取默认输入音频格式失败")?;
        let device_channels = usize::from(device_config.channels().max(1));
        let device_rate = device_config.sample_rate().max(1);
        let state = Arc::new(Mutex::new(CaptureBuffer {
            queue: VecDeque::new(),
            resampler: MonoResampler::new(device_rate, config.sample_rate),
            channels: device_channels,
            max_queue: config.samples_per_frame() * 25,
        }));
        let callback_state = Arc::clone(&state);
        let stream = match device_config.sample_format() {
            cpal::SampleFormat::I16 => device.build_input_stream(
                device_config.config(),
                move |data: &[i16], _| append_samples(data, &callback_state),
                |err| tracing::warn!(error = %err, "音频采集回调失败"),
                None,
            ),
            cpal::SampleFormat::F32 => device.build_input_stream(
                device_config.config(),
                move |data: &[f32], _| append_samples(data, &callback_state),
                |err| tracing::warn!(error = %err, "音频采集回调失败"),
                None,
            ),
            other => bail!("不支持的输入采样格式：{other}"),
        }
        .context("建立音频输入流失败")?;
        stream.play().context("启动音频输入流失败")?;
        Ok(Self {
            _stream: stream,
            state,
            per_frame: config.samples_per_frame(),
            wait: Duration::from_millis(config.frame_ms as u64),
        })
    }
}

#[cfg(feature = "device-backend")]
impl AudioSource for DeviceSource {
    fn next_frame(&mut self) -> Result<Option<PcmFrame>> {
        let deadline = Instant::now() + self.wait;
        loop {
            {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if guard.queue.len() >= self.per_frame {
                    let samples: Vec<i16> = guard.queue.drain(..self.per_frame).collect();
                    return Ok(Some(PcmFrame {
                        ts_us: dfy_transport_now_us(),
                        samples,
                    }));
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// cpal 默认输出设备播放器。回调只做出队，不阻塞设备线程。
#[cfg(feature = "device-backend")]
pub struct DeviceSink {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<i16>>>,
    resampler: MonoResampler,
    device_channels: usize,
    max_queue: usize,
}

#[cfg(feature = "device-backend")]
impl DeviceSink {
    pub fn open(config: AudioConfig) -> Result<Self> {
        config.validate()?;
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("没有可用的默认音频输出设备"))?;
        let device_config = device
            .default_output_config()
            .context("读取默认输出音频格式失败")?;
        let device_channels = usize::from(device_config.channels().max(1));
        let device_rate = device_config.sample_rate().max(1);
        let queue = Arc::new(Mutex::new(VecDeque::<i16>::new()));
        let callback_queue = Arc::clone(&queue);
        let stream = match device_config.sample_format() {
            cpal::SampleFormat::I16 => device.build_output_stream(
                device_config.config(),
                move |data: &mut [i16], _| drain_samples(data, &callback_queue),
                |err| tracing::warn!(error = %err, "音频播放回调失败"),
                None,
            ),
            cpal::SampleFormat::F32 => device.build_output_stream(
                device_config.config(),
                move |data: &mut [f32], _| drain_samples(data, &callback_queue),
                |err| tracing::warn!(error = %err, "音频播放回调失败"),
                None,
            ),
            other => bail!("不支持的输出采样格式：{other}"),
        }
        .context("建立音频输出流失败")?;
        stream.play().context("启动音频输出流失败")?;
        Ok(Self {
            _stream: stream,
            queue,
            resampler: MonoResampler::new(config.sample_rate, device_rate),
            device_channels,
            max_queue: (device_rate as usize / 2).saturating_mul(device_channels),
        })
    }
}

#[cfg(feature = "device-backend")]
fn drain_samples<T: cpal::Sample + cpal::FromSample<i16>>(
    data: &mut [T],
    queue: &Mutex<VecDeque<i16>>,
) {
    if let Ok(mut guard) = queue.lock() {
        for slot in data.iter_mut() {
            let sample = guard.pop_front().unwrap_or(0);
            *slot = cpal::Sample::to_sample(sample);
        }
    }
}

#[cfg(feature = "device-backend")]
impl AudioSink for DeviceSink {
    fn play(&mut self, frame: &PcmFrame) -> Result<()> {
        let resampled = self.resampler.process(&frame.samples);
        let interleaved = upmix_mono_to_interleaved(&resampled, self.device_channels);
        let mut guard = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.extend(interleaved);
        while guard.len() > self.max_queue {
            guard.pop_front();
        }
        Ok(())
    }
}

/// 避免为取时间引入额外依赖；由会话层传入的 `ts_us` 才是权威时间戳。
#[cfg(feature = "device-backend")]
fn dfy_transport_now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// 打开默认采集/播放后端；未编译 `device-backend` 时返回 Null 降级。
#[cfg(feature = "device-backend")]
pub fn create_source(config: AudioConfig) -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(DeviceSource::open(config)?))
}

/// 打开默认播放后端；未编译 `device-backend` 时返回 Null 降级。
#[cfg(feature = "device-backend")]
pub fn create_sink(config: AudioConfig) -> Result<Box<dyn AudioSink>> {
    Ok(Box::new(DeviceSink::open(config)?))
}

#[cfg(not(feature = "device-backend"))]
pub fn create_source(_config: AudioConfig) -> Result<Box<dyn AudioSource>> {
    Ok(Box::new(NullSource))
}

#[cfg(not(feature = "device-backend"))]
pub fn create_sink(_config: AudioConfig) -> Result<Box<dyn AudioSink>> {
    Ok(Box::new(NullSink))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq_ts: u64) -> PcmFrame {
        PcmFrame {
            ts_us: seq_ts,
            samples: (0..AudioConfig::default().samples_per_frame())
                .map(|i| (i as i16).wrapping_mul(7))
                .collect(),
        }
    }

    #[test]
    fn jitter_buffer_plays_truncated_epoch_timestamps() {
        // DATAGRAM 里的 ts_us 是 u32 截断值；本地 now 是完整 Unix 微秒。
        // 缓冲必须按“已到期”播放，不能因两者数量级不同而出队失败。
        let now = 1_790_000_000_000u64;
        let truncated = (now as u32) as u64;
        let mut buffer = AudioJitterBuffer::new(100);
        for seq in 0..10 {
            let mut f = frame(truncated + u64::from(seq) * 20_000);
            f.ts_us = truncated + u64::from(seq) * 20_000;
            buffer.push(seq, f, now).unwrap();
        }
        let mut out = 0;
        while buffer.pop_ready(now).is_some() {
            out += 1;
        }
        assert_eq!(out, 10);
    }

    #[test]
    fn jitter_buffer_reorders_detects_loss_and_duplicates() {
        let mut buffer = AudioJitterBuffer::new(20);
        for (seq, ts) in [(1u32, 1000u64), (3, 1040), (2, 1020), (2, 1020), (5, 1080)] {
            let mut f = frame(ts);
            f.ts_us = ts;
            buffer.push(seq, f, ts).unwrap();
        }
        let stats = buffer.stats();
        assert_eq!(stats.received, 5);
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.reordered, 2);
        assert_eq!(stats.lost, 0); // 跳号丢失在送出时确认，避免迟到包被误计。
                                   // 到期后按序号顺序送出 1,2,3,5；缺的 4 在送出时计为丢失。
        let mut out = Vec::new();
        while let Some((seq, _)) = buffer.pop_ready(30_000) {
            out.push(seq);
        }
        assert_eq!(out, vec![1, 2, 3, 5]);
        assert_eq!(buffer.stats().lost, 1);
        assert!(buffer.is_empty());
    }

    #[test]
    fn config_and_frame_sizes() {
        let config = AudioConfig::default();
        assert_eq!(config.samples_per_frame(), 320);
        assert!(config.validate().is_ok());
        assert!(AudioConfig {
            sample_rate: 48_000,
            ..config
        }
        .validate()
        .is_err());
    }

    #[test]
    fn pcm_datagram_roundtrip() {
        let original = frame(1_700_000_000_123);
        let bytes = original.to_datagram(17).unwrap();
        assert_eq!(bytes[0], CHAN_AUDIO);
        let (seq, decoded) = PcmFrame::from_datagram(&bytes).unwrap().unwrap();
        assert_eq!(seq, 17);
        // ts_us 在 DATAGRAM 里是 u32，回读后会截断。
        assert_eq!(decoded.ts_us, original.ts_us as u32 as u64);
        assert_eq!(decoded.samples, original.samples);
        assert!(PcmFrame::from_datagram(&[0u8; 4]).unwrap().is_none());
    }

    #[test]
    #[cfg(feature = "device-backend")]
    fn device_enumeration_reports_state() {
        let host = cpal::default_host();
        let inputs = host.input_devices().map(|it| it.count()).unwrap_or(0);
        let outputs = host.output_devices().map(|it| it.count()).unwrap_or(0);
        println!(
            "cpal host={} inputs={inputs} outputs={outputs} default_input={} default_output={}",
            host.id().name(),
            host.default_input_device().is_some(),
            host.default_output_device().is_some()
        );
    }

    #[test]
    #[cfg(feature = "device-backend")]
    fn default_output_device_accepts_pcm() {
        if cpal::default_host().default_output_device().is_none() {
            eprintln!("skip: no default output device");
            return;
        }
        let mut sink = create_sink(AudioConfig::default()).expect("open default output device");
        sink.play(&frame(1)).expect("play one PCM frame");
    }

    #[test]
    fn jitter_buffer_adapts_within_bounds() {
        let mut buffer = AudioJitterBuffer::new(100);
        for _ in 0..20 {
            buffer.adapt(16);
        }
        assert_eq!(buffer.target_delay_ms(), 200);
        for _ in 0..200 {
            buffer.adapt(0);
        }
        assert_eq!(buffer.target_delay_ms(), 20);
    }

    #[test]
    fn gain_sink_volume_and_mute() {
        struct Collect(Vec<i16>);
        impl AudioSink for Collect {
            fn play(&mut self, frame: &PcmFrame) -> Result<()> {
                self.0.extend(frame.samples.iter().copied());
                Ok(())
            }
        }
        let original = PcmFrame {
            ts_us: 1,
            samples: vec![1000, -1000, 32767],
        };
        let mut sink = GainSink::new(Box::new(Collect(Vec::new())), 100, false);
        assert_eq!(sink.apply(&original).samples, original.samples);
        sink.set_volume(50);
        assert_eq!(sink.apply(&original).samples, vec![500, -500, 16_384]);
        sink.set_muted(true);
        assert_eq!(sink.apply(&original).samples, vec![0, 0, 0]);
        sink.set_muted(false);
        sink.set_volume(200);
        assert_eq!(sink.volume(), 100);
    }

    #[test]
    fn null_backends_never_panic() {
        let mut source = NullSource;
        let mut sink = NullSink;
        assert!(source.next_frame().unwrap().is_none());
        sink.play(&frame(1)).unwrap();
    }

    #[test]
    fn downmix_stereo_and_resample_48k_to_16k() {
        // Two consecutive 20ms stereo chunks at 48 kHz (960 frames each) -> ~320 + 320 mono 16kHz.
        let mut resampler = MonoResampler::new(48_000, 16_000);
        let mut total = Vec::new();
        for _ in 0..2 {
            let mut stereo = Vec::with_capacity(960 * 2);
            for i in 0..960 {
                // Constant DC average should stay close to 1000 across channel mix & resampling.
                stereo.push(600 + ((i % 2) as i16 * 100));
                stereo.push(1400 - ((i % 2) as i16 * 100));
            }
            let mono = downmix_interleaved_to_mono(&stereo, 2);
            assert_eq!(mono.len(), 960);
            total.extend(resampler.process(&mono));
        }
        assert_eq!(total.len(), 640);
        assert!(total.iter().all(|&s| (s - 1000).abs() <= 2));
    }

    #[test]
    fn resample_16k_to_48k_and_upmix_stereo() {
        let mut resampler = MonoResampler::new(16_000, 48_000);
        let mut mono_out = Vec::new();
        for chunk_idx in 0..2 {
            let chunk: Vec<i16> = (0..320)
                .map(|i| (((chunk_idx * 320 + i) as f64 * 0.05).sin() * 10_000.0) as i16)
                .collect();
            mono_out.extend(resampler.process(&chunk));
        }
        // First chunk has 319 intervals -> 957 samples, second has 320 intervals -> 960 samples.
        assert_eq!(mono_out.len(), 957 + 960);
        let stereo = upmix_mono_to_interleaved(&mono_out, 2);
        assert_eq!(stereo.len(), mono_out.len() * 2);
        assert!(stereo.chunks_exact(2).all(|lr| lr[0] == lr[1]));
    }

    #[test]
    fn resampler_passthrough_when_rates_match() {
        let mut resampler = MonoResampler::new(16_000, 16_000);
        let input = vec![10, -20, 300, -400];
        assert_eq!(resampler.process(&input), input);
    }
}
