use std::str::FromStr;

use anchor_lang::{
    prelude::borsh::{self},
    AnchorDeserialize, AnchorSerialize, InitSpace, Space,
};
use anyhow::{Context, Result};
use arrayvec::ArrayVec;
use base64::Engine;
use drift_rs::types::{MarketType, SignedMsgOrderParamsMessage};
use ed25519_dalek::{PublicKey, Signature, Verifier};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

#[derive(AnchorDeserialize, AnchorSerialize, Debug, Clone, InitSpace, Copy, Default, PartialEq)]
pub struct SignedOrderParamsMessageWithPrefix {
    // 8-byte discriminator manually added in drift client for message parsing
    discriminator: [u8; 8],
    pub message: SignedMsgOrderParamsMessage,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
pub struct IncomingSignedMessage {
    #[serde(deserialize_with = "base58_to_array")]
    pub taker_pubkey: [u8; 32],
    #[serde(deserialize_with = "base64_to_array")]
    pub signature: [u8; 64],
    #[serde(deserialize_with = "hex_to_borsh_buf")]
    pub message: BorshBuf<{ SignedMsgOrderParamsMessage::INIT_SPACE + 8 }>,
    #[serde(deserialize_with = "base58_to_array", default = "default_deserialize")]
    pub signing_authority: [u8; 32],
}

impl IncomingSignedMessage {
    /// Verify taker signature against sha256 digest of `message`
    pub fn verify_signature(&self) -> Result<()> {
        let pubkey = if self.signing_authority != [0u8; 32] {
            PublicKey::from_bytes(self.signing_authority.as_ref())
                .context("Invalid signing authority")?
        } else {
            PublicKey::from_bytes(&self.taker_pubkey).context("Invalid taker pubkey")?
        };

        let signature =
            Signature::from_bytes(self.signature.as_slice()).context("Invalid signature")?;

        // client hex encodes msg before signing so we need to use that as comparison
        let msg_data = self.message.data();
        let mut hex_bytes = vec![0u8; msg_data.len() * 2]; // 2 hex bytes per msg byte
        let _ = faster_hex::hex_encode(msg_data, &mut hex_bytes).expect("hexified");

        pubkey
            .verify(hex_bytes.as_ref(), &signature)
            .context("Signature did not verify")
    }
    pub fn verify_and_get_signed_message(&self) -> Result<SignedOrderParamsMessageWithPrefix> {
        self.verify_signature()?;
        self.message
            .deserialize::<SignedOrderParamsMessageWithPrefix>()
            .context("invalid SignedMsgOrderParamsMessage")
    }
}

#[derive(AnchorDeserialize, AnchorSerialize, Clone, Debug, InitSpace)]
pub struct OrderMetadataAndMessage {
    pub signing_authority: Pubkey,
    pub taker_authority: Pubkey,
    pub order_message: SignedOrderParamsMessageWithPrefix,
    pub order_signature: [u8; 64],
    pub uuid: [u8; 8],
    pub ts: u64,
}

impl OrderMetadataAndMessage {
    /// Borsh serialize and base64 encode the message
    pub fn encode(&self) -> String {
        let mut buffer = ArrayVec::<u8, { Self::INIT_SPACE }>::new_const();
        self.serialize(&mut buffer).expect("serialized");
        base64::prelude::BASE64_STANDARD.encode(&buffer)
    }

    /// deserialize base64 encoded, borsh message
    pub fn decode(encoded: &str) -> Result<Self> {
        let mut buffer = [0u8; Self::INIT_SPACE];
        base64::prelude::BASE64_STANDARD
            .decode_slice(encoded, &mut buffer)
            .map_err(|e| anyhow::anyhow!("Failed to decode base64: {e:?}"))?;

        let order_metadata = Self::deserialize(&mut &buffer[..])
            .context("Failed to deserialize OrderMetadataAndMessage")?;

        Ok(order_metadata)
    }

    /// Get the message uuid
    pub fn uuid(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.uuid) }
    }

    pub fn jsonify(&self) -> serde_json::Value {
        let taker_order_params = self.order_message.message.signed_msg_order_params;

        let mut order_buf = ArrayVec::<u8, { SignedMsgOrderParamsMessage::INIT_SPACE }>::new();
        self.order_message
            .serialize(&mut order_buf)
            .expect("serialize SignedOrderParams");

        json!({
            "market_type": match taker_order_params.market_type {
                MarketType::Perp => "perp",
                MarketType::Spot => "spot",
            },
            "market_index": taker_order_params.market_index,
            "order_message": faster_hex::hex_string(order_buf.as_slice()),
            "order_signature": base64::prelude::BASE64_STANDARD.encode(self.order_signature),
            "taker_authority": self.taker_authority.to_string(),
            "signing_authority": self.signing_authority.to_string(),
            "uuid": self.uuid(),
            "ts": self.ts,
        })
    }
}

