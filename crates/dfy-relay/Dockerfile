# SPDX-License-Identifier: AGPL-3.0-or-later
# SPDX-FileCopyrightText: 2026 Dafeiyu Remote contributors
# Copyright (c) 2026 Dafeiyu Remote contributors
#
# 构建上下文必须是仓库根目录（需要 workspace 其他 crates 与 Cargo.lock）：
#   docker build -f crates/dfy-relay/Dockerfile -t dfy-relay:0.1.0 .
# 运行：
#   docker run -d --name dfy-relay --restart unless-stopped \
#     -p 21117:21117/udp -p 21118:21118/tcp \
#     -v /etc/dfy/relay.toml:/etc/dfy/relay.toml:ro \
#     dfy-relay:0.1.0 serve
# 管理面 127.0.0.1:21119 只在容器内可见（不 -p 暴露），查看状态：
#   docker exec dfy-relay wget -qO- http://127.0.0.1:21119/healthz

FROM rust:1-slim-bookworm AS build
WORKDIR /src
# 先拷清单以复用依赖缓存层
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p dfy-relay

FROM debian:bookworm-slim
ARG UID=10001
RUN useradd --system --no-create-home --shell /usr/sbin/nologin --uid "${UID}" dfy-relay \
    && mkdir -p /etc/dfy && chown -R root:dfy-relay /etc/dfy && chmod 0750 /etc/dfy
COPY --from=build /src/target/release/dfy-relay /usr/local/bin/dfy-relay
USER dfy-relay
# 数据面 UDP 21117、TCP 降级 21118；admin 只监听 127.0.0.1
EXPOSE 21117/udp
EXPOSE 21118/tcp
ENTRYPOINT ["dfy-relay"]
CMD ["serve"]