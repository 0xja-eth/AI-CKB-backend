docker build -f node.Dockerfile -t fiber-node .
docker run -d -p 8227:8227 -p 8228:8228 \
  -e FIBER_NODE_PATH=./fiber-node --name fiber-node fiber-node