
// Fiber transfer
import {authMiddleware} from "../auth";
import {Request, Response} from "express";
import {transferByInvoice, TransferError} from "../../ckb/fiber/transfer";
import {fiberClient, hex2Ckb} from "../../ckb/fiber/rpc";
import {shannonToCKB} from "../../ckb/signer";
import router from "./index";
import {wait} from "../../utils/promise";

router.post("/transfer", authMiddleware, async (req: Request, res: Response) => {
  let { invoice, amountInCKB } = req.body;

  try {
    // if (!channelId && process.env.DEFAULT_PEER_ID) { // Use JoyId Channel
    //   const { channels } = await fiberClient.listChannels({
    //     peer_id: process.env.DEFAULT_PEER_ID
    //   })
    //
    //   channelId = channels[0].channel_id
    //   console.log(`Using default channel: ${channelId}`)
    // }

    if (!invoice || !amountInCKB) {
      return res.status(400).json({
        error: "Invoice, amount and channel ID are required"
      });
    }

    let payment = await fiberClient.sendPayment({ invoice })

    while (payment.status == "Inflight") {
      await wait(1000) // 1s
      payment = await fiberClient.getPayment(payment.payment_hash)
    }

    res.json({
      message: "Transfer created",
      payment
    });
  } catch (error) {
    console.error("/fiber/transfer", error);
    const statusCode = error instanceof TransferError ? 400 : 500;
    res.status(statusCode).json({
      error: (error as Error).message
    });
  }
});

// Parse invoice
router.get("/invoice", async (req: Request, res: Response) => {
  const { invoice } = req.query as { invoice: string };

  if (!invoice) {
    return res.status(400).json({ error: "Invoice is required" });
  }

  try {
    const parsedInvoice = await fiberClient.parseInvoice(invoice);
    res.json({
      invoice: {
        ...parsedInvoice,
        amount: hex2Ckb(parsedInvoice.amount).toString(),
      }
    });
  } catch (error) {
    console.error("/fiber/parse-invoice", error);
    res.status(400).json({ error: "Invalid invoice format" });
  }
});