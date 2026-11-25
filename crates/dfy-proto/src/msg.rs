//! 控制流消息：手写 prost 定义（不使用 protoc / prost-build）。
//!
//! 所有消息都装在 [`Envelope`] 的 oneof 里，经 `control` 双向流传输（帧格式见 [`crate::frame`]）。

/// 控制流信封：一个消息只填一个 oneof 分支。
///
/// tag 分配（DFYP/1，自研定义）：
/// 1 Hello / 2 Challenge / 3 Auth / 4 Welcome / 5 CapabilityAck /
/// 6 Ping / 7 Pong / 8 VideoConfig / 9 MonitorList / 10 InputEvent /
/// 11 Bye / 12 Reject / 13 ClipboardFormatList / 14 ClipboardPull /
/// 15 ClipboardDataChunk / 16 ClipboardApplied / 17 FileOffer /
/// 18 FileAccept / 19 FileChunk / 20 FileAck / 21 FileResult
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Envelope {
    /// 消息体；空 Envelope 视为协议错误。
    #[prost(
        oneof = "envelope::Body",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21"
    )]
    pub body: Option<envelope::Body>,
}

/// [`Envelope`] 的消息体 oneof。
pub mod envelope {
    /// 协议消息（DFYP/1 v1 最小集）。
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Body {
        /// 客户端问候：版本 + 客户端名 + 设备公钥。
        #[prost(message, tag = "1")]
        Hello(super::Hello),
        /// 服务端挑战：32B nonce。
        #[prost(message, tag = "2")]
        Challenge(super::Challenge),
        /// 客户端鉴权应答：`HMAC-SHA256(sha256(password), nonce)`（MVP 简化）。
        #[prost(message, tag = "3")]
        Auth(super::Auth),
        /// 服务端欢迎：会话 ID + 能力 + 权限位。
        #[prost(message, tag = "4")]
        Welcome(super::Welcome),
        /// 能力确认。
        #[prost(message, tag = "5")]
        CapabilityAck(super::CapabilityAck),
        /// 心跳请求（5s 间隔）。
        #[prost(message, tag = "6")]
        Ping(super::Ping),
        /// 心跳应答（回显 `ts_us`）。
        #[prost(message, tag = "7")]
        Pong(super::Pong),
        /// 视频参数协商（ABR 时可再次下发）。
        #[prost(message, tag = "8")]
        VideoConfig(super::VideoConfig),
        /// 显示器清单。
        #[prost(message, tag = "9")]
        MonitorList(super::MonitorList),
        /// 输入事件（可靠流；鼠标位移走 input-move DATAGRAM）。
        #[prost(message, tag = "10")]
        InputEvent(super::InputEvent),
        /// 正常/异常结束会话。
        #[prost(message, tag = "11")]
        Bye(super::Bye),
        /// 握手拒绝 + 错误码。
        #[prost(message, tag = "12")]
        Reject(super::Reject),
        /// 剪贴板格式清单（拉模式：只报描述符，不推数据）。
        #[prost(message, tag = "13")]
        ClipboardFormatList(super::ClipboardFormatList),
        /// 请求拉取某个格式的数据（按 32B hash 定位）。
        #[prost(message, tag = "14")]
        ClipboardPull(super::ClipboardPull),
        /// 格式数据分块（128KiB 默认块）。
        #[prost(message, tag = "15")]
        ClipboardDataChunk(super::ClipboardDataChunk),
        /// 已写入确认（防环：来源端忽略自己 hash 的事件）。
        #[prost(message, tag = "16")]
        ClipboardApplied(super::ClipboardApplied),
        /// 文件传输：发送方发起（含元数据与断点信息）。
        #[prost(message, tag = "17")]
        FileOffer(super::FileOffer),
        /// 文件传输：接收方接受/拒绝（含续传偏移）。
        #[prost(message, tag = "18")]
        FileAccept(super::FileAccept),
        /// 文件传输：数据块。
        #[prost(message, tag = "19")]
        FileChunk(super::FileChunk),
        /// 文件传输：接收方流控确认（每 N 块一次）。
        #[prost(message, tag = "20")]
        FileAck(super::FileAck),
        /// 文件传输：最终结果（含 BLAKE3 摘要）。
        #[prost(message, tag = "21")]
        FileResult(super::FileResult),
    }
}