#[derive(serde::Serialize)]
pub struct ProcessOrderResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SubscribeActions {
    Subscribe,
    Unsubscribe,
}

// For Websocket servers
#[derive(serde::Deserialize, Clone, Debug)]
pub struct WsSubscribeMessage {
    pub action: SubscribeActions,
    #[serde(deserialize_with = "deser_market_type")]
    pub market_type: MarketType,
    pub market_name: String,
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct WsAuthMessage {
    #[serde(deserialize_with = "base58_to_array", default = "default_deserialize")]
    pub stake_pubkey: [u8; 32],
    #[serde(deserialize_with = "base58_to_array")]
    pub pubkey: [u8; 32],
    #[serde(deserialize_with = "base64_to_array")]
    pub signature: [u8; 64],
}

#[derive(serde::Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum WsClientMessage {
    Subscribe(WsSubscribeMessage),
    Auth(WsAuthMessage),
}

#[derive(Clone, Debug)]
pub struct WsMessage<'a> {
    channel: &'a str,
    order: Option<&'a OrderMetadataAndMessage>,
    nonce: Option<&'a str>,
    message: Option<&'a str>,
    error: Option<&'a str>,
}

impl<'a> WsMessage<'a> {
    pub const fn auth() -> Self {
        WsMessage::new("auth")
    }

    pub const fn heartbeat() -> Self {
        WsMessage::new("heartbeat")
    }

    pub const fn subscribe() -> Self {
        WsMessage::new("subscribe")
    }

    pub const fn new(channel: &'a str) -> Self {
        Self {
            channel,
            order: None,
            nonce: None,
            message: None,
            error: None,
        }
    }

    pub fn set_order(mut self, order: &'a OrderMetadataAndMessage) -> Self {
        self.order = Some(order);
        self
    }

    pub fn set_nonce(mut self, nonce: &'a str) -> Self {
        self.nonce = Some(nonce);
        self
    }

    pub fn set_message(mut self, message: &'a str) -> Self {
        self.message = Some(message);
        self
    }

    pub fn set_error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }

    pub fn jsonify(self) -> String {
        let mut message = json!({
            "channel": self.channel,
        });

        if let Some(order_metadata) = self.order {
            message["order"] = order_metadata.jsonify();
        }

        if let Some(nonce) = self.nonce {
            message["nonce"] = json!(nonce);
        }

        if let Some(msg) = self.message {
            message["message"] = json!(msg);
        }

        if let Some(error) = self.error {
            message["error"] = json!(error);
        }

        message.to_string()
    }
}

/// Fixed size, serialized borsh bytes
#[derive(Clone, Debug, PartialEq)]
pub struct BorshBuf<const N: usize> {
    /// number of bytes written in the buffer
    length: usize,
    /// underlying buffer, it should not be used directly
    data: [u8; N],
}

impl<const N: usize> BorshBuf<N> {
    /// Deserialize the buffer as `T`
    pub fn deserialize<T: anchor_lang::AnchorDeserialize>(&self) -> Result<T, std::io::Error> {
        T::deserialize(&mut self.data())
    }
    /// Return the encoded buffer data
    pub fn data(&self) -> &[u8] {
        assert!(self.length < N);
        &self.data[..self.length]
    }
}

/// Deserialize base58 str as fixed size byte array
pub fn base58_to_array<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &str = serde::Deserialize::deserialize(deserializer)?;
    let mut buf = [0u8; N];
    let _wrote = solana_sdk::bs58::decode(payload)
        .onto(&mut buf)
        .map_err(serde::de::Error::custom)?;

    Ok(buf)
}

/// Deserialize base58 str as fixed size byte array
pub fn default_deserialize() -> [u8; 32] {
    [0u8; 32]
}

/// Deserialize base64 str as fixed size byte array
pub fn base64_to_array<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &str = serde::Deserialize::deserialize(deserializer)?;
    let mut buf = [0u8; N];
    let _wrote = base64::prelude::BASE64_STANDARD
        .decode_slice_unchecked(payload, &mut buf)
        .map_err(serde::de::Error::custom)?;

    Ok(buf)
}

/// Deserialize base64 str as fixed size byte array with Borsh helpers
pub fn _base64_to_borsh_buf<'de, D, const N: usize>(
    deserializer: D,
) -> Result<BorshBuf<N>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &str = serde::Deserialize::deserialize(deserializer)?;
    let mut buf = [0u8; N];
    let wrote = base64::prelude::BASE64_STANDARD
        .decode_slice_unchecked(payload, &mut buf)
        .map_err(serde::de::Error::custom)?;

    Ok(BorshBuf {
        data: buf,
        length: wrote,
    })
}

