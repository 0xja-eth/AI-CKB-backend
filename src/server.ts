import dotenv from "dotenv";

dotenv.config();

import express, {Request, Response} from "express";
import {cccClient} from "./core/ccc-client";
import {capacityOf, capacityOfXUDT, transfer, transferXUDT} from "./ckb/transfer";
import {getAddress, shannonToCKB} from "./ckb/signer";

const AIToken = process.env.AI_TOKEN as string;

const app = express();
app.use(express.json());

app.post("/transfer", async (req: Request, res: Response) => {
  const { toAddress, amountInCKB } = req.body;

  const auth = req.header("Authorization");
  if (auth !== AIToken)
    return res.status(401).json({ error: "Unauthorized" });

  try {
    const txHash = await transfer(toAddress, amountInCKB);
    res.json({ txHash });
  } catch (error) {
    console.error(`/transfer`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

app.post("/transfer/:xudtArgs", async (req: Request, res: Response) => {
  const { xudtArgs } = req.params;
  const { toAddress, amountInCKB } = req.body;

  const auth = req.header("Authorization");
  if (auth !== AIToken)
    return res.status(401).json({ error: "Unauthorized" });

  try {
    const txHash = await transferXUDT(xudtArgs, toAddress, amountInCKB);
    res.json({ txHash });
  } catch (error) {
    console.error(`/transfer`, error);
    res.status(500).json({ error: (error as Error).message });
  }
});

app.get("/balance", async (req: Request, res: Response) => {
  try {
    const address = await getAddress();
    const balance = await cccClient.getBalance([address.script]);
    res.json({ balance: shannonToCKB(balance).toString() });
  } catch (error) {
    console.error(`/balance`, error);
    res.status(500).json({ error: (error as Error).message });
  }
})

app.get("/balance/:xudtArgs", async (req: Request, res: Response) => {
  const { xudtArgs } = req.params;

  try {
    const address = await getAddress();
    const balance = await capacityOfXUDT(xudtArgs, address);
    res.json({ balance: shannonToCKB(balance).toString() });
  } catch (error) {
    console.error(`/balance`, error);
    res.status(500).json({ error: (error as Error).message });
  }
})

// app.get("/balance/:address", async (req: Request, res: Response) => {
//   const { address } = req.params;
//   try {
//     const balance = await capacityOf(address);
//     res.json({ balance:
//         shannonToCKB(balance).toString() });
//   } catch (error) {
//     console.error(`/balance/${address}`, error);
//     res.status(500).json({ error: (error as Error).message });
//   }
// })

app.get("/address", async (req: Request, res: Response) => {
  try {
    const addressObj = await getAddress();
    const address = addressObj.toString()
    res.json({ address });
  } catch (error) {
    console.error("/address", error);
    res.status(500).json({ error: (error as Error).message });
  }
})

const PORT = process.env.PORT || 3000;
app.listen(PORT, () => {
  console.log(`Server is running on port ${PORT}`);
});
