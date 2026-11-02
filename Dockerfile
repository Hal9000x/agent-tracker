# Agent Tracker on a server, where Claude Code cloud sessions can reach it.
# Put HTTPS in front of it (a reverse proxy, or your hosting platform's), then:
#
#   docker build -t agent-tracker .
#   docker run -d -p 127.0.0.1:4318:4318 -v agent-tracker-data:/data \
#     -e AGENT_TRACKER_PASSWORD='choose 12+ characters' agent-tracker --public-url https://tracker.example.com

FROM node:24-slim AS web
WORKDIR /src
COPY web/package.json web/package-lock.json web/
RUN npm --prefix web ci
COPY web web
RUN npm --prefix web run build

FROM rust:1-slim AS daemon
WORKDIR /src
COPY daemon daemon
COPY integrations integrations
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --manifest-path daemon/Cargo.toml

FROM debian:stable-slim
COPY --from=daemon /src/daemon/target/release/agent-tracker /usr/local/bin/agent-tracker
VOLUME /data
EXPOSE 4318
ENTRYPOINT ["agent-tracker", "--host", "0.0.0.0", "--port", "4318", "--data", "/data", "--no-open"]
