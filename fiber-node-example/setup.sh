#!/bin/bash

# 配置
BASE_DIR=..
CONFIG_FILE=./config.yml
FNN_EXECUTABLE=./fnn
CKB_CLI=ckb-cli
NODE_DIR="$BASE_DIR/fiber-node-local"

# 创建节点目录并复制配置和可执行文件
echo "Setting up $NODE_DIR"
mkdir -p "$NODE_DIR/ckb"
cp "$CONFIG_FILE" "$NODE_DIR/config.yml"
cp "$FNN_EXECUTABLE" "$NODE_DIR"

cd "$NODE_DIR"

# 创建钱包
#echo "Creating wallet for node"
#expect << EOF
#  spawn $CKB_CLI account new
#  expect "Password:"
#  send "\r"
#  expect "Repeat password:"
#  send "\r"
#  expect eof
#EOF

ADDRESS_OUTPUT=$($CKB_CLI account list | tail -n 11)
echo "ADDRESS_OUTPUT: $ADDRESS_OUTPUT"

LOCK_ARG=$(echo "$ADDRESS_OUTPUT" | grep -o 'lock_arg: 0x[a-f0-9]\{40\}' | awk '{print $2}')
echo "LOCK_ARG: $LOCK_ARG"

# 导出私钥并处理密码输入
echo "Exporting wallet private key for node"
expect << EOF
  spawn $CKB_CLI account export --lock-arg "$LOCK_ARG" --extended-privkey-path ./ckb/exported-key
  expect "Password:"
  send "\r"
  expect eof
EOF
#$CKB_CLI account export --lock-arg "$LOCK_ARG" --extended-privkey-path ./ckb/exported-key

head -n 1 ./ckb/exported-key > ./ckb/key
echo "Wallet created with lock_arg: $LOCK_ARG"

# 修改节点配置
CONFIG_FILE="$NODE_DIR/config.yml"
PORT=8228
RPC_PORT=8227

sed -i "s|listening_addr: \"/ip4/127.0.0.1/tcp/[0-9]*\"|listening_addr: \"/ip4/127.0.0.1/tcp/$PORT\"|g" "$CONFIG_FILE"
sed -i "s|listening_addr: \"127.0.0.1:[0-9]*\"|listening_addr: \"127.0.0.1:$RPC_PORT\"|g" "$CONFIG_FILE"

# 启动节点
#RUST_LOG=info ./fnn -c config.yml -d . &

# 输出提示
echo "Node setup complete. Check the following details:"
echo "  Config Directory: $NODE_DIR"
echo "  RPC URL: http://127.0.0.1:$RPC_PORT"
echo "  Fiber Port: $PORT"
echo "  Wallet exported key: $NODE_DIR/ckb/key"

# 提示领取测试币
echo "Please visit https://faucet.nervos.org/ to get test CKB for your node."

echo "Run following command to start node:

cd $NODE_DIR
RUST_LOG=info ./fnn -c config.yml -d .
"