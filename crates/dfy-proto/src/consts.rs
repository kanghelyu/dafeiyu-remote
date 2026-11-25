//! DFYP/1 协议常量：版本、通道号、头部长度、错误码、权限位、输入事件类别。
//!
//! 这些常量是 clean-room 定义（草案见 `spec/DFYP-v1.md`），不复用任何第三方协议的字段号或常量表。

/// 协议主版本：不兼容变更才升级（`spec/DFYP-v1.md` §11）。
pub const PROTO_MAJOR: u32 = 1;
/// 协议次版本：追加字段 / 消息时递增。
pub const PROTO_MINOR: u32 = 0;

/// QUIC ALPN 标识。
pub const ALPN: &[u8] = b"dfyp/1";

/// 控制帧（4B 大端长度 + protobuf）单帧上限：8 MiB。
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// DATAGRAM 通道号（`spec/DFYP-v1.md` §3 通道表）。
pub const CHAN_INPUT_MOVE: u8 = 0;
/// 视频 DATAGRAM 通道。
pub const CHAN_VIDEO: u8 = 1;
/// 音频 DATAGRAM 通道（v1 未实现）。
pub const CHAN_AUDIO: u8 = 2;
/// 剪贴板数据通道（DATAGRAM 预留；v1 走 control 流，见 DFYP §8）。
pub const CHAN_CLIPBOARD: u8 = 4;

/// 视频 DATAGRAM 头长度（字节）。
pub const VIDEO_HEADER_LEN: usize = 16;
/// flags bit0：关键帧（IDR）。
pub const VIDEO_FLAG_KEY: u8 = 0b0000_0001;
/// flags bit1：本帧最后一个分片。
pub const VIDEO_FLAG_LAST: u8 = 0b0000_0010;
/// flags bit2：本包是 FEC 包（v1 未实现）。
pub const VIDEO_FLAG_FEC: u8 = 0b0000_0100;
/// flags bit3：本包属于 XOR 组（v1 未实现）。
pub const VIDEO_FLAG_XOR_GROUP: u8 = 0b0000_1000;

/// 单个视频分片的建议负载上限（QUIC 初始 MTU 1200 − 头开销，见 `research/050` §4.1）。
pub const VIDEO_MAX_PAYLOAD: usize = 1100;

/// 输入事件类别：按键（可靠流）。
pub const INPUT_KIND_KEY: u32 = 1;
/// 输入事件类别：鼠标按键（可靠流）。
pub const INPUT_KIND_MOUSE_BTN: u32 = 2;
/// 输入事件类别：鼠标位移 / 绝对坐标（input-move DATAGRAM）。
pub const INPUT_KIND_MOUSE_MOVE: u32 = 3;
/// 输入事件类别：滚轮（可靠流）。
pub const INPUT_KIND_WHEEL: u32 = 4;
/// 输入事件类别：触摸。
pub const INPUT_KIND_TOUCH: u32 = 5;
/// 输入事件类别：手柄轴。
pub const INPUT_KIND_PAD: u32 = 6;
/// 输入事件类别：IME / Unicode 文本。
pub const INPUT_KIND_TEXT: u32 = 7;

/// 输入事件 state：按下。
pub const INPUT_STATE_DOWN: u32 = 1;
/// 输入事件 state：抬起 / 释放。
pub const INPUT_STATE_UP: u32 = 0;

/// 权限位：查看（`spec/DFYP-v1.md` §7）。
pub const PERM_VIEW: u32 = 1 << 0;
/// 权限位：控制（输入注入）。
pub const PERM_CONTROL: u32 = 1 << 1;
/// 权限位：剪贴板。
pub const PERM_CLIPBOARD: u32 = 1 << 2;
/// 权限位：文件传输。
pub const PERM_FILE: u32 = 1 << 3;
/// 权限位：被控端音频输出。
pub const PERM_AUDIO_OUT: u32 = 1 << 4;
/// 权限位：麦克风回传。
pub const PERM_MIC_IN: u32 = 1 << 5;
/// 权限位：终端。
pub const PERM_TERMINAL: u32 = 1 << 6;
/// 权限位：系统操作（关机/重启等）。
pub const PERM_SYSTEM_ACTION: u32 = 1 << 7;
/// 权限位：隐私屏。
pub const PERM_PRIVACY_SCREEN: u32 = 1 << 8;

/// 默认权限：`VIEW|CONTROL|CLIPBOARD`。
pub const PERM_DEFAULT: u32 = PERM_VIEW | PERM_CONTROL | PERM_CLIPBOARD;

/// 错误码 `E_AUTH`：鉴权失败。
pub const ERR_AUTH: u32 = 1;
/// 错误码 `E_PERM`：权限不足。
pub const ERR_PERM: u32 = 2;
/// 错误码 `E_VERSION`：协议版本不兼容。
pub const ERR_VERSION: u32 = 3;
/// 错误码 `E_BUSY`：已有会话。
pub const ERR_BUSY: u32 = 4;
/// 错误码 `E_TIMEOUT`：超时。
pub const ERR_TIMEOUT: u32 = 5;
/// 错误码 `E_NET`：网络错误。
pub const ERR_NET: u32 = 6;
/// 错误码 `E_CLIPBOARD_BUSY`：剪贴板忙。
pub const ERR_CLIPBOARD_BUSY: u32 = 7;
/// 错误码 `E_FILE_DENIED`：文件访问被拒。
pub const ERR_FILE_DENIED: u32 = 8;
/// 错误码 `E_ENCODER_UNAVAILABLE`：编码器不可用。
pub const ERR_ENCODER_UNAVAILABLE: u32 = 9;
/// 错误码 `E_PROTOCOL`：对端违反协议（缺消息、字段非法）。
pub const ERR_PROTOCOL: u32 = 10;

/// `Bye` 原因：正常结束。
pub const BYE_NORMAL: u32 = 0;
/// `Bye` 原因：用户主动断开。
pub const BYE_USER: u32 = 1;
/// `Bye` 原因：心跳超时。
pub const BYE_TIMEOUT: u32 = 2;
/// `Bye` 原因：内部错误。
pub const BYE_ERROR: u32 = 3;

/// 本实现宣告的能力名（Welcome.capability）。
pub const CAPABILITIES: &[&str] = &[
    "ctrl.frame.v1",
    "video.datagram.v1",
    "input-move.datagram.v1",
    "monitor-list.v1",
    "heartbeat.v1",
];

/// 错误码 → 规范名（日志/诊断用）。
pub fn error_name(code: u32) -> &'static str {
    match code {
        ERR_AUTH => "E_AUTH",
        ERR_PERM => "E_PERM",
        ERR_VERSION => "E_VERSION",
        ERR_BUSY => "E_BUSY",
        ERR_TIMEOUT => "E_TIMEOUT",
        ERR_NET => "E_NET",
        ERR_CLIPBOARD_BUSY => "E_CLIPBOARD_BUSY",
        ERR_FILE_DENIED => "E_FILE_DENIED",
        ERR_ENCODER_UNAVAILABLE => "E_ENCODER_UNAVAILABLE",
        ERR_PROTOCOL => "E_PROTOCOL",
        _ => "E_UNKNOWN",
    }
}
