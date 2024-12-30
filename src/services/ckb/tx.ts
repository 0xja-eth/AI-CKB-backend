import {cccClient} from "../../core/ccc-client";
import {Address, ClientTransactionResponse} from "@ckb-ccc/core";

export async function getBlockNumber() {
  return await cccClient.getTip();
}

export type TxInfo = {
  txHash: string;
  blockNumber: bigint;
  txIndex: bigint;
  cellIndex: bigint;
  isInput: boolean;
}

export async function findTxs(address: Address, startBlock: number | bigint, endBlock: number | bigint,
                              ...filters: ((txs: TxInfo[]) => TxInfo[])[]) {
  const asyncTxs = await cccClient.findTransactions({
    script: address.script,
    scriptType: "lock",
    scriptSearchMode: "exact",
    filter: { blockRange: [startBlock, endBlock] }
  })

  const res: TxInfo[] = []

  for await (const tx of asyncTxs) res.push(tx);
  // const tx = await cccClient.getTransaction(tx.txHash);

  return filters?.reduce((txs, filter) => filter(txs), res) || res;
}

export function filterInputs(txs: TxInfo[]) {
  // 1. Filter the txs that are inputs
  return txs.filter(tx => !tx.isInput)
    // 2. Filter the txs that are not inputs
    .filter(tx => !txs.find(t => t.txHash === tx.txHash && t.isInput));
}
export function filterOutputs(txs: TxInfo[]) {
  return txs.filter(tx => tx.isInput)
}

export type FetchedTx = {
  txHash: string
  tx: ClientTransactionResponse
  inputAddresses: {address: Address, value: bigint}[]
  outputAddresses: {address: Address, value: bigint}[]
}

export async function fetchTxDetail(txHash: string) {
  const tx = await cccClient.getTransaction(txHash);
  await Promise.all(tx.transaction.inputs.map(input => input.completeExtraInfos(cccClient)))

  const inputAddresses = tx.transaction.inputs.map(
    input => ({
      address: Address.fromScript(input.cellOutput.lock, cccClient),
      value: input.cellOutput.capacity
    })
  )
  const outputAddresses = tx.transaction.outputs.map(
    output => ({
      address: Address.fromScript(output.lock, cccClient),
      value: output.capacity
    })
  )

  return {txHash, tx, inputAddresses, outputAddresses} as FetchedTx
}
