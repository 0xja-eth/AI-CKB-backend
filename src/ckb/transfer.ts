import {Address, ccc, Hex} from "@ckb-ccc/core";
import {cccClient} from "../core/ccc-client";
import {getSigner} from "./signer";
import {client} from "../core/redis";

export const ExplorerURL = process.env.EXPLORER_URL || "https://pudge.explorer.nervos.org";

export const CKBTransferLimit = BigInt(process.env.TRANSFER_LIMIT_CKB || "3000");

export const getXUDTTransferLimit = (xudtArgs: Hex) =>
  BigInt(process.env[`TRANSFER_LIMIT_${xudtArgs}`] || "1000");

export const CKBTransferRecordKey = "transfer:ckb";
export const getTransferRecordKey = (xudtArgs: Hex) => `transfer:${xudtArgs}`;

export async function capacityOf(address: string) {
  const addr = await ccc.Address.fromString(address, cccClient);
  return await cccClient.getBalance([addr.script]);
}

export async function capacityOfXUDT(xudtArgs: string, address: string) {
  const addr = await ccc.Address.fromString(address, cccClient);
  const cellsInfo = await getXUDTCells(addr, xudtArgs as Hex);
  return cellsInfo.reduce((acc, {amount}) => acc + amount, 0n);
}

export async function getXUDTCells(address: Address, xudtArgs: Hex) {
  const typeScript = await ccc.Script.fromKnownScript(
    cccClient, ccc.KnownScript.XUdt, xudtArgs
  );

  const res: {cell: ccc.Cell, amount: bigint}[] = [];
  const collector = cccClient.findCellsByLock(address.script, typeScript, true);
  for await (const cell of collector)
    res.push({cell, amount: ccc.numLeFromBytes(cell.outputData)});

  return res;
}

async function checkAndIncrementTransferLimit(key: string, amount: bigint, limit: bigint) {
  const currentHour = new Date().toISOString().slice(0, 13); // e.g., "2024-10-31T10"
  const redisKey = `${key}:${currentHour}`;

  const currentAmount = BigInt(await client.get(redisKey) || "0");
  if (currentAmount + amount > limit) {
    throw new Error("Hourly transfer limit exceeded");
  }

  await client.incrby(redisKey, Number(amount));
  await client.expire(redisKey, 3600); // Set expiration to 1 hour
}

export async function transfer(toAddress: string, amountInCKB: string): Promise<string> {
  // Check limit before transferring
  await checkAndIncrementTransferLimit(CKBTransferRecordKey, BigInt(amountInCKB), CKBTransferLimit);

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
    `Transaction sent. Check it at ${ExplorerURL}/transaction/${txHash}`
  );

  return txHash;
}
export async function transferXUDT(xudtArgs: string, toAddress: string, amountInCKB: string): Promise<string> {
  const recordKey = getTransferRecordKey(xudtArgs as Hex);
  const limit = getXUDTTransferLimit(xudtArgs as Hex);

  // Check limit before transferring
  await checkAndIncrementTransferLimit(recordKey, BigInt(amountInCKB), limit);

  const signer = getSigner()
  const fromLock = (await signer.getAddressObjSecp256k1()).script;
  const address = await ccc.Address.fromString(toAddress, cccClient);
  const { script: toLock } = address

  const xUdtType = await ccc.Script.fromKnownScript(cccClient, ccc.KnownScript.XUdt, xudtArgs);

  const amount = ccc.fixedPointFrom(amountInCKB)
  const tx = ccc.Transaction.from({
    outputs: [{ lock: toLock, type: xUdtType }],
    outputsData: [ccc.numLeToBytes(amount, 16)],
  });
  await tx.completeInputsByUdt(signer, xUdtType);

  const balanceDiff =
    (await tx.getInputsUdtBalance(signer.client, xUdtType)) -
    tx.getOutputsUdtBalance(xUdtType);
  console.log("balanceDiff: ", balanceDiff);
  if (balanceDiff > ccc.Zero) {
    tx.addOutput(
      {
        lock: fromLock,
        type: xUdtType,
      },
      ccc.numLeToBytes(balanceDiff, 16)
    );
  }
  await tx.addCellDepsOfKnownScripts(signer.client, ccc.KnownScript.XUdt);

  await tx.completeInputsByCapacity(signer);
  await tx.completeFeeBy(signer, 1000);

  const txHash = await signer.sendTransaction(tx);
  console.log(
    `Transaction sent. Check it at ${ExplorerURL}/transaction/${txHash}`
  );

  return txHash;
}

// export async function transferTokenToAddress(
//   udtIssuerArgs: string,
//   senderPrivKey: string,
//   amount: string,
//   receiverAddress: string
// ) {
//   const signer = new ccc.SignerCkbPrivateKey(cccClient, senderPrivKey);
//   const senderLockScript = (await signer.getAddressObjSecp256k1()).script;
//   const receiverLockScript = (
//     await ccc.Address.fromString(receiverAddress, cccClient)
//   ).script;
//
//   const xudtArgs = udtIssuerArgs;
//   const xUdtType = await ccc.Script.fromKnownScript(
//     cccClient,
//     ccc.KnownScript.XUdt,
//     xudtArgs
//   );
//
//   const tx = ccc.Transaction.from({
//     outputs: [{ lock: receiverLockScript, type: xUdtType }],
//     outputsData: [ccc.numLeToBytes(amount, 16)],
//   });
//   await tx.completeInputsByUdt(signer, xUdtType);
//
//   const balanceDiff =
//     (await tx.getInputsUdtBalance(signer.client, xUdtType)) -
//     tx.getOutputsUdtBalance(xUdtType);
//   console.log("balanceDiff: ", balanceDiff);
//   if (balanceDiff > ccc.Zero) {
//     tx.addOutput(
//       {
//         lock: senderLockScript,
//         type: xUdtType,
//       },
//       ccc.numLeToBytes(balanceDiff, 16)
//     );
//   }
//   await tx.addCellDepsOfKnownScripts(signer.client, ccc.KnownScript.XUdt);
//
//   // Complete missing parts for transaction
//   await tx.completeInputsByCapacity(signer);
//   await tx.completeFeeBy(signer, 1000);
//
//   const txHash = await signer.sendTransaction(tx);
//   console.log("The transaction hash is", txHash);
//   return { txHash, tx };
// }
