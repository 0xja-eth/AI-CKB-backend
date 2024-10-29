import {ccc} from "@ckb-ccc/core";
import {cccClient} from "../core/ccc-client";
import {getSigner} from "./signer";

export async function capacityOf(address: string) {
  const addr = await ccc.Address.fromString(address, cccClient);
  return await cccClient.getBalance([addr.script]);
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
