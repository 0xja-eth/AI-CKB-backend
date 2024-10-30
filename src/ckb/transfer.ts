import {Address, ccc, Hex} from "@ckb-ccc/core";
import {cccClient} from "../core/ccc-client";
import {getSigner} from "./signer";

export async function capacityOf(address: string) {
  const addr = await ccc.Address.fromString(address, cccClient);
  return await cccClient.getBalance([addr.script]);
}

export async function capacityOfXUDT(xudtArgs: string, address: string) {
  const cellsInfo = await getXUDTCells(address, xudtArgs);
  return cellsInfo.reduce((acc, {amount}) => acc + amount, 0);
}

export async function getXUDTCells(address: Address, xudtArgs: Hex) {
  const typeScript = await ccc.Script.fromKnownScript(
    cccClient, ccc.KnownScript.XUdt, xudtArgs
  );

  const collected: {cell: ccc.Cell, amount: any}[] = [];
  const collector = cccClient.findCellsByLock(address.script, typeScript, true);
  for await (const cell of collector)
    collected.push({cell, amount: ccc.numLeFromBytes(cell.outputData)});

  return collected;
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
export async function transferXUDT(xudtArgs: string, toAddress: string, amount: string): Promise<string> {
  const signer = getSigner()
  const address = await ccc.Address.fromString(toAddress, cccClient);
  const { script: toLock } = address

  const typeScript = await ccc.Script.fromKnownScript(cccClient, ccc.KnownScript.XUdt, xudtArgs);

  const tx = ccc.Transaction.from({
    outputs: [{ lock: toLock, type: typeScript }],
    outputsData: [ccc.numLeToBytes(amount, 16)],
  });

  await tx.addCellDepsOfKnownScripts(cccClient, ccc.KnownScript.XUdt);
  await tx.completeInputsByCapacity(signer);
  await tx.completeFeeBy(signer, 1000);
  const txHash = await signer.sendTransaction(tx);
  console.log(
    `Transaction sent. Check it at https://pudge.explorer.nervos.org/transaction/${txHash}`
  );

  return txHash;
}
