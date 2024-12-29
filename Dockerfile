# Stage 1: Build Fiber node
FROM rust:1.73-alpine as fiber-builder

# 安装必要的依赖（包括 clang, libclang-dev 等）
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /fiber

# Copy Fiber source code
COPY fiber/ .

# Build Fiber in release mode
RUN cargo build --release

# Stage 2: Build and run Node.js application
FROM node:18-alpine

# Create app directory
WORKDIR /app

# Copy Fiber binary from builder stage
COPY --from=fiber-builder /fiber/target/release/fnn /app/fiber-node/fnn

# Copy package files
COPY package*.json ./

# Install dependencies
RUN npm install

# Copy application source
COPY . .

# Build TypeScript
RUN npm run build

# Expose port
EXPOSE 3000
EXPOSE 8228 # Fiber port

# Start application
CMD ["npm", "start"]
