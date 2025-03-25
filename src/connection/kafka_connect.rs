use std::env;

use anyhow::Result;
use aws_config::SdkConfig;
use aws_sdk_kafka as kafka;
use aws_sdk_sts::Client;
use dotenv::dotenv;
use rdkafka::{
    consumer::StreamConsumer, error::KafkaError, producer::FutureProducer, ClientConfig,
};
use uuid::Uuid;

pub struct KafkaConnectionConfigs {
    kafka_cluster_arn: String,
    kafka_username: String,
    kafka_password: String,
}

async fn _assume_role(config: &SdkConfig, role_name: String, session_name: Option<String>) {
    let provider = aws_config::sts::AssumeRoleProvider::builder(role_name)
        .session_name(session_name.unwrap_or("swift_dev_session".into()))
        .configure(config)
        .build()
        .await;

    let local_config = aws_config::from_env()
        .credentials_provider(provider)
        .load()
        .await;
    let client = Client::new(&local_config);
    let req = client.get_caller_identity();
    let resp = req.send().await;
    match resp {
        Ok(e) => {
            log::info!(target: "kafka", "UserID :               {}", e.user_id().unwrap_or_default());
            log::info!(target: "kafka", "Account:               {}", e.account().unwrap_or_default());
            log::info!(target: "kafka", "Arn    :               {}", e.arn().unwrap_or_default());
        }
        Err(e) => log::error!(target: "kafka", "{e:?}"),
    }
}

pub struct KafkaClientBuilder {
    config: Option<KafkaConnectionConfigs>,
    is_local: bool,
    uuid: Uuid,
    brokers: String,
}

impl KafkaClientBuilder {
    /// build an AWS kafka client
    pub async fn aws_from_env() -> Self {
        let aws_config = aws_config::load_from_env().await;
        let connection_config = get_kafka_connection_configs_from_env();
        let client = kafka::Client::new(&aws_config);
        let bootstrap_brokers = client
            .get_bootstrap_brokers()
            .cluster_arn(connection_config.kafka_cluster_arn.clone())
            .send()
            .await
            .unwrap();
        let brokers = bootstrap_brokers
            .bootstrap_broker_string_sasl_scram()
            .expect("No brokers found");

        Self {
            config: Some(connection_config),
            uuid: Uuid::new_v4(),
            is_local: false,
            brokers: brokers.to_string(),
        }
    }
    /// build a local kafka client
    pub fn local() -> Self {
        Self {
            config: None,
            uuid: Uuid::new_v4(),
            is_local: true,
            brokers: "0.0.0.0:9092,kafka:9093".into(),
        }
    }
    pub fn uuid(mut self, uuid: Uuid) -> Self {
        self.uuid = uuid;
        self
    }
    /// Make a Kafka consumer from the builder
    pub fn consumer(self) -> Result<StreamConsumer, KafkaError> {
        let mut config = ClientConfig::new();
        if self.is_local {
            config
                .set("bootstrap.servers", self.brokers)
                .set("auto.offset.reset", "latest")
                .set("group.id", "swift-local")
                .set("metadata.max.age.ms", 30_000.to_string())
                .set("topic.metadata.refresh.interval.ms", 10_000.to_string())
        } else {
            let connection_config = self.config.unwrap();
            log::info!("kafka group.id: {:?}", env::var("HOSTNAME").unwrap());
            config
                .set("bootstrap.servers", self.brokers)
                .set("security.protocol", "SASL_SSL")
                .set("sasl.mechanisms", "SCRAM-SHA-512")
                .set("auto.offset.reset", "latest")
                .set("enable.auto.commit", "true")
                .set("auto.commit.interval.ms", "1000")
                .set("sasl.username", connection_config.kafka_username)
                .set("sasl.password", connection_config.kafka_password)
                .set("group.id", env::var("HOSTNAME").unwrap())
                .set("group.instance.id", "consumer-1")
                .set("fetch.wait.max.ms", "100")
        }
        .create()
    }
    /// Make a Kafka producer from the builder
    pub fn producer(self) -> Result<FutureProducer, KafkaError> {
        let mut config = ClientConfig::new();
        if self.is_local {
            config.set("bootstrap.servers", self.brokers).create()
        } else {
            let connection_config = self.config.unwrap();
            config
                .set("bootstrap.servers", self.brokers)
                .set("batch.num.messages", "1")
                .set("security.protocol", "SASL_SSL")
                .set("sasl.mechanisms", "SCRAM-SHA-512")
                .set("sasl.username", connection_config.kafka_username)
                .set("sasl.password", connection_config.kafka_password)
                .create()
        }
    }
}

fn get_kafka_connection_configs_from_env() -> KafkaConnectionConfigs {
    dotenv().ok();
    KafkaConnectionConfigs {
        kafka_cluster_arn: std::env::var("KAFKA_CLUSTER_ARN").unwrap(),
        kafka_username: std::env::var("KAFKA_USERNAME").unwrap(),
        kafka_password: std::env::var("KAFKA_PASSWORD").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;
    use rdkafka::{consumer::Consumer, producer::FutureRecord, util::Timeout, Message};

    use super::*;

    #[ignore]
    #[tokio::test]
    async fn local_kafka_connect() {
        let consumer = KafkaClientBuilder::local().consumer().unwrap();
        let my_topic = "my-topic";
        consumer.subscribe(&[my_topic]).expect("subscribed");
        let producer = KafkaClientBuilder::local().producer().unwrap();

        let message_count = 3;

        tokio::spawn(async move {
            let mut actual_count = 0;
            let mut stream = consumer.stream();
            for _ in 0..message_count {
                if let Some(Ok(msg)) = stream.next().await {
                    dbg!(
                        "consumer got",
                        core::str::from_utf8(msg.key().unwrap_or_default()).unwrap(),
                        core::str::from_utf8(msg.payload().unwrap_or_default()).unwrap()
                    );
                    actual_count += 1;
                }
            }
            assert_eq!(message_count, actual_count);
        });

        for i in 0..message_count {
            let msg = format!("hello world: {i:?}");
            let record = FutureRecord::to(&my_topic).key(b"msg").payload(&msg);
            producer.send(record, Timeout::Never).await.expect("sent");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
