import { Router } from 'express';
import { checkHealth } from '../services/health';

const router = Router();

router.get('/check', async (req, res) => {
  try {
    const health = await checkHealth();
    if (health.fiberRpc.status === 'down')
      res.status(500).json(health);
    else
      res.json(health);
  } catch (error) {
    res.status(500).json({
      error: 'Health check failed',
      message: error.message
    });
  }
});

export default router;
