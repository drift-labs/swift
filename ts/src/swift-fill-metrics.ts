/* eslint-disable @typescript-eslint/no-unused-vars */
import { Keypair } from '@solana/web3.js';
import { decodeUTF8 } from 'tweetnacl-util';
import WebSocket from 'ws';
import nacl from 'tweetnacl';
import { loadKeypair, DriftClient, decodeName, grpcSignedMsgUserOrdersAccountSubscriber, decodeUser, SignedMsgUserOrdersAccount } from '@drift-labs/sdk';
import dotenv from 'dotenv';
import { Connection } from '@solana/web3.js';
import { Wallet } from '@coral-xyz/anchor';

dotenv.config();

export async function runWsListener() {
  const keypair = process.env.PRIVATE_KEY ? loadKeypair(process.env.PRIVATE_KEY) : Keypair.generate();
  const network = process.env.NETWORK || 'devnet';
  const endpoint = process.env.ENDPOINT || 'https://api.devnet.solana.com';
  const wsBaseUrl = network === 'mainnet-beta' ? 'swift.drift.trade' : 'master.swift.drift.trade';
  const ws = new WebSocket(`wss://${wsBaseUrl}/ws?pubkey=` + keypair.publicKey.toBase58(), { perMessageDeflate: true });

  const driftClient = new DriftClient({
    connection: new Connection(endpoint),
    wallet: new Wallet(keypair),
  });
  console.log(`connected: endpoint: ${endpoint}, wsBaseUrl: ${wsBaseUrl}, network: ${network}`);

  await driftClient.subscribe();
  const marketLookup = new Map();
  const perpMarkets = driftClient.getPerpMarketAccounts();
  for (const perpMarketAccount of perpMarkets) {
    console.log(
      perpMarketAccount.marketIndex,
      decodeName(perpMarketAccount.name),
    );
    marketLookup.set(
      perpMarketAccount.marketIndex,
      decodeName(perpMarketAccount.name)
    );
  }

  const landedOrdersStream = new grpcSignedMsgUserOrdersAccountSubscriber({
    grpcConfigs: {
      endpoint: process.env.GRPC_ENDPOINT!,
      token: process.env.GRPC_TOKEN!,
    },
    driftClient,
    commitment: 'finalized',
    decodeFn(name, data) {
      return driftClient.program.coder.types.decode('SignedMsgUserOrdersAccount', data) as SignedMsgUserOrdersAccount
    },
  });
  await landedOrdersStream.subscribe();

  // log that the swift order didn't expire
  landedOrdersStream.eventEmitter.on('newSignedMsgOrderIds', (newIds, authority) => {
    for (const id of newIds) {
      console.log(`placed uuid:${id}`);
    }
  });

  ws.on('open', async () => {
    console.log('Connected to the server');

    ws.on('message', async (data: WebSocket.Data) => {
      const now = +new Date();
      const message = JSON.parse(data.toString());
      // console.log(`got msg ✉️: ${JSON.stringify(message)}`);

      if (message['order']) {
        const order = message['order'];
        const uuid = order['uuid'];
        const marketName = marketLookup.get(order['market_index'])!;
        const { oraclePrice, bid, ask } = await getOracleAndBbo(marketName);
        console.log(`oracle:${oraclePrice},dlobAsk:${ask},dlobBid:${bid},uuid:${uuid},latency:${now - order['ts']}`);
        return;
      }

      if (message['channel'] == 'auth' && message['nonce'] != null) {
        const messageBytes = decodeUTF8(message['nonce']);
        const signature = nacl.sign.detached(messageBytes, keypair.secretKey);
        const signatureBase64 = Buffer.from(signature).toString('base64');
        console.log(signatureBase64);
        ws.send(
          JSON.stringify({
            pubkey: keypair.publicKey.toBase58(),
            signature: signatureBase64,
          })
        );
        return;
      }

      if (message['channel'] == 'auth' && message['message'] == 'Authenticated') {
        for (const market of perpMarkets) {
          if (!('active' in market.status)) {
            console.log(`not subscribing idle market: ${market.marketIndex},${JSON.stringify(market.status)}`);
            continue
          }

          if (!marketLookup.get(market.marketIndex)) {
            throw new Error("market not found")
          }

          const marketName: string = marketLookup.get(market.marketIndex)!;
          if (marketName.includes('BET')) {
            console.log(`not subscribing BET market: ${market.marketIndex},${JSON.stringify(market.status)}`);
            continue;
          }
          console.log(`subscribing market: ${marketName}/${market.marketIndex}`);
          ws.send(
            JSON.stringify({
              action: 'subscribe',
              market_type: 'perp',
              market_name: marketName,
            })
          );
          await new Promise(r => setTimeout(r, 50));
        }
      }
    });

    ws.on('close', () => {
      console.log('Disconnected from the server');
    });

    ws.on('error', (error: Error) => {
      console.error('WebSocket error:', error);
    });
  });
}

runWsListener().then(() => {
  console.log('Done!');
}).catch((err) => {
  console.error(`failed: ${JSON.stringify(err)}`);
})


interface RawDLOBResponse {
  bids: {
    price: string;
    size: string;
    sources: {
      vamm: string;
    };
  }[];
  asks: {
    price: string;
    size: string;
    sources: {
      vamm: string;
    };
  }[];
  marketName: string;
  marketType: string;
  marketIndex: number;
  ts: number;
  slot: number;
  oracle: number;
  oracleData: {
    price: string;
    slot: string;
    confidence: string;
    hasSufficientNumberOfDataPoints: boolean;
    twap: string;
    twapConfidence: string;
  };
  marketSlot: number;
}

interface OracleAndBbo {
  bid: number;
  ask: number;
  oraclePrice: number;
  oracleTwap: number;
}

/**
 * 
 * @param marketName - The name of the market to query (e.g., "JTO-PERP")
 * @returns A promise that resolves to formatted market data
 */
async function getOracleAndBbo(marketName: string): Promise<OracleAndBbo> {
  // TODO: switch with network context
  const baseUrl = "https://dlob.drift.trade/l2";
  const params = new URLSearchParams({
    marketName,
    depth: "1",
    includeOracle: "true",
    includeVamm: "true"
  });

  const url = `${baseUrl}?${params.toString()}`;
  try {
    const response = await fetch(url);

    if (!response.ok) {
      throw new Error(`HTTP error! Status: ${response.status}`);
    }

    const data: RawDLOBResponse = await response.json();

    return {
      bid: Number(data.bids[0]?.price),
      ask: Number(data.asks[0]?.price),
      oraclePrice: Number(data.oracleData.price),
      oracleTwap: Number(data.oracleData.twap),
    };
  } catch (error) {
    console.error("Error fetching dlob data:", error);
    throw error;
  }
}