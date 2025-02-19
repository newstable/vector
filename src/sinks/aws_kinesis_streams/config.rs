use aws_sdk_kinesis::error::{DescribeStreamError, PutRecordsError, PutRecordsErrorKind};
use aws_sdk_kinesis::types::SdkError;
use aws_sdk_kinesis::Client as KinesisClient;
use futures::FutureExt;
use snafu::Snafu;
use tower::ServiceBuilder;
use vector_config::configurable_component;

use super::service::KinesisResponse;
use crate::{
    aws::{create_client, is_retriable_error, AwsAuthentication, ClientBuilder, RegionOrEndpoint},
    codecs::{Encoder, EncodingConfig},
    config::{
        AcknowledgementsConfig, DataType, GenerateConfig, Input, ProxyConfig, SinkConfig,
        SinkContext,
    },
    sinks::{
        aws_kinesis_streams::{
            request_builder::KinesisRequestBuilder, service::KinesisService, sink::KinesisSink,
        },
        util::{
            retries::RetryLogic, BatchConfig, Compression, ServiceBuilderExt, SinkBatchSettings,
            TowerRequestConfig,
        },
        Healthcheck, VectorSink,
    },
    tls::TlsConfig,
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Snafu)]
enum HealthcheckError {
    #[snafu(display("DescribeStream failed: {}", source))]
    DescribeStreamFailed {
        source: SdkError<DescribeStreamError>,
    },
    #[snafu(display("Stream names do not match, got {}, expected {}", name, stream_name))]
    StreamNamesMismatch { name: String, stream_name: String },
    #[snafu(display(
        "Stream returned does not contain any streams that match {}",
        stream_name
    ))]
    NoMatchingStreamName { stream_name: String },
}

pub struct KinesisClientBuilder;

impl ClientBuilder for KinesisClientBuilder {
    type Config = aws_sdk_kinesis::config::Config;
    type Client = aws_sdk_kinesis::client::Client;
    type DefaultMiddleware = aws_sdk_kinesis::middleware::DefaultMiddleware;

    fn default_middleware() -> Self::DefaultMiddleware {
        aws_sdk_kinesis::middleware::DefaultMiddleware::new()
    }

    fn build(client: aws_smithy_client::Client, config: &aws_types::SdkConfig) -> Self::Client {
        aws_sdk_kinesis::client::Client::with_config(client, config.into())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KinesisDefaultBatchSettings;

impl SinkBatchSettings for KinesisDefaultBatchSettings {
    const MAX_EVENTS: Option<usize> = Some(500);
    const MAX_BYTES: Option<usize> = Some(5_000_000);
    const TIMEOUT_SECS: f64 = 1.0;
}

/// Configuration for the `aws_kinesis_streams` sink.
#[configurable_component(sink("aws_kinesis_streams"))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct KinesisSinkConfig {
    /// The [stream name][stream_name] of the target Kinesis Logs stream.
    ///
    /// [stream_name]: https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/Working-with-log-groups-and-streams.html
    pub stream_name: String,

    /// The log field used as the Kinesis record’s partition key value.
    ///
    /// If not specified, a unique partition key will be generated for each Kinesis record.
    pub partition_key_field: Option<String>,

    #[serde(flatten)]
    pub region: RegionOrEndpoint,

    #[configurable(derived)]
    pub encoding: EncodingConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub compression: Compression,

    #[configurable(derived)]
    #[serde(default)]
    pub batch: BatchConfig<KinesisDefaultBatchSettings>,

    #[configurable(derived)]
    #[serde(default)]
    pub request: TowerRequestConfig,

    #[configurable(derived)]
    pub tls: Option<TlsConfig>,

    #[configurable(derived)]
    #[serde(default)]
    pub auth: AwsAuthentication,

    #[configurable(derived)]
    #[serde(
        default,
        deserialize_with = "crate::serde::bool_or_struct",
        skip_serializing_if = "crate::serde::skip_serializing_if_default"
    )]
    pub acknowledgements: AcknowledgementsConfig,
}

impl KinesisSinkConfig {
    async fn healthcheck(self, client: KinesisClient) -> crate::Result<()> {
        let stream_name = self.stream_name;

        let describe_result = client
            .describe_stream()
            .stream_name(stream_name.clone())
            .set_exclusive_start_shard_id(None)
            .limit(1)
            .send()
            .await;

        match describe_result {
            Ok(resp) => {
                let name = resp
                    .stream_description
                    .and_then(|x| x.stream_name)
                    .unwrap_or_default();
                if name == stream_name {
                    Ok(())
                } else {
                    Err(HealthcheckError::StreamNamesMismatch { name, stream_name }.into())
                }
            }
            Err(source) => Err(HealthcheckError::DescribeStreamFailed { source }.into()),
        }
    }

    pub async fn create_client(&self, proxy: &ProxyConfig) -> crate::Result<KinesisClient> {
        create_client::<KinesisClientBuilder>(
            &self.auth,
            self.region.region(),
            self.region.endpoint()?,
            proxy,
            &self.tls,
            true,
        )
        .await
    }
}

#[async_trait::async_trait]
impl SinkConfig for KinesisSinkConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        let client = self.create_client(&cx.proxy).await?;
        let healthcheck = self.clone().healthcheck(client.clone()).boxed();

        let batch_settings = self.batch.into_batcher_settings()?;

        let request_settings = self.request.unwrap_with(&TowerRequestConfig::default());

        let region = self.region.region();
        let service = ServiceBuilder::new()
            .settings(request_settings, KinesisRetryLogic)
            .service(KinesisService {
                client,
                stream_name: self.stream_name.clone(),
                region,
            });

        let transformer = self.encoding.transformer();
        let serializer = self.encoding.build()?;
        let encoder = Encoder::<()>::new(serializer);

        let request_builder = KinesisRequestBuilder {
            compression: self.compression,
            encoder: (transformer, encoder),
        };

        let sink = KinesisSink {
            batch_settings,

            service,
            request_builder,
            partition_key_field: self.partition_key_field.clone(),
        };
        Ok((VectorSink::from_event_streamsink(sink), healthcheck))
    }

    fn input(&self) -> Input {
        Input::new(self.encoding.config().input_type() & DataType::Log)
    }

    fn acknowledgements(&self) -> &AcknowledgementsConfig {
        &self.acknowledgements
    }
}

impl GenerateConfig for KinesisSinkConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(
            r#"region = "us-east-1"
            stream_name = "my-stream"
            encoding.codec = "json""#,
        )
        .unwrap()
    }
}

#[derive(Debug, Clone)]
struct KinesisRetryLogic;

impl RetryLogic for KinesisRetryLogic {
    type Error = SdkError<PutRecordsError>;
    type Response = KinesisResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        if let SdkError::ServiceError { err, raw: _ } = error {
            if let PutRecordsErrorKind::ProvisionedThroughputExceededException(_) = err.kind {
                return true;
            }
        }
        is_retriable_error(error)
    }
}

#[cfg(test)]
mod tests {
    use crate::sinks::aws_kinesis_streams::config::KinesisSinkConfig;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<KinesisSinkConfig>();
    }
}
