import dotenv from "dotenv";

dotenv.config();

import {getSigner} from "../src/ckb/signer";
import {fetchTxDetail, filterInputs, findTxs, getBlockNumber} from "../src/ckb/tx";
import {cccClient} from "../src/core/ccc-client";
import {Address, ccc, Hex} from "@ckb-ccc/core";

export async function queryIssuedTokenCells(address: Address, xudtArgs: Hex) {
  const typeScript = await ccc.Script.fromKnownScript(
    cccClient,
    ccc.KnownScript.XUdt,
    xudtArgs
  );

  const res: {cell: ccc.Cell, amount: any}[] = [];
  const collector = cccClient.findCellsByLock(address.script, typeScript, true);
  for await (const cell of collector) {
    res.push({cell, amount: ccc.numLeFromBytes(cell.outputData)});
  }
  return res;
}

async function main() {
  const tip = await getBlockNumber()
  const address = await getSigner().getAddressObjSecp256k1()

  // const xudtArgs = "0x2ae639d6233f9b15545573b8e78f38ff7aa6c7bf8ef6460bf1f12d0a76c09c4e" + "";
  const xudtArgs = "0x7de82d61a7eb2ec82b0dc653e558ba120efcbfbb44dac87c12972d05bf25065300000000";

  const cells = await queryIssuedTokenCells(address, xudtArgs as Hex)
  console.log(cells, address.toString())

  // const txs = await findTxs(address, 0, tip, filterInputs)
  // const txDetails = await Promise.all(txs.map(async tx => fetchTxDetail(tx.txHash)))
  //
  // console.log(txs, txDetails)
}
main();
