use std::str::FromStr;

use anchor_lang::{
    prelude::borsh::{self},
    AnchorDeserialize, AnchorSerialize, InitSpace, Space,
};
use anyhow::{Context, Result};
use arrayvec::ArrayVec;
use base64::Engine;
use drift_rs::{
    swift_order_subscriber::{deser_signed_msg_type, SignedOrderType},
    types::MarketType,
};
use ed25519_dalek::{PublicKey, Signature, Verifier};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
pub struct IncomingSignedMessage {
    #[serde(deserialize_with = "deser_pubkey", default = "Default::default")]
    pub taker_pubkey: Pubkey,
    #[serde(deserialize_with = "deser_signature")]
    pub signature: Signature,
    #[serde(deserialize_with = "deser_signed_msg_type")]
    pub message: SignedOrderType,
    #[serde(deserialize_with = "deser_pubkey", default = "Default::default")]
    pub signing_authority: Pubkey,
    #[serde(deserialize_with = "deser_pubkey", default = "Default::default")]
    pub taker_authority: Pubkey,
}

impl IncomingSignedMessage {
    /// Verify taker signature against hex encoded `message`
    pub fn verify_signature(&self) -> Result<()> {
        let pubkey = if self.signing_authority != Pubkey::default() {
            PublicKey::from_bytes(self.signing_authority.as_array())
        } else if self.taker_authority != Pubkey::default() {
            PublicKey::from_bytes(self.taker_authority.as_array())
        } else {
            PublicKey::from_bytes(self.taker_pubkey.as_array())
        }?;

        // client hex encodes msg before signing so use that as comparison
        let msg_data = &self.message.to_borsh();
        let mut hex_bytes = vec![0; msg_data.len() * 2]; // 2 hex bytes per msg byte
        let _ = faster_hex::hex_encode(msg_data, &mut hex_bytes).expect("hexified");

        pubkey
            .verify(&hex_bytes, &self.signature)
            .context("Signature did not verify")
    }
    pub fn verify_and_get_signed_message(&self) -> Result<&SignedOrderType> {
        self.verify_signature()?;
        Ok(&self.message)
    }
}

#[derive(AnchorDeserialize, AnchorSerialize, Clone, Debug, InitSpace)]
pub struct OrderMetadataAndMessage {
    pub signing_authority: Pubkey,
    pub taker_authority: Pubkey,
    pub order_message: SignedOrderType,
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
        let taker_order_params = self.order_message.info(&self.taker_authority).order_params;
        let signed_msg_borsh = self.order_message.to_borsh();

