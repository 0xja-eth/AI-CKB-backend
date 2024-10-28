import dotenv from "dotenv";
import express, {Request, Response} from "express";
import {ccc} from "@ckb-ccc/core";
import {cccClient} from "./ccc-client";

dotenv.config();

const AIToken = process.env.AI_TOKEN as string;

const app = express();
app.use(express.json());

export function getPrivateKey() {
  return process.env.PRIVATE_KEY as string;
}
export function getSigner() {
  return new ccc.SignerCkbPrivateKey(cccClient, getPrivateKey());
}
export function getAddress() {
  return getSigner().getAddressObjSecp256k1();
}

export async function capacityOf(address: string) {
  const addr = await ccc.Address.fromString(address, cccClient);
  return await cccClient.getBalance([addr.script]);
}

export function shannonToCKB(amount: bigint){
  return amount / 100000000n;
}

export async function transfer(toAddress: string, amountInCKB: string): Promise<string> {
  const signer = getSigner()
  const address = await ccc.Address.fromString(toAddress, cccClient);
  const { script: toLock } = address

  const tx = ccc.Transaction.from({
    outputs: [{ lock: toLock }],
    outputsData: [],
  });

  tx.outputs.forEach((output, i) => {
    if (output.capacity > ccc.fixedPointFrom(amountInCKB)) {
      throw new Error(`Output ${i} has insufficient capacity to store data`);
    }
    output.capacity = ccc.fixedPointFrom(amountInCKB);
  });

  await tx.completeInputsByCapacity(signer);
  await tx.completeFeeBy(signer, 1000);
  const txHash = await signer.sendTransaction(tx);
  console.log(
    `Transaction sent. Check it at https://pudge.explorer.nervos.org/transaction/${txHash}`
  );

  return txHash;
}

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

app.get("/balance/:address", async (req: Request, res: Response) => {
  const { address } = req.params;
  try {
    const balance = await capacityOf(address);
    res.json({ balance: shannonToCKB(balance).toString() });
  } catch (error) {
    console.error(`/balance/${address}`, error);
    res.status(500).json({ error: (error as Error).message });
  }
})

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
