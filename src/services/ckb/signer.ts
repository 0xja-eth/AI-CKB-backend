import {ccc} from "@ckb-ccc/core";
import {cccClient} from "../../core/ccc-client";

export function getPrivateKey() {
  return process.env.PRIVATE_KEY as string;
}

export function getSigner() {
  return new ccc.SignerCkbPrivateKey(cccClient, getPrivateKey());
}

export function getAddress() {
  return getSigner().getAddressObjSecp256k1();
}

export function shannonToCKB(amount: bigint) {
  return amount / 100000000n;
  // return (Number(amount) / 100000000);
  // return Number(amount) / 100000000;
}
