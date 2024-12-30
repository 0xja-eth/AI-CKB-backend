import {fiberClient, hex2Ckb} from "./rpc";
import { shannonToCKB } from "../signer";

export class TransferError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'TransferError';
  }
}

/**
 * Transfer CKB through Fiber network
 * @param invoice Fiber invoice string
 * @param amountInCKB Amount to transfer (in CKB)
 * @param channelId Channel ID
 * @returns TLC ID for the transfer
 */
export async function transferByInvoice(
  invoice: string,
  amountInCKB: number,
  channelId: string
): Promise<string> {
  try {
    // 1. Parse invoice
    const parsedInvoice = await fiberClient.parseInvoice(invoice);
    
    // 2. Verify amount
    const invoiceAmount = hex2Ckb(parsedInvoice.amount);
    if (invoiceAmount !== amountInCKB) {
      throw new TransferError(
        `Invoice amount does not match transfer amount: Invoice amount ${invoiceAmount} CKB, Transfer amount ${amountInCKB} CKB`
      );
    }

    // 3. Get payment hash
    const paymentHash = parsedInvoice.data.payment_hash;
    if (!paymentHash) {
      throw new TransferError('Payment hash not found in invoice');
    }

    // 4. Add TLC for transfer
    const result = await fiberClient.addTLC({
      channel_id: channelId,
      amount: parsedInvoice.amount, // Use original amount (in Shannon, hex)
      payment_hash: paymentHash,
    });

    return result.tlc_id;
  } catch (error) {
    if (error instanceof TransferError) {
      throw error;
    }
    throw new TransferError(
      `Fiber transfer failed: ${(error as Error).message}`
    );
  }
}
