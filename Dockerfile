# syntax=docker/dockerfile:1.7
# Stage 1: the admin dashboard (Vite build, embedded into the binary by rust-embed).
FROM node:24-alpine AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY ui/ ./
RUN npm run build

# Stage 2: Rust build. Builder and runtime must share a glibc generation: bookworm (2.36) on both sides.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
COPY --from=ui /ui/dist /app/ui/dist
RUN cargo build --release --bin rustllmroute && mkdir -p /app/data

# Stage 3: distroless runtime.
FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /app/target/release/rustllmroute /app/rustllmroute
COPY --from=builder --chown=65532:65532 /app/data /data
COPY config.minimal.yaml /app/config.yaml
ENV PORT=4000 \
    HOST=0.0.0.0 \
    DATABASE_URL=sqlite:///data/rustllmroute.db \
    LITELLM_LOG=info
VOLUME ["/data"]
EXPOSE 4000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD ["/app/rustllmroute", "healthcheck"]
ENTRYPOINT ["/app/rustllmroute"]
CMD ["--config", "/app/config.yaml"]