/// `Hello`：客户端 → 服务端，握手第一步。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Hello {
    /// 协议主版本，必须等于 [`crate::consts::PROTO_MAJOR`]。
    #[prost(uint32, tag = "1")]
    pub proto_major: u32,
    /// 协议次版本。
    #[prost(uint32, tag = "2")]
    pub proto_minor: u32,
    /// 客户端名（日志/审计用）。
    #[prost(string, tag = "3")]
    pub client_name: String,
    /// 设备 Ed25519 公钥（32B；v1 只做记录与指纹展示）。
    #[prost(bytes = "vec", tag = "4")]
    pub device_pubkey: Vec<u8>,
}

/// `Challenge`：服务端 → 客户端，32B 随机 nonce。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Challenge {
    /// 一次性随机数（32B）。
    #[prost(bytes = "vec", tag = "1")]
    pub nonce: Vec<u8>,
}

/// `Auth`：客户端 → 服务端，密码证明。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Auth {
    /// `HMAC-SHA256(key = SHA-256(password), msg = nonce)`；MVP 简化，正式版换 Argon2id。
    #[prost(bytes = "vec", tag = "1")]
    pub proof: Vec<u8>,
}

/// `Welcome`：服务端 → 客户端，握手成功。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Welcome {
    /// 会话 ID（16B）。
    #[prost(bytes = "vec", tag = "1")]
    pub session_id: Vec<u8>,
    /// 被控端名。
    #[prost(string, tag = "2")]
    pub agent_name: String,
    /// 能力名清单（见 [`crate::consts::CAPABILITIES`]）。
    #[prost(string, repeated, tag = "3")]
    pub capability: Vec<String>,
    /// 本会话授予的权限位（见 [`crate::consts::PERM_VIEW`] 等）。
    #[prost(uint32, tag = "4")]
    pub permission_bitmap: u32,
}

/// `CapabilityAck`：能力确认（当前无字段，保留扩展位）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CapabilityAck {}

/// `Ping`：心跳请求。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Ping {
    /// 发送时刻（Unix 微秒）。
    #[prost(uint64, tag = "1")]
    pub ts_us: u64,
}

/// `Pong`：心跳应答，回显请求的 `ts_us`。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Pong {
    /// 回显的发送时刻（Unix 微秒）。
    #[prost(uint64, tag = "1")]
    pub ts_us: u64,
}

/// `VideoConfig`：视频参数协商 / 变更（ABR）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct VideoConfig {
    /// 宽（像素）。
    #[prost(uint32, tag = "1")]
    pub width: u32,
    /// 高（像素）。
    #[prost(uint32, tag = "2")]
    pub height: u32,
    /// 帧率。
    #[prost(uint32, tag = "3")]
    pub fps: u32,
    /// 目标码率（kbps）。
    #[prost(uint32, tag = "4")]
    pub bitrate_kbps: u32,
}

/// `MonitorList`：显示器清单。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct MonitorList {
    /// 显示器列表。
    #[prost(message, repeated, tag = "1")]
    pub monitors: Vec<Monitor>,
}

/// 单个显示器描述。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Monitor {
    /// 显示器 ID。
    #[prost(uint32, tag = "1")]
    pub id: u32,
    /// 宽（像素）。
    #[prost(uint32, tag = "2")]
    pub width: u32,
    /// 高（像素）。
    #[prost(uint32, tag = "3")]
    pub height: u32,
    /// DPI（96 = 100% 缩放）。
    #[prost(uint32, tag = "4")]
    pub dpi: u32,
    /// 显示名。
    #[prost(string, tag = "5")]
    pub name: String,
    /// 是否主显示器。
    #[prost(bool, tag = "6")]
    pub primary: bool,
}