        json!({
            "market_type": match taker_order_params.market_type {
                MarketType::Perp => "perp",
                MarketType::Spot => "spot",
            },
            "market_index": taker_order_params.market_index,
            "order_message": faster_hex::hex_string(signed_msg_borsh.as_slice()),
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
    pub message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS: &str = "Order processed";
pub const PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE: &str =
    "Error verifying signed message";
pub const PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD: &str = "Order slot too old";
pub const PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER: &str = "Invalid order";
pub const PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED: &str = "Failed to deliver message";
pub const PROCESS_ORDER_RESPONSE_ERROR_USER_NOT_FOUND: &str = "User not found";
pub const PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY: &str = "Ignore pubkey";

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
    #[serde(deserialize_with = "deser_pubkey", default = "Default::default")]
    pub stake_pubkey: Pubkey,
    #[serde(deserialize_with = "deser_pubkey")]
    pub pubkey: Pubkey,
    #[serde(deserialize_with = "deser_signature")]
    pub signature: Signature,
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

/// Deserialize solana pubkey str
pub fn deser_pubkey<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &str = serde::Deserialize::deserialize(deserializer)?;
    Pubkey::from_str(payload).map_err(serde::de::Error::custom)
}

/// Deserialize solana signature str
pub fn deser_signature<'de, D>(deserializer: D) -> Result<Signature, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let payload: &str = serde::Deserialize::deserialize(deserializer)?;
    let mut buf = [0u8; 64];
    let _wrote = base64::prelude::BASE64_STANDARD
        .decode_slice(payload, &mut buf)
        .map_err(serde::de::Error::custom)?;
    Signature::from_bytes(&buf).map_err(serde::de::Error::custom)
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
    use drift_rs::types::{
        OrderParams, OrderTriggerCondition, OrderType, PositionDirection, PostOnlyParam,
        SignedMsgOrderParamsDelegateMessage, SignedMsgOrderParamsMessage,
    };
    use nanoid::nanoid;

    use super::*;

    #[test]
    fn deserialize_incoming_signed_message_delegated() {
        let message = r#"{
            "market_index": 2,
            "market_type": "perp",
            "message": "42656638c7259e230001010080841e00000000000000000000000000020000000000000000013201bb60507d000000000117c0127c00000000395311d51c1b87fd56c3b5872d1041111e51f399b12d291d981a0ea383407295272108160000000073386c754a4c5a650000",
            "signature": "9G8luwFfeAc25HwXCgaUjrKv6yJHcMFDq4Z4uPXqom5mhwZ63YU5g7p07Kxe/AKSt5A/9OPDh3nN/c9IHjkCDA==",
            "taker_pubkey": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE",
            "signing_authority": "GiMXQkJXLVjScmQDkoLJShBJpTh9SDPvT2AZQq8NyEBf"
        }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        assert!(actual.verify_signature().is_ok());
        assert!(
            actual.signing_authority
                == solana_sdk::pubkey!("GiMXQkJXLVjScmQDkoLJShBJpTh9SDPvT2AZQq8NyEBf")
        );
        if let SignedOrderType::Delegated(signed_msg) = actual.message {
            let expected = SignedMsgOrderParamsDelegateMessage {
                signed_msg_order_params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Short,
                    user_order_id: 0,
                    base_asset_amount: 2000000,
                    price: 0,
                    market_index: 2,
                    reduce_only: false,
                    post_only: PostOnlyParam::None,
                    immediate_or_cancel: false,
                    max_ts: None,
                    trigger_price: None,
                    trigger_condition: OrderTriggerCondition::Above,
                    oracle_price_offset: None,
                    auction_duration: Some(50),
                    auction_start_price: Some(2102419643),
                    auction_end_price: Some(2081603607),
                },
                taker_pubkey: solana_sdk::pubkey!("4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"),
                slot: 369631527,
                uuid: [115, 56, 108, 117, 74, 76, 90, 101],
                take_profit_order_params: None,
                stop_loss_order_params: None,
            };
            assert_eq!(signed_msg, expected);
        } else {
            assert!(false, "unexpected variant");
        }
    }

    #[test]
    fn deserialize_incoming_signed_message() {
        let message = r#"{
            "market_index": 2,
            "market_type": "perp",
            "message": "c8d5a65e2234f55d0001000080841e00000000000000000000000000020000000000000000013201abe72e7c000000000162d06c7d00000000000066190816000000005a645349472d634c0000",
            "signature": "LiwPgg6VXxOWfCI/PGQpv2c2PqDs11zgSrqDCOvHq1S0yvE0KZeQa84u7Pb0tanN2KO4Ac8laT7odaAyWxRDBA==",
            "taker_pubkey": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
        }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        assert!(actual.verify_signature().is_ok());
        assert!(actual.signing_authority == Pubkey::default());
    }

    #[test]
    fn deserialize_incoming_signed_message_with_taker_authority() {
        let message = r#"{
            "market_index": 2,
            "market_type": "perp",
            "message": "c8d5a65e2234f55d0001000080841e00000000000000000000000000020000000000000000013201abe72e7c000000000162d06c7d00000000000066190816000000005a645349472d634c0000",
            "signature": "LiwPgg6VXxOWfCI/PGQpv2c2PqDs11zgSrqDCOvHq1S0yvE0KZeQa84u7Pb0tanN2KO4Ac8laT7odaAyWxRDBA==",
            "taker_authority": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
        }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        assert!(actual.verify_signature().is_ok());
        assert!(actual.signing_authority == Pubkey::default());

        let message = r#"{
            "market_index": 2,
            "market_type": "perp",
            "message": "c8d5a65e2234f55d0001000080841e00000000000000000000000000020000000000000000013201abe72e7c000000000162d06c7d00000000000066190816000000005a645349472d634c0000",
            "signature": "LiwPgg6VXxOWfCI/PGQpv2c2PqDs11zgSrqDCOvHq1S0yvE0KZeQa84u7Pb0tanN2KO4Ac8laT7odaAyWxRDBA==",
            "taker_pubkey": "2Ym3QkbXGEZSLDSERE6zCuar6fMCHTzvmw2He3MSL1s9",
            "taker_authority": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
        }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        assert!(actual.verify_signature().is_ok());
        assert!(actual.signing_authority == Pubkey::default());
    }

    #[test]
    fn deserialize_incoming_signed_message_with_signing_authority() {
        let message = r#"{
            "market_index": 2,
            "market_type": "perp",
            "message": "c8d5a65e2234f55d0001010080841e00000000000000000000000000020000000000000000013201bb60507d000000000117c0127c000000000000272108160000000073386c754a4c5a650000",
            "signature": "H8HRloc2vBdhHyiNK5W/Shv3kVKmIYsHTBzlD2ecyxyOUh7EuysU/Y5AOXZ3IpsMxRyLn6OSAHKEgCIQX4OpDQ==",
            "signing_authority": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE",
            "taker_pubkey": "4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE"
        }"#;

        let actual: IncomingSignedMessage = serde_json::from_str(&message).expect("deserializes");
        dbg!(&actual.message);
        assert!(actual.verify_signature().is_ok());
        assert!(
            actual.signing_authority
                == solana_sdk::pubkey!("4rmhwytmKH1XsgGAUyUUH7U64HS5FtT6gM8HGKAfwcFE")
        );

        if let SignedOrderType::Authority(signed_msg) = actual.message {
            let expected = SignedMsgOrderParamsMessage {
                signed_msg_order_params: OrderParams {
                    order_type: OrderType::Market,
                    market_type: MarketType::Perp,
                    direction: PositionDirection::Short,
                    user_order_id: 0,
                    base_asset_amount: 2000000,
                    price: 0,
                    market_index: 2,
                    reduce_only: false,
                    post_only: PostOnlyParam::None,
                    immediate_or_cancel: false,
                    max_ts: None,
                    trigger_price: None,
                    trigger_condition: OrderTriggerCondition::Above,
                    oracle_price_offset: None,
                    auction_duration: Some(50),
                    auction_start_price: Some(2102419643),
                    auction_end_price: Some(2081603607),
                },
                sub_account_id: 0,
                slot: 369631527,
                uuid: [115, 56, 108, 117, 74, 76, 90, 101],
                take_profit_order_params: None,
                stop_loss_order_params: None,
            };
            assert_eq!(signed_msg, expected);
        } else {
            assert!(false, "unexpected variant");
        }
    }

    #[test]
    fn order_metadata_encode_decode_reflexive() {
        let encoded = OrderMetadataAndMessage {
            signing_authority: Pubkey::new_unique(),
            taker_authority: Pubkey::new_unique(),
            order_message: SignedOrderType::Authority(Default::default()),
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

        let signed_order_message = SignedOrderType::Authority(order_params);

        let order_metadata_json = OrderMetadataAndMessage {
            signing_authority,
            taker_authority,
            order_signature,
            uuid: order_params.uuid,
            order_message: signed_order_message.clone(),
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
            faster_hex::hex_string(signed_order_message.to_borsh().as_slice()),
        );

        assert_eq!(order_metadata_json["ts"], 55555);

        assert_eq!(
            order_metadata_json["signing_authority"],
            signing_authority.to_string(),
        );
    }
}
