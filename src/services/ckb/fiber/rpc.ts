import axios from 'axios';

// Basic types
export type Currency = 'fibb' | 'fibt' | 'fibd';
export type HashAlgorithm = 'sha256';
export type Hash256 = string;
export type PeerId = string;
export type MultiAddr = string;
export type Pubkey = string;

// Script related types
export interface Script {
  code_hash: string;
  hash_type: 'type' | 'data' | 'data1';
  args: string;
}

// Channel related types
export interface ChannelState {
  state_name: string;
  state_flags: string[];
}

export interface Channel {
  channel_id: Hash256;
  is_public: boolean;
  channel_outpoint: string;
  peer_id: PeerId;
  funding_udt_type_script: Script | null;
  state: ChannelState;
  local_balance: string;
  offered_tlc_balance: string;
  remote_balance: string;
  received_tlc_balance: string;
  latest_commitment_transaction_hash: string;
  created_at: string;
}

export interface ChannelListResponse {
  channels: Channel[];
}

// Invoice related types
export interface InvoiceData {
  timestamp: string;
  payment_hash: Hash256;
  attrs: Array<Record<string, any>>;
}

export interface Invoice {
  currency: Currency;
  amount: string;
  signature: string;
  data: InvoiceData;
}

export interface InvoiceResponse {
  invoice_address: string;
  invoice: Invoice;
}

export type PaymentSessionStatus = 'Success' | 'Inflight' | 'Failed';

export interface PaymentResponse {
  payment_hash: Hash256;
  status: PaymentSessionStatus;
  created_at: string; // hex timestamp
  last_updated_at: string; // hex timestamp
  failed_error?: string;
  fee: string; // hex amount
}

export interface SendPaymentParams {
  target_pubkey?: string;
  amount?: string; // hex amount
  payment_hash?: Hash256;
  final_tlc_expiry_delta?: string; // hex milliseconds
  tlc_expiry_limit?: string; // hex milliseconds
  invoice?: string;
  timeout?: string; // hex seconds
  max_fee_amount?: string; // hex amount
  max_parts?: string; // hex
  keysend?: boolean;
  udt_type_script?: Script;
  allow_self_payment?: boolean;
  dry_run?: boolean;
}

export interface UdtCfgInfos {
  [key: string]: {
    code_hash: string;
    hash_type: string;
    args: string;
  };
}

export interface NodeInfo {
  version: string;
  commit_hash: string;
  public_key: string;
  node_name?: string;
  peer_id: PeerId;
  addresses: MultiAddr[];
  chain_hash: Hash256;
  open_channel_auto_accept_min_ckb_funding_amount: string; // hex
  auto_accept_channel_ckb_funding_amount: string; // hex
  tlc_expiry_delta: string; // hex
  tlc_min_value: string; // hex
  tlc_max_value: string; // hex
  tlc_fee_proportional_millionths: string; // hex
  channel_count: string; // hex
  pending_channel_count: string; // hex
  peers_count: string; // hex
  udt_cfg_infos: UdtCfgInfos;
}

export class FiberRPCClient {
  private rpcUrl: string;
  private counter: number;

  public tlcExpirySecond = 3600; // s
  public invoiceExpirySecond = 3600; // s

  constructor(rpcUrl: string) {
    this.rpcUrl = rpcUrl;
    this.counter = 0;
  }

  private generateId(): number {
    const timestamp = Math.floor(Date.now() / 1000);
    return timestamp * 10 + (this.counter++ % 10);
  }

  public async call<T>(method: string, params: any[]): Promise<T> {
    const response = await axios.post(this.rpcUrl, {
      id: this.generateId(),
      jsonrpc: '2.0',
      method,
      params,
    });

    if (response.data.error) {
      throw new Error(`RPC Error: ${response.data.error.message}`);
    }

    return response.data.result;
  }

  // region RPC base call

