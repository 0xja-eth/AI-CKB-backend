# Stage 1: Build Fiber node
FROM rust:1.73-bullseye as fiber-builder

# 安装必要依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /fiber

COPY fiber/ .
RUN cargo build --release
