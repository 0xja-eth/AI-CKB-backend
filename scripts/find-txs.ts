import dotenv from "dotenv";

dotenv.config();

import {getSigner} from "../src/ckb/signer";
import {fetchTxDetail, filterInputs, findTxs, getBlockNumber} from "../src/ckb/tx";
import {cccClient} from "../src/core/ccc-client";
import {Address, ccc} from "@ckb-ccc/core";

async function main() {
  const tip = await getBlockNumber()
  const address = await getSigner().getAddressObjSecp256k1()

  const cells = await cccClient.findCellsByLock(address.script, undefined, true)
  for await (const cell of cells) {
    console.log(cell)
  }

  const txs = await findTxs(address, 0, tip, filterInputs)
  const txDetails = await Promise.all(txs.map(async tx => fetchTxDetail(tx.txHash)))

  console.log(txs, txDetails)
}
main();