/// `InputEvent`：统一输入事件（`spec/DFYP-v1.md` §6）。
///
/// 按键/点击/滚轮/触摸/文本走 `input-events` 可靠流；鼠标位移走 input-move DATAGRAM。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InputEvent {
    /// 事件类别，见 [`crate::consts::INPUT_KIND_KEY`] 等。
    #[prost(uint32, tag = "1")]
    pub kind: u32,
    /// 统一 HID Usage（键盘页 0x07 / 按键页 0x0C）。
    #[prost(uint32, tag = "2")]
    pub hid_usage: u32,
    /// 修饰键位图（Ctrl/Alt/Shift/Meta）。
    #[prost(uint32, tag = "3")]
    pub mods: u32,
    /// 按下/抬起，见 [`crate::consts::INPUT_STATE_DOWN`]。
    #[prost(uint32, tag = "4")]
    pub state: u32,
    /// 归一化绝对坐标 X（0..65535）。
    #[prost(uint32, tag = "5")]
    pub x: u32,
    /// 归一化绝对坐标 Y（0..65535）。
    #[prost(uint32, tag = "6")]
    pub y: u32,
    /// 相对位移 X。
    #[prost(sint32, tag = "7")]
    pub dx: i32,
    /// 相对位移 Y。
    #[prost(sint32, tag = "8")]
    pub dy: i32,
    /// Unicode 码点（0 = 无；仅 TEXT 事件使用）。
    #[prost(uint32, tag = "9")]
    pub unicode: u32,
    /// 事件时间戳（Unix 微秒）。
    #[prost(uint64, tag = "10")]
    pub ts_us: u64,
}

/// 剪贴板格式描述（清单项）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardFormat {
    /// 平台格式 ID（Windows CF_* 或自定义注册格式）。
    #[prost(uint32, tag = "1")]
    pub id: u32,
    /// 内容 BLAKE3（32B），跨端去重与拉取定位用。
    #[prost(bytes = "vec", tag = "2")]
    pub hash: Vec<u8>,
    /// 大小提示（字节，0=未知）。
    #[prost(uint32, tag = "3")]
    pub size_hint: u32,
    /// 格式名（Windows 注册格式名或 "text/plain" 等 MIME）。
    #[prost(string, tag = "4")]
    pub name: String,
}

/// 剪贴板格式清单。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardFormatList {
    /// 来源端单调序号；用于丢弃过期清单。
    #[prost(uint64, tag = "1")]
    pub seq: u64,
    /// 清单项（通常文本+HTML+图片+文件列表）。
    #[prost(message, repeated, tag = "2")]
    pub items: Vec<ClipboardFormat>,
}

/// 拉取请求。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardPull {
    /// 目标格式 hash（32B）。
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
}

/// 格式数据分块。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardDataChunk {
    /// 目标格式 hash（32B）。
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
    /// 本块起始偏移。
    #[prost(uint32, tag = "2")]
    pub offset: u32,
    /// 本块数据（≤128KiB）。
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    /// 是否最后一块（true 时 `offset+data.len()` 为总长）。
    #[prost(bool, tag = "4")]
    pub last: bool,
}

/// 已写入确认。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardApplied {
    /// 已写入的格式 hash（32B）。
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
}

/// 文件传输发起（发送方 → 接收方）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileOffer {
    /// 传输 ID（发送方单调递增）。
    #[prost(uint64, tag = "1")]
    pub transfer_id: u64,
    /// 相对路径（收发两端都做路径净化，禁止 `..` 与绝对路径）。
    #[prost(string, tag = "2")]
    pub name: String,
    /// 文件字节数（目录为 0）。
    #[prost(uint64, tag = "3")]
    pub size: u64,
    /// 修改时间（Unix 毫秒，0=未知）。
    #[prost(uint64, tag = "4")]
    pub last_modified_ms: u64,
    /// 是否为目录（目录先建目录再逐个文件传输）。
    #[prost(bool, tag = "5")]
    pub is_dir: bool,
}

