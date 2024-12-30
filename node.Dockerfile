# Stage 1: Build Fiber node
#FROM rust:1.73-bullseye as fiber-builder
#
## 安装必要依赖
#RUN apt-get update && apt-get install -y --no-install-recommends \
#    clang \
#    libclang-dev \
#    && rm -rf /var/lib/apt/lists/*
#
#WORKDIR /fiber
#
#COPY fiber/ .
#RUN cargo build --release
#
## Stage 2: Node.js app (Debian-based)
#FROM node:18-bullseye
#
#WORKDIR /app
#
## Copy the compiled Rust binary
#COPY --from=fiber-builder /fiber/target/release/fnn /app/fiber-node/fnn
#
## Copy application source
#COPY . .
#
#EXPOSE 8228
#EXPOSE 8227
#
#CMD ["npm", "start-node"]

FROM node:18-bullseye

WORKDIR /app

# Copy package files
COPY package*.json ./
RUN npm install

# Copy application source
COPY . .
COPY pre-compiled/fnn /app/fiber-node/fnn

RUN npm run build

EXPOSE 8228
EXPOSE 8227

CMD ["npm", "start-node"]