/// Deserialize hex str as fixed size byte array with Borsh helpers
pub fn hex_to_borsh_buf<'de, D, const N: usize>(deserializer: D) -> Result<BorshBuf<N>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &[u8] = serde::Deserialize::deserialize(deserializer)?;
    let mut buf = [0_u8; N];
    faster_hex::hex_decode(payload, &mut buf[..payload.len() / 2])
        .map_err(serde::de::Error::custom)?;

    Ok(BorshBuf {
        data: buf,
        length: payload.len() / 2,
    })
}

pub fn deser_market_type<'de, D>(deserializer: D) -> Result<MarketType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let market_type: &str = serde::Deserialize::deserialize(deserializer)?;
    MarketType::from_str(market_type).map_err(|_| serde::de::Error::custom("perp or spot"))
}

#[cfg(test)]
mod tests {
    use drift_rs::types::{OrderParams, OrderType};
    use nanoid::nanoid;

    use super::*;

    #[test]
    fn deserialize_incoming_signed_message() {
        let message = r#"{
            "market_index":6,
            "market_type":"perp",
            "message": "39656134656332650001010000f2052a01000000000000000000000006000000000000000001c8016fe90b0000000000013ecb0b0000000000000085d02c150000000034525a62354259710000",
            "signature": "z2fOUSo9R4+zYWLXO+JgQL1XLlEmzKOGkgFVwsGs8sEI42mbe1U8saBv2jsUazuHB5ENe/maGLl50YyUzNDMBA==",
            "taker_pubkey": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
            }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        dbg!(actual.message.data);
        assert!(actual.verify_signature().is_ok());
        assert!(actual.signing_authority == [0u8; 32]);
    }

    #[test]
    fn deserialize_incoming_signed_message_with_signing_authority() {
        let message = r#"{
            "market_index":6,
            "market_type":"perp",
            "message": "39656134656332650001010000f2052a01000000000000000000000006000000000000000001c8016fe90b0000000000013ecb0b0000000000000085d02c150000000034525a62354259710000",
            "signature": "z2fOUSo9R4+zYWLXO+JgQL1XLlEmzKOGkgFVwsGs8sEI42mbe1U8saBv2jsUazuHB5ENe/maGLl50YyUzNDMBA==",
            "taker_pubkey": "8YLKoCu7NwqHNS8GzuvA2ibsvLrsg22YMfMDafxh1B15",
            "signing_authority": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
            }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        dbg!(actual.message.data);
        assert!(actual.verify_signature().is_ok());
    }

    #[test]
    fn order_metadata_encode_decode_reflexive() {
        let encoded = OrderMetadataAndMessage {
            signing_authority: Pubkey::new_unique(),
            taker_authority: Pubkey::new_unique(),
            order_message: SignedOrderParamsMessageWithPrefix::default(),
            order_signature: [1u8; 64],
            ts: 55555,
            uuid: nanoid!(8).as_bytes().try_into().unwrap(),
        }
        .encode();
        let order_metadata = OrderMetadataAndMessage::decode(&encoded).unwrap();
        dbg!(base64::prelude::BASE64_STANDARD.encode(
            &order_metadata
                .order_message
                .try_to_vec()
                .unwrap()
                .as_slice()
        ),);
        assert_eq!(order_metadata.encode(), encoded);
    }

    #[test]
    fn order_metadata_jsonify() {
        let taker_authority = Pubkey::new_unique();
        let signing_authority = Pubkey::new_unique();

        let uuid = nanoid!(8).as_bytes().try_into().unwrap();
        let order_signature = [1u8; 64];
        let order_params = SignedMsgOrderParamsMessage {
            sub_account_id: 2,
            signed_msg_order_params: OrderParams {
                market_index: 24,
                market_type: MarketType::Perp,
                base_asset_amount: 123456_789,
                order_type: OrderType::Limit,
                ..Default::default()
            },
            uuid,
            ..Default::default()
        };

        let signed_order_message = SignedOrderParamsMessageWithPrefix {
            discriminator: [0u8; 8],
            message: order_params,
        };

        let order_metadata_json = OrderMetadataAndMessage {
            signing_authority,
            taker_authority,
            order_signature,
            uuid: order_params.uuid,
            order_message: signed_order_message,
            ts: 55555,
        }
        .jsonify();

        dbg!(&order_metadata_json);
        dbg!(order_metadata_json.to_string());

        assert_eq!(
            order_metadata_json["taker_authority"],
            taker_authority.to_string(),
        );

        assert_eq!(
            order_metadata_json["order_signature"],
            base64::prelude::BASE64_STANDARD.encode(order_signature),
        );

        assert_eq!(
            order_metadata_json["order_message"],
            faster_hex::hex_string(signed_order_message.try_to_vec().unwrap().as_slice()),
        );

        assert_eq!(order_metadata_json["ts"], 55555);

        assert_eq!(
            order_metadata_json["signing_authority"],
            signing_authority.to_string(),
        );
    }
}
