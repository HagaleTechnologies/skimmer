# MAN-21: lowest-friction non-developer install path, independent of
# native packaging. Multi-stage: build with the full Rust toolchain +
# ALSA dev headers (cpal, manta-input's unconditional audio dependency,
# needs them at build time), ship only the built binary + runtime ALSA
# lib in a slim runtime image.
#
# `hpsdr` is enabled (pure UDP/std, no native dependency -- see
# crates/manta-input/src/hpsdr.rs's own module docs) matching the same
# feature set as the native release binaries (.github/workflows/release.yml).
# `soapy` is deliberately excluded -- see that workflow's comment for why.
#
# Built for linux/amd64 and linux/arm64 via `docker buildx build
# --platform linux/amd64,linux/arm64` (release.yml); a plain `docker build`
# on either architecture also works natively, cross-compiling nothing.

FROM rust:1-slim-bookworm AS builder

# `git` is a BUILD-TIME requirement here, not a convenience: this repo's
# own .cargo/config.toml sets `[net] git-fetch-with-cli = true`, and it is
# copied into the build context by the `COPY . .` below (.dockerignore does
# not exclude .cargo). Under that setting cargo shells out to the `git`
# binary to fetch the coppa-{dsp,audio,channel} git dependencies instead of
# using its built-in libgit2 transport -- and `rust:1-slim-*` ships no git
# (unlike the non-slim `rust:1-*`, which inherits one from buildpack-deps).
# Without it the fetch fails to spawn at all: `could not execute process
# 'git fetch ...' (never executed) / No such file or directory (os error 2)`,
# and `cargo build` exits 101 before compiling a single crate (PR #65
# docker-build failure, MAN-48).
RUN apt-get update && apt-get install --no-install-recommends -y \
    libasound2-dev \
    pkg-config \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .
RUN cargo build --release -p manta-cli --features hpsdr

FROM debian:bookworm-slim

RUN apt-get update && apt-get install --no-install-recommends -y \
    libasound2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /home/manta manta

COPY --from=builder /build/target/release/manta /usr/local/bin/manta

USER manta
WORKDIR /home/manta

# Telnet DX cluster (:7300), JSON/WebSocket spot stream (:7301), Prometheus
# metrics (:7302) -- ARCHITECTURE.md §7/§8. Defaults; override via config.
# Metrics listens on the container's 127.0.0.1 (metrics_bind_addr, MAN-132),
# which a published port cannot reach, unless MANTA_SERVER_METRICS_BIND_ADDR
# is set to 0.0.0.0.
EXPOSE 7300 7301 7302

# No STOPSIGNAL override. This image carried `STOPSIGNAL SIGINT` from PR
# #78 until MAN-85, because manta-cli's `ctrlc` handler was registered for
# SIGINT only and `docker stop`'s default SIGTERM killed the process
# outright -- bypassing manta_engine::listen's cleanup, track finalization
# and the server-drain sequence. Since MAN-85, SIGTERM and SIGINT drive the
# same drain path (since MAN-78 through `signal-hook` for SIGTERM); Docker's
# default stop signal is the correct one again, and retargeting it would
# only hide whether that path still works. With a `[server]` table, SIGHUP
# reloads the `[spot]` lists and each `[[rbn_uplink]]` `dry_run` instead of
# stopping (MAN-78): `docker kill --signal=HUP <container>`.
# `crates/manta-cli/tests/signal_shutdown.rs` keeps both halves honest.

# `docker stop`'s own default grace period (10s on Linux) before SIGKILL
# is SHORTER than manta-cli's own supported graceful-shutdown drain
# window (SHUTDOWN_DRAIN_DEADLINE, 50s -- crates/manta-cli/src/main.rs)
# for a legitimately-slow client's final write (PR #78 review round 5).
# Handling the signal is only half of it: the container can still be
# SIGKILLed mid-drain, dropping the final spots the drain exists to
# preserve. A Dockerfile has no way to change the CALLER's stop grace
# period -- operators must pass it explicitly: `docker stop -t 60
# <container>`, or `--stop-timeout 60` on `docker run`, or the
# equivalent `stop_grace_period`/`terminationGracePeriodSeconds` on
# Compose/Kubernetes. Documented in README's Docker install section too.
#
# MAN-45 remediate (code-review round 19, P1): 60, not the 30 this
# comment and the README previously specified. MAN-45's per-client drain
# work raised SHUTDOWN_DRAIN_DEADLINE from 25s to 50s (a stalled client
# can burn up to 2 * telnet::WRITE_TIMEOUT = 20s finishing one in-flight
# write and then a further CLIENT_DRAIN_DEADLINE = 20s draining its own
# backlog). A 30s caller-side grace period is now SHORTER than the
# window it is supposed to cover, so following it would SIGKILL the
# daemon before it can record the abandoned backlog -- reintroducing the
# exact silent truncation MAN-45 exists to remove. 60 leaves margin over
# the 50s registry-wide deadline; if SHUTDOWN_DRAIN_DEADLINE ever grows
# again, this number and README's must grow with it.
ENTRYPOINT ["manta"]
CMD ["--help"]