/// 文件传输接受/拒绝（接收方 → 发送方）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileAccept {
    /// 传输 ID。
    #[prost(uint64, tag = "1")]
    pub transfer_id: u64,
    /// 续传起始偏移（接收方已落盘的字节数）。
    #[prost(uint64, tag = "2")]
    pub resume_offset: u64,
    /// false 表示拒绝（例如目标已存在且策略为跳过）。
    #[prost(bool, tag = "3")]
    pub accept: bool,
}

/// 文件数据块（发送方 → 接收方）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileChunk {
    /// 传输 ID。
    #[prost(uint64, tag = "1")]
    pub transfer_id: u64,
    /// 本块在文件内的起始偏移。
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    /// 数据（建议 ≤256KiB，避免阻塞控制流）。
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    /// 是否最后一块。
    #[prost(bool, tag = "4")]
    pub last: bool,
}

/// 文件传输流控确认（接收方 → 发送方）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileAck {
    /// 传输 ID。
    #[prost(uint64, tag = "1")]
    pub transfer_id: u64,
    /// 已确认落盘的累计字节数。
    #[prost(uint64, tag = "2")]
    pub received: u64,
}

/// 文件传输最终结果（接收方 → 发送方）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileResult {
    /// 传输 ID。
    #[prost(uint64, tag = "1")]
    pub transfer_id: u64,
    /// 是否成功（校验通过且已落盘）。
    #[prost(bool, tag = "2")]
    pub ok: bool,
    /// 失败原因（成功时为空）。
    #[prost(string, tag = "3")]
    pub error: String,
    /// 接收内容的 BLAKE3（32B；成功时填）。
    #[prost(bytes = "vec", tag = "4")]
    pub digest: Vec<u8>,
}

/// `Bye`：结束会话。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Bye {
    /// 原因，见 [`crate::consts::BYE_NORMAL`] 等。
    #[prost(uint32, tag = "1")]
    pub reason: u32,
}

/// `Reject`：握手/会话拒绝。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Reject {
    /// 错误码，见 [`crate::consts::ERR_AUTH`] 等。
    #[prost(uint32, tag = "1")]
    pub code: u32,
}

impl Envelope {
    /// 构造 `Hello` 信封。
    pub fn hello(
        proto_major: u32,
        proto_minor: u32,
        client_name: impl Into<String>,
        device_pubkey: Vec<u8>,
    ) -> Self {
        Self {
            body: Some(envelope::Body::Hello(Hello {
                proto_major,
                proto_minor,
                client_name: client_name.into(),
                device_pubkey,
            })),
        }
    }

    /// 构造 `Challenge` 信封。
    pub fn challenge(nonce: Vec<u8>) -> Self {
        Self {
            body: Some(envelope::Body::Challenge(Challenge { nonce })),
        }
    }

    /// 构造 `Auth` 信封。
    pub fn auth(proof: Vec<u8>) -> Self {
        Self {
            body: Some(envelope::Body::Auth(Auth { proof })),
        }
    }

    /// 构造 `Welcome` 信封。
    pub fn welcome(
        session_id: Vec<u8>,
        agent_name: impl Into<String>,
        capability: Vec<String>,
        permission_bitmap: u32,
    ) -> Self {
        Self {
            body: Some(envelope::Body::Welcome(Welcome {
                session_id,
                agent_name: agent_name.into(),
                capability,
                permission_bitmap,
            })),
        }
    }

    /// 构造 `CapabilityAck` 信封。
    pub fn capability_ack() -> Self {
        Self {
            body: Some(envelope::Body::CapabilityAck(CapabilityAck {})),
        }
    }

    /// 构造 `Ping` 信封。
    pub fn ping(ts_us: u64) -> Self {
        Self {
            body: Some(envelope::Body::Ping(Ping { ts_us })),
        }
    }

    /// 构造 `Pong` 信封。
    pub fn pong(ts_us: u64) -> Self {
        Self {
            body: Some(envelope::Body::Pong(Pong { ts_us })),
        }
    }

