import { BN, DriftClient, getMarketOrderParams, isVariant, loadKeypair, MarketType, PositionDirection, PRICE_PRECISION, SlotSubscriber, Wallet } from "@drift-labs/sdk";
import { Connection, Keypair } from "@solana/web3.js";
import * as axios from 'axios';
import * as dotenv from 'dotenv';

dotenv.config();

async function main() {
  const numOrders = 100_000;
  console.log('starting load test...');
  console.log(`number of orders: ${numOrders}`);

  const privateKey = process.env.PRIVATE_KEY;
  const keypair = privateKey ? loadKeypair(privateKey) : Keypair.generate();

  const wallet = new Wallet(keypair);
  const driftClient = new DriftClient({
    wallet,
    connection: new Connection(process.env.ENDPOINT!),
    env: 'devnet'
  });
  await driftClient.subscribe();

  const slotSubscriber = new SlotSubscriber(driftClient.connection);
  await slotSubscriber.subscribe();

  for (let i = 0; i < numOrders; i++) {
    const slot = slotSubscriber.getSlot() + 100;
    const id = i.toString().padStart(8, '0');
    const marketIndexes = [0];

    const marketIndex =
				marketIndexes[Math.floor(Math.random() * marketIndexes.length)];

      const oracleInfo = driftClient.getOracleDataForPerpMarket(0);
			const highPrice = oracleInfo.price.muln(101).divn(100);
      const lowPrice = oracleInfo.price.muln(99).divn(100);
      const direction = Math.random() > 0.5 ? PositionDirection.LONG : PositionDirection.SHORT;

			const orderMessage = {
				swiftOrderParams: getMarketOrderParams({
					marketIndex,
					marketType: MarketType.PERP,
					direction,
					baseAssetAmount:
						driftClient.getPerpMarketAccount(marketIndex)!.amm
							.minOrderSize,
					auctionStartPrice: isVariant(direction, 'long')
						? lowPrice
						: highPrice,
					auctionEndPrice: isVariant(direction, 'long') ? highPrice : lowPrice,
					auctionDuration: 200,
				}),
				subAccountId: 0,
				slot: new BN(slot),
				uuid: Uint8Array.from(Buffer.from(id)),
				stopLossOrderParams: null,
				takeProfitOrderParams: null,
			};
			const { orderParams: message, signature } =
				driftClient.signSwiftOrderParamsMessage(orderMessage);

			console.log(
				`uuid: ${id} at ${Date.now()}`
			);

			axios.default.post(
				'http://0.0.0.0:3000/orders',
				{
					market_index: marketIndex,
					market_type: 'perp',
					message: message.toString(),
					signature: signature.toString('base64'),
					taker_pubkey: driftClient.wallet.publicKey.toBase58(),
				},
				{
					headers: {
						'Content-Type': 'application/json',
					},
				}
			);
      await new Promise((resolve) => setTimeout(resolve, 100));  
  }
}

main().then(() => {
  console.log('Done!');
});
