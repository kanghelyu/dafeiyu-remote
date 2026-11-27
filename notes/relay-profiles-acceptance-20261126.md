# 用户中继池验收（2026-11-26）

## 范围

- 新增 crate `crates/dfy-relay-profiles`：无限档案 CRUD、健康状态、四种选择策略、导入导出、脱敏、原子保存与 Unix 0600 权限。
- `dfy-cli` 新增 `relay-profile` 子命令：`list/add/update/remove/use/select/test/test-all/import/export`。
- `agent`/`view` 新增 `--relay-profile <ID|@current>` 与 `--relay-profiles-config <FILE>`；密钥来自档案，不再暴露在命令行；当前服务器名称与地址在连接时明确打印。
- 配置文件含预共享密钥，已加入 `.gitignore`，Unix 写入时设置 0600；导出默认脱敏，`--with-secrets` 才写出密钥。

## 自动测试

Windows：

- `cargo test -p dfy-relay-profiles -p dfy-cli --lib --bins`：44 + 4 通过 / 0 失败。
- `cargo clippy -p dfy-relay-profiles -p dfy-cli --all-targets -- -D warnings`：通过。
- 覆盖：CRUD/校验、四种策略选择、`@current` 构建 RelayOptions、导出导入往返、Debug 不打印密钥、脱敏导出不含密钥。

## 真实中继功能测试

环境：Windows 11 x64；`dfy-relay 0.1.0` 监听 `127.0.0.1:21117`，TCP 降级通道关闭，admin 仅本机 `127.0.0.1:21119`；`dfy.exe` Debug SHA-256 `45df60d0b168e9bff0d65af962454f527d9364e1483be10a8f0019d6aa7e406b`。

实际命令与结果：

1. `dfy-relay config init --bind 127.0.0.1:21117 --no-tcp --admin 127.0.0.1:21119 --force`：生成随机 32 字节密钥，服务成功启动。
2. `dfy relay-profile add --id local --name 'Local Relay' --addr 127.0.0.1:21117 --key <from-config> --region cn --priority 10 --preferred`：成功。
3. `dfy relay-profile add --id dead --name 'Dead Relay' --addr 127.0.0.1:21118 --priority 50`：自动生成密钥并成功写入；未传入的密钥不会打印到持久日志以外的位置。
4. `dfy relay-profile list`：显示两台服务器、策略 `preferred_first`、当前服务器 `Local Relay (127.0.0.1:21117)`，密钥显示为 `base64:<redacted>`。
5. `dfy relay-profile test --id local --timeout-ms 2000`：`中继 local 可达，BindRequest 往返 2.86ms`；健康状态写回档案。
6. `dfy relay-profile test --id dead --timeout-ms 1000`：按预期失败；失败健康状态写回档案。
7. `dfy relay-profile select --strategy lowest_latency` 与 `use --id local`：当前服务器显示正确。
8. `dfy relay-profile export`：脱敏导出不含原始密钥；`--with-secrets` 导出包含原始密钥并按机密文件写入。
9. 从含密钥导出文件 `import` 到新配置：导入 2 个档案，`list` 显示健康状态随档案保留。

## 2026-11-27 补充：真实带宽/丢包探针

- `relay-profile test` 不再只做 BindRequest 往返：用随机 tag 同时绑定一对 Client/Agent socket，先测配对 RTT，再经中继发送 8×1200B 带标记的探测包，由对端计数，写回 `loss_ppm` 与 `bandwidth_bps`；`list`/`test`/`test-all` 输出均显示这两项。
- Windows Debug `dfy.exe` SHA-256 `eef59d13231b114e360f2a543ebc9c2e8d035ec360047e7472a463f050303a2d`；本机真实中继结果：`配对 2.31ms，丢包 0ppm，带宽 3,840,000bps`，日志 `.tmp/relay-health-probe-run-20261127.log` SHA-256 `2e3b85fc970132a98b1798bd36ea13585c5335007ab5fae5b31efced76f8e945245`；`list` 日志 SHA-256 `c3bf4384973c3eb9250440b1afbba8f32c8939864fa5e2819d0bf31b5dcbbb524`。
- 不可达目标按预期失败并写回失败健康状态（`.tmp/relay-health-probe-dead-20261127.log`）。
- 边界：3.84 Mbps 是 8 包小突发的观测吞吐，**不是**链路容量测量；没有做多服务器对比或长期带宽标定；TCP 降级通道未参与探测。

## 边界

- 已实现的是客户端中继池管理与选择策略；**P2P ↔ Relay 的运行时自动切换由独立议案验收**，本轮没有把它记为完成。
- 当前 profile 主机字段支持 IP 或 DNS（连接时解析）；带宽/丢包已由上述真实探针填充，但口径仍需标定。
- 外层 TLS 策略字段已定义，当前 relay 数据面仍由 QUIC/TLS 保护，尚未实现外层 TLS 管理面/数据面证书协商。
- 图形化中继管理界面尚未实现；CLI 完成的是可验证的配置层。
