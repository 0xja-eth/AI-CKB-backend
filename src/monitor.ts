import dotenv from "dotenv";

dotenv.config();

import cron from "node-cron";
import {FetchedTx, fetchTxDetail, filterInputs, findTxs, getBlockNumber} from "./services/ckb/tx";
import {client, connect} from "./core/redis";
import {getSigner, shannonToCKB} from "./services/ckb/signer";

const LastSyncedBlockKey = "last_synced_block";
const TransactionsKey = "transactions";
const TransactionHashKey = "transaction_hash";

const LockKey = "sync_lock";
const LockExpiry = 60;

async function acquireLock() {
  const result = await client.set(LockKey, "locked", { NX: true, EX: LockExpiry });
  return result === "OK";
}

async function releaseLock() {
  await client.del(LockKey);
}

function txDetail2TransferInfo(txDetail: FetchedTx) {
  const addresses = new Set([
    ...txDetail.inputAddresses.map(input => input.address.toString()),
    ...txDetail.outputAddresses.map(output => output.address.toString())
  ]);
  const balanceChanges = Array.from(addresses).map(address => ({
    address,
    value:
      shannonToCKB(txDetail.outputAddresses
        .filter(output => output.address.toString() === address)
        .reduce((acc, cur) => acc + cur.value, 0n) -
      txDetail.inputAddresses
        .filter(input => input.address.toString() === address)
        .reduce((acc, cur) => acc + cur.value, 0n)).toString()
  }));
  return {
    txHash: txDetail.txHash,
    inputs: txDetail.inputAddresses.map(input => ({
      address: input.address.toString(),
      value: shannonToCKB(input.value).toString()
    })),
    outputs: txDetail.outputAddresses.map(output => ({
      address: output.address.toString(),
      value: shannonToCKB(output.value).toString()
    })),
    balanceChanges
  } as {
    txHash: string;
    inputs: { address: string; value: string }[];
    outputs: { address: string; value: string }[];
    balanceChanges: { address: string; value: string }[];
  }
}

async function syncIncrementalTransactions() {
  try {
    const lockAcquired = await acquireLock();
    if (!lockAcquired) {
      // console.log("Another sync process is running. Exiting...");
      return;
    }

    const currentTip = await getBlockNumber();

    const lastSyncedBlock = await client.get(LastSyncedBlockKey);
    const startBlock = lastSyncedBlock ? Number(lastSyncedBlock) + 1 : 0;

    if (startBlock > currentTip) {
      // console.log("No new blocks to sync.");
      return;
    }

    const address = await getSigner().getAddressObjSecp256k1();
    const txs = await findTxs(address, startBlock, currentTip, filterInputs);
    if (txs.length === 0) return;

    console.log(`Found ${txs.length} new transactions from block ${startBlock} to ${currentTip}.`);

    const txDetails = await Promise.all(txs.map(async tx => fetchTxDetail(tx.txHash)));

    const pipeline = client.multi();

    for (const tx of txDetails) {
      pipeline.hSet(TransactionsKey, tx.txHash, JSON.stringify(txDetail2TransferInfo(tx)));
      pipeline.zAdd(TransactionHashKey, {
        score: Number(tx.tx.blockNumber), value: tx.txHash
      })
    }
    await pipeline.exec();

    await client.set(LastSyncedBlockKey, currentTip.toString());

    console.log(`Synced transactions up to block ${currentTip}.`);
  } catch (error) {
    console.error("Error during syncing:", error);
  } finally {
    await releaseLock();
  }
}

connect().then(() => {
  cron.schedule("*/3 * * * * *", async () => {
    // console.log("Running incremental sync...");
    await syncIncrementalTransactions();
  });

})

