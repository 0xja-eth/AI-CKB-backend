import express, { Router, Request, Response } from "express";
import { cccClient } from "../core/ccc-client";
import { capacityOf, capacityOfXUDT, transfer, transferXUDT } from "../ckb/transfer";
import { getAddress, shannonToCKB } from "../ckb/signer";
import {authMiddleware} from "./auth";

const router = Router();

router.post("/transfer", authMiddleware, async (req: Request, res: Response) => {
  const { toAddress, amountInCKB, ignoreLimit = false } = req.body;

  try {
    const txHash = await transfer(toAddress, amountInCKB, ignoreLimit);
    res.json({ txHash });
  } catch (error) {
    console.error(`/transfer`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

router.post("/transfer/:xudtArgs", authMiddleware, async (req: Request, res: Response) => {
  const { xudtArgs } = req.params;
  const { toAddress, amountInCKB, ignoreLimit = false } = req.body;

  try {
    const txHash = await transferXUDT(xudtArgs, toAddress, amountInCKB, ignoreLimit);
    res.json({ txHash });
  } catch (error) {
    console.error(`/transfer`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

router.get("/balance", async (req: Request, res: Response) => {
  try {
    const address = await getAddress();
    const balance = await cccClient.getBalance([address.script]);
    res.json({ balance: shannonToCKB(balance).toString() });
  } catch (error) {
    console.error(`/balance`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

router.get("/balance/:xudtArgs", async (req: Request, res: Response) => {
  const { xudtArgs } = req.params;

  try {
    const address = await getAddress();
    const balance = await capacityOfXUDT(xudtArgs, address.toString());
    res.json({ balance: shannonToCKB(balance).toString() });
  } catch (error) {
    console.error(`/balance`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

router.get("/address", async (req: Request, res: Response) => {
  try {
    const addressObj = await getAddress();
    const address = addressObj.toString()
    res.json({ address });
  } catch (error) {
    console.error("/address", error);
    res.status(500).json({ error: (error as Error).message });
  }
});

export default router;