  // Peer Module
  async connectPeer(address: MultiAddr, save?: boolean): Promise<void> {
    await this.call<void>('connect_peer', [{
      address,
      save,
    }]);
  }

  async disconnectPeer(peerId: PeerId): Promise<void> {
    await this.call<void>('disconnect_peer', [{
      peer_id: peerId,
    }]);
  }

  // Channel Module
  async openChannel(params: {
    peer_id: PeerId;
    funding_amount: string;
    public?: boolean;
    funding_udt_type_script?: Script;
  }): Promise<{ temporary_channel_id: Hash256 }> {
    return await this.call<{ temporary_channel_id: Hash256 }>('open_channel', [params]);
  }

  async listChannels(params?: {
    peer_id?: PeerId;
    include_closed?: boolean;
  }): Promise<ChannelListResponse> {
    return await this.call<ChannelListResponse>('list_channels', [params || {}]);
  }

  async addTLC(params: {
    channel_id: Hash256;
    amount: string;
    payment_hash: Hash256;
    expiry?: string;
    hash_algorithm?: HashAlgorithm;
  }): Promise<{ tlc_id: string }> {
    const requestParams = {
      ...params,
      expiry: params.expiry || num2Hex(Date.now() + this.tlcExpirySecond * 1000),
      hash_algorithm: params.hash_algorithm || 'sha256',
    };
    return await this.call<{ tlc_id: string }>('add_tlc', [requestParams]);
  }

  async removeTLC(params: {
    channel_id: Hash256;
    tlc_id: string;
    payment_preimage: string;
  }): Promise<void> {
    await this.call<void>('remove_tlc', [{
      channel_id: params.channel_id,
      tlc_id: params.tlc_id,
      reason: {
        payment_preimage: params.payment_preimage,
      },
    }]);
  }

  async shutdownChannel(params: {
    channel_id: Hash256;
    close_script: Script;
    force?: boolean;
    fee_rate: string;
  }): Promise<void> {
    await this.call<void>('shutdown_channel', [params]);
  }

  // Invoice Module
  async newInvoice(params: {
    amount: string;
    description?: string;
    currency: Currency;
    payment_preimage: Hash256;
    expiry?: string;
    hash_algorithm?: HashAlgorithm;
  }): Promise<InvoiceResponse> {
    const requestParams = {
      ...params,
      expiry: params.expiry || num2Hex(this.invoiceExpirySecond), // `0x${this.invoiceExpirySecond.toString(16)}`,
      hash_algorithm: params.hash_algorithm || 'sha256',
    };
    return await this.call<InvoiceResponse>('new_invoice', [requestParams]);
  }

  async parseInvoice(invoice: string): Promise<Invoice> {
    return (await this.call<{invoice: Invoice}>('parse_invoice', [{ invoice }])).invoice;
  }

  // Payment Module
  async sendPayment(params: SendPaymentParams): Promise<PaymentResponse> {
    return await this.call<PaymentResponse>('send_payment', [params]);
  }

  async getPayment(paymentHash: Hash256): Promise<PaymentResponse> {
    return await this.call<PaymentResponse>('get_payment', [{
      payment_hash: paymentHash
    }]);
  }

  // Node Module
  async getNodeInfo(): Promise<NodeInfo> {
    return await this.call<NodeInfo>('node_info', []);
  }

  // endregion
}

// Helper functions
export function ckb2Hex(ckb: number): string {
  return `0x${(ckb * 10 ** 8).toString(16)}`;
}
export function hex2Ckb(hex: string): number {
  return Number(hex) / 10 ** 8;
}

export function num2Hex(num: number): string {
  return `0x${num.toString(16)}`;
}
export function hex2Num(hex: string): number {
  return Number(hex)
}

// Create a singleton instance using the environment variable
const fiberRpcUrl = process.env.FIBER_RPC_URL || 'http://127.0.0.1:8227';
export const fiberClient = new FiberRPCClient(fiberRpcUrl);