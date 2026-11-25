//! 控制流消息：手写 prost 定义（不使用 protoc / prost-build）。
//!
//! 所有消息都装在 [`Envelope`] 的 oneof 里，经 `control` 双向流传输（帧格式见 [`crate::frame`]）。

/// 控制流信封：一个消息只填一个 oneof 分支。
///
/// tag 分配（DFYP/1，自研定义）：
/// 1 Hello / 2 Challenge / 3 Auth / 4 Welcome / 5 CapabilityAck /
/// 6 Ping / 7 Pong / 8 VideoConfig / 9 MonitorList / 10 InputEvent /
/// 11 Bye / 12 Reject
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Envelope {
    /// 消息体；空 Envelope 视为协议错误。
    #[prost(
        oneof = "envelope::Body",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12"
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
