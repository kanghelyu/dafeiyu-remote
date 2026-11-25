# 部署中继服务器（大肥鱼远程）

中继（relay）是**零知识**的 UDP 会合点：它按 `tag` 把两个客户端的包互相转发，不解析 QUIC、
不终结 TLS。当两端都在 NAT 后面、P2P 打洞不可行时，它是最后一条可用路径。

## 方式一：一键脚本（推荐）

```bash
# 在 VPS 上（Ubuntu 20.04+ / Debian 11+ / Rocky / Alpine）：
curl -fsSL https://raw.githubusercontent.com/kanghelyu/dafeiyu/main/deploy/install-relay.sh | sudo bash
```

脚本会自动：装依赖 → 拉预编译包（失败则回退源码构建）→ 生成 `/etc/dfy-relay/relay.toml`
（随机 `relay_key`）→ 注册并启动 `dfy-relay.service` → 放行防火墙 → 健康检查 → 打印客户端参数。

常用变体：

```bash
# 强制源码构建（无预编译包时）
sudo bash install-relay.sh --from-source
# 自定义端口
sudo bash install-relay.sh --relay-port 21117 --tcp-port 21118
# 非交互
sudo bash install-relay.sh --yes
# 卸载（保留配置）
sudo bash install-relay.sh --uninstall
```

## 方式二：Docker Compose

```bash
git clone https://github.com/kanghelyu/dafeiyu.git && cd dafeiyu
docker compose -f deploy/docker-compose.yml up -d
docker compose -f deploy/docker-compose.yml exec relay cat /etc/dfy-relay/relay.toml
```

镜像内已放开 `21117/udp`、`21118/tcp`；管理端口只绑定容器内 `127.0.0.1:21119`。

## 方式三：手动（Windows / 任意系统）

```powershell
# 生成配置（自动随机 relay_key）
.\dfy-relay.exe config init --out relay.toml
# 前台运行（调试）
.\dfy-relay.exe serve --config relay.toml
# 或注册为服务（Windows，需管理员）
sc.exe create dfy-relay binPath= "C:\path\dfy-relay.exe serve --config C:\path\relay.toml" start= auto
```

## 配置说明（`relay.toml`）

```toml
bind = "0.0.0.0:21117"        # UDP 主端口（QUIC 走这里）
tcp_bind = "0.0.0.0:21118"    # TCP 降级端口（UDP 被封时）
admin = "127.0.0.1:21119"     # 管理 HTTP（/healthz /metrics /status），只绑本机
relay_key = "base64:..."      # 客户端必须一致；错误 key 会被静默拒绝
tag_ttl_s = 172800            # tag 有效期（48h）
max_tags = 4096               # 最大并发配对
per_tag_bps = 50000000        # 每 tag 限速（bit/s）
```

## 客户端接入

```bash
# 被控端
dfy agent --relay <中继IP>:21117 --relay-key '<relay_key>'

# 主控端（同一 relay_key；tag 默认按口令自动派生，无需手动指定）
dfy view --relay <中继IP>:21117 --relay-key '<relay_key>' --clipboard
```

需要固定 tag 时用 `--relay-tag <32位十六进制>`。

## 验收与排障

```bash
curl http://127.0.0.1:21119/healthz      # 应返回 ok
curl http://127.0.0.1:21119/status       # 当前 tag 数、字节数
curl http://127.0.0.1:21119/metrics      # Prometheus 指标
journalctl -u dfy-relay -n 100 --no-pager
```

常见问题：

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 客户端 `AUTH_FAILED` | 两端 `--relay-key` 不一致 | 用同一份 `relay_key` |
| 客户端 `TAG_FULL` | 同一 tag 已有两端 | 换个 tag（或等 48h 过期 / 重启 relay 用不同 tag） |
| 配对成功但无画面 | 防火墙没放行 UDP | 放行 `21117/udp`；或依赖 TCP 降级 `21118/tcp` |
| 管理端口不通 | 只绑 `127.0.0.1` | 在服务器上 `curl`，或改 `admin` 配置 |
| 限速丢包 | `per_tag_bps` 太小 | 调大或设为 0（不限速，视实现） |

## 安全注意

- `relay_key` 只用于 tag 配对鉴权，**不参与媒体加密**；媒体始终是端到端 TLS 1.3。
- 泄露 `relay_key` 的影响是「他人可抢占你的 tag」，不会解密流量；仍建议保密并定期轮换。
- 中继默认无日志落盘（仅 metrics），适合不信任的第三方 VPS。