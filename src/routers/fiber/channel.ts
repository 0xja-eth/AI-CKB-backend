
// Connect to peer
import {authMiddleware} from "../auth";
import {Request, Response} from "express";
import {ckb2Hex, fiberClient, hex2Ckb, num2Hex} from "../../ckb/fiber/rpc";
import router from "./index";

router.post("/connect", authMiddleware, async (req: Request, res: Response) => {
  const { address, save = true } = req.body;

  if (!address) {
    return res.status(400).json({
      error: "Peer address is required"
    });
  }

  try {
    await fiberClient.connectPeer(address, save);
    res.json({
      message: "Successfully connected to peer"
    });
  } catch (error) {
    console.error("/fiber/connect", error);
    res.status(500).json({
      error: (error as Error).message
    });
  }
});

// Open channel
router.post("/channel", authMiddleware, async (req: Request, res: Response) => {
  const { peerId = process.env.DEFAULT_PEER_ID, fundingAmount, isPublic = true } = req.body;

  if (!peerId || !fundingAmount) {
    return res.status(400).json({
      error: "Peer ID and funding amount are required"
    });
  }

  const fundingAmountHex = ckb2Hex(fundingAmount)

  try {
    const result = await fiberClient.openChannel({
      peer_id: peerId,
      funding_amount: fundingAmountHex,
      public: isPublic
    });
    res.json({
      message: "Channel creation request sent",
      channelId: result.temporary_channel_id
    });
  } catch (error) {
    console.error("/fiber/channel", error);
    res.status(500).json({
      error: (error as Error).message
    });
  }
});

// List channels
router.get("/channels", authMiddleware, async (req: Request, res: Response) => {
  const { peerId = process.env.DEFAULT_PEER_ID, includeClosed } = req.query;

  try {
    const channels = await fiberClient.listChannels({
      peer_id: peerId as string,
      include_closed: (includeClosed as string)?.toLowerCase() === 'true'
    });

    // Format amounts to CKB
    const formattedChannels = channels.channels.map(channel => ({
      ...channel,
      local_balance: hex2Ckb(channel.local_balance),
      remote_balance: hex2Ckb(channel.remote_balance),
      offered_tlc_balance: hex2Ckb(channel.offered_tlc_balance),
      received_tlc_balance: hex2Ckb(channel.received_tlc_balance),
    }));

    res.json({ channels: formattedChannels });
  } catch (error) {
    console.error("/fiber/channels", error);
    res.status(500).json({
      error: (error as Error).message
    });
  }
});

// // Get default channel
// router.get("/channel", authMiddleware, async (req: Request, res: Response) => {
//   let { peerId = process.env.DEFAULT_PEER_ID, includeClosed } = req.query
//
//   try {
//     const { channels } = await fiberClient.listChannels({
//       peer_id: peerId as string,
//       include_closed: (includeClosed as string).toLowerCase() === 'true'
//     })
//
//     // Format amounts to CKB
//     const formattedChannels = channels.map(channel => ({
//       ...channel,
//       local_balance: hex2Ckb(channel.local_balance),
//       remote_balance: hex2Ckb(channel.remote_balance),
//       offered_tlc_balance: hex2Ckb(channel.offered_tlc_balance),
//       received_tlc_balance: hex2Ckb(channel.received_tlc_balance),
//     }));
//
//     res.json({ channels: formattedChannels[0] });
//   } catch (error) {
//     console.error("/fiber/channels", error);
//     res.status(500).json({
//       error: (error as Error).message
//     });
//   }
// });

// Close channel
router.post("/channel/close", authMiddleware, async (req: Request, res: Response) => {
  let { channelId,
    closeScript = {
      code_hash: process.env.CLOSE_CODE_HASH,
      hash_type: "type",
      args: process.env.CLOSE_ARGS
    }, force = false, feeRate = 1010 } = req.body;

  if (!closeScript) {
    return res.status(400).json({
      error: "Close script are required"
    });
  }

  if (!channelId && process.env.DEFAULT_PEER_ID) { // Use JoyId Channel
    const { channels } = await fiberClient.listChannels({
      peer_id: process.env.DEFAULT_PEER_ID
    })

    channelId = channels[0].channel_id
    console.log(`Using default channel: ${channelId}`)
  }

  if (!channelId) {
    return res.status(400).json({
      error: "Channel ID are required"
    });
  }

  try {
    await fiberClient.shutdownChannel({
      channel_id: channelId,
      close_script: closeScript,
      force,
      fee_rate: num2Hex(feeRate)
    });
    res.json({
      message: "Channel close request sent"
    });
  } catch (error) {
    console.error("/fiber/channel/close", error);
    res.status(500).json({
      error: (error as Error).message
    });
  }
});
