import axios from 'axios';
import { fiberClient } from './ckb/fiber/rpc';

export interface HealthStatus {
  fiberRpc: {
    status: 'up' | 'down';
    latency?: number;
    error?: string;
    nodeInfo?: {
      version: string;
      peer_id: string;
      channel_count: string;
      peers_count: string;
    };
  };
}

export async function checkHealth(): Promise<HealthStatus> {
  const status: HealthStatus = {
    fiberRpc: {
      status: 'down'
    }
  };

  try {
    const start = Date.now();
    const nodeInfo = await fiberClient.getNodeInfo();
    const latency = Date.now() - start;

    status.fiberRpc = {
      status: 'up',
      latency,
      nodeInfo: {
        version: nodeInfo.version,
        peer_id: nodeInfo.peer_id,
        channel_count: nodeInfo.channel_count,
        peers_count: nodeInfo.peers_count
      }
    };
  } catch (error) {
    status.fiberRpc = {
      status: 'down',
      error: error.message
    };
  }

  return status;
}

// let healthCheckInterval: NodeJS.Timer;
//
// export function startHealthCheck(intervalMs: number = 30000) { // 默认30秒检查一次
//   if (healthCheckInterval) {
//     clearInterval(healthCheckInterval);
//   }
//
//   healthCheckInterval = setInterval(async () => {
//     try {
//       const health = await checkHealth();
//
//       if (health.fiberRpc.status === 'down') {
//         console.error('Fiber RPC is down:', health.fiberRpc.error);
//         process.exit(1); // 退出进程，让 Docker 重启容器
//       }
//     } catch (error) {
//       console.error('Health check failed:', error);
//       process.exit(1);
//     }
//   }, intervalMs);
//
//   // 确保进程退出时清理定时器
//   process.on('SIGTERM', () => {
//     clearInterval(healthCheckInterval);
//   });
// }
//
// export function stopHealthCheck() {
//   if (healthCheckInterval) {
//     clearInterval(healthCheckInterval);
//   }
// }