    /// 构造 `VideoConfig` 信封。
    pub fn video_config(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            body: Some(envelope::Body::VideoConfig(VideoConfig {
                width,
                height,
                fps,
                bitrate_kbps,
            })),
        }
    }

    /// 构造 `Bye` 信封。
    pub fn bye(reason: u32) -> Self {
        Self {
            body: Some(envelope::Body::Bye(Bye { reason })),
        }
    }

    /// 构造 `Reject` 信封。
    pub fn reject(code: u32) -> Self {
        Self {
            body: Some(envelope::Body::Reject(Reject { code })),
        }
    }

    /// 构造 `ClipboardFormatList` 信封。
    pub fn clipboard_format_list(seq: u64, items: Vec<ClipboardFormat>) -> Self {
        Self {
            body: Some(envelope::Body::ClipboardFormatList(ClipboardFormatList {
                seq,
                items,
            })),
        }
    }

    /// 构造 `ClipboardPull` 信封。
    pub fn clipboard_pull(hash: Vec<u8>) -> Self {
        Self {
            body: Some(envelope::Body::ClipboardPull(ClipboardPull { hash })),
        }
    }

    /// 构造 `ClipboardDataChunk` 信封。
    pub fn clipboard_data_chunk(hash: Vec<u8>, offset: u32, data: Vec<u8>, last: bool) -> Self {
        Self {
            body: Some(envelope::Body::ClipboardDataChunk(ClipboardDataChunk {
                hash,
                offset,
                data,
                last,
            })),
        }
    }

    /// 构造 `FileOffer` 信封。
    pub fn file_offer(
        transfer_id: u64,
        name: impl Into<String>,
        size: u64,
        last_modified_ms: u64,
        is_dir: bool,
    ) -> Self {
        Self {
            body: Some(envelope::Body::FileOffer(FileOffer {
                transfer_id,
                name: name.into(),
                size,
                last_modified_ms,
                is_dir,
            })),
        }
    }

    /// 构造 `FileAccept` 信封。
    pub fn file_accept(transfer_id: u64, resume_offset: u64, accept: bool) -> Self {
        Self {
            body: Some(envelope::Body::FileAccept(FileAccept {
                transfer_id,
                resume_offset,
                accept,
            })),
        }
    }

    /// 构造 `FileChunk` 信封。
    pub fn file_chunk(transfer_id: u64, offset: u64, data: Vec<u8>, last: bool) -> Self {
        Self {
            body: Some(envelope::Body::FileChunk(FileChunk {
                transfer_id,
                offset,
                data,
                last,
            })),
        }
    }

    /// 构造 `FileAck` 信封。
    pub fn file_ack(transfer_id: u64, received: u64) -> Self {
        Self {
            body: Some(envelope::Body::FileAck(FileAck {
                transfer_id,
                received,
            })),
        }
    }

    /// 构造 `FileResult` 信封。
    pub fn file_result(
        transfer_id: u64,
        ok: bool,
        error: impl Into<String>,
        digest: Vec<u8>,
    ) -> Self {
        Self {
            body: Some(envelope::Body::FileResult(FileResult {
                transfer_id,
                ok,
                error: error.into(),
                digest,
            })),
        }
    }

    /// 构造 `ClipboardApplied` 信封。
    pub fn clipboard_applied(hash: Vec<u8>) -> Self {
        Self {
            body: Some(envelope::Body::ClipboardApplied(ClipboardApplied { hash })),
        }
    }

    /// 消息短名（日志用）。
    pub fn kind_name(&self) -> &'static str {
        match &self.body {
            Some(envelope::Body::Hello(_)) => "Hello",
            Some(envelope::Body::Challenge(_)) => "Challenge",
            Some(envelope::Body::Auth(_)) => "Auth",
            Some(envelope::Body::Welcome(_)) => "Welcome",
            Some(envelope::Body::CapabilityAck(_)) => "CapabilityAck",
            Some(envelope::Body::Ping(_)) => "Ping",
            Some(envelope::Body::Pong(_)) => "Pong",
            Some(envelope::Body::VideoConfig(_)) => "VideoConfig",
            Some(envelope::Body::MonitorList(_)) => "MonitorList",
            Some(envelope::Body::InputEvent(_)) => "InputEvent",
            Some(envelope::Body::Bye(_)) => "Bye",
            Some(envelope::Body::Reject(_)) => "Reject",
            Some(envelope::Body::ClipboardFormatList(_)) => "ClipboardFormatList",
            Some(envelope::Body::ClipboardPull(_)) => "ClipboardPull",
            Some(envelope::Body::ClipboardDataChunk(_)) => "ClipboardDataChunk",
            Some(envelope::Body::ClipboardApplied(_)) => "ClipboardApplied",
            Some(envelope::Body::FileOffer(_)) => "FileOffer",
            Some(envelope::Body::FileAccept(_)) => "FileAccept",
            Some(envelope::Body::FileChunk(_)) => "FileChunk",
            Some(envelope::Body::FileAck(_)) => "FileAck",
            Some(envelope::Body::FileResult(_)) => "FileResult",
            None => "<empty>",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `encode_to_vec` / `decode` 来自 prost::Message trait，测试内需显式引入。
    use prost::Message as _;

    fn roundtrip(env: &Envelope) -> Envelope {
        let bytes = env.encode_to_vec();
        Envelope::decode(bytes.as_slice()).expect("decode")
    }

    #[test]
    fn hello_roundtrip() {
        let env = Envelope::hello(1, 0, "dfy-demo-client", vec![7u8; 32]);
        let got = roundtrip(&env);
        assert_eq!(got, env);
        match got.body {
            Some(envelope::Body::Hello(h)) => {
                assert_eq!(h.proto_major, 1);
                assert_eq!(h.client_name, "dfy-demo-client");
                assert_eq!(h.device_pubkey.len(), 32);
            }
            other => panic!("unexpected body: {other:?}"),
        }
    }

    #[test]
    fn all_messages_roundtrip() {
        let cases: Vec<Envelope> = vec![
            Envelope::hello(1, 0, "c", vec![1, 2, 3]),
            Envelope::challenge(vec![9u8; 32]),
            Envelope::auth(vec![4u8; 32]),
            Envelope::welcome(vec![0u8; 16], "agent", vec!["a".into(), "b".into()], 0b111),
            Envelope::capability_ack(),
            Envelope::ping(1_700_000_000_000_000),
            Envelope::pong(1_700_000_000_000_000),
            Envelope::video_config(1920, 1080, 60, 8000),
            Envelope {
                body: Some(envelope::Body::MonitorList(MonitorList {
                    monitors: vec![
                        Monitor {
                            id: 1,
                            width: 1920,
                            height: 1080,
                            dpi: 96,
                            name: "Primary".into(),
                            primary: true,
                        },
                        Monitor {
                            id: 2,
                            width: 2560,
                            height: 1440,
                            dpi: 144,
                            name: "Side".into(),
                            primary: false,
                        },
                    ],
                })),
            },
            Envelope {
                body: Some(envelope::Body::InputEvent(InputEvent {
                    kind: crate::consts::INPUT_KIND_MOUSE_MOVE,
                    hid_usage: 0,
                    mods: 0,
                    state: crate::consts::INPUT_STATE_DOWN,
                    x: 32_767,
                    y: 16_383,
                    dx: -12,
                    dy: 7,
                    unicode: 0,
                    ts_us: 42,
                })),
            },
            Envelope::bye(crate::consts::BYE_NORMAL),
            Envelope::reject(crate::consts::ERR_AUTH),
        ];
        for env in cases {
            let got = roundtrip(&env);
            assert_eq!(got, env, "roundtrip mismatch for {}", env.kind_name());
        }
    }

    #[test]
    fn kind_names_are_distinct() {
        assert_eq!(Envelope::ping(0).kind_name(), "Ping");
        assert_eq!(Envelope::bye(0).kind_name(), "Bye");
        assert_eq!(Envelope { body: None }.kind_name(), "<empty>");
    }
}
