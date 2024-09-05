//! This module provides an abstraction on top of the various crates we use to communicate with the
//! amazon web services backend for streaming our data.

use anyhow::Context;
use serde::Deserialize;
use std::io;

/// TODO: reference to kvs documentation
const TIMECODE_TYPE: &str = "RELATIVE";

/// TODO: reference to kvs documentation
const AWS_PRODUCER_STREAM_ARN_HEADER: &str = "x-amzn-stream-arn";

/// TODO: reference to kvs documentation
const AWS_PRODUCER_TIMECODE_TYPE_HEADER: &str = "x-amzn-fragment-timecode-type";

/// TODO: reference to kvs documentation
const AWS_KINESIS_SERVICE_NAME: &str = "kinesisvideo";

/// TODO: reference to kvs documentation
const AWS_PRODUCER_START_TIMESTAMP_HEADER: &str = "x-amzn-producer-start-timestamp";

/// This type is used as the schema for JSON messages sent from KVS while we are streaming.
/// Effectively, we are using it here to validate that our network thread is receiving confirming
/// responses.
#[derive(Debug, Deserialize)]
pub struct KVSResponse {
  #[allow(dead_code, clippy::missing_docs_in_private_items)]
  #[serde(rename = "EventType")]
  event_type: String,
}

/// This type is used to bundle up the things we need to actually communicate with the aws backend.
#[derive(Debug, Deserialize)]
pub struct KVSCredentials {
  #[allow(clippy::missing_docs_in_private_items, dead_code)]
  pub key: String,
  #[allow(clippy::missing_docs_in_private_items, dead_code)]
  pub secret: String,
  #[allow(clippy::missing_docs_in_private_items, dead_code)]
  pub region: String,
  #[allow(clippy::missing_docs_in_private_items, dead_code)]
  pub stream_name: String,
  #[allow(clippy::missing_docs_in_private_items, dead_code)]
  pub stream_arn: String,
}

impl KVSCredentials {
  /// Load environment variables into our struct.
  #[allow(dead_code)]
  pub fn from_env() -> io::Result<Self> {
    let stream_name = std::env::var("AWS_STREAM_NAME")
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "missing AWS_STREAM_NAME in env"))?;

    let stream_arn = std::env::var("AWS_STREAM_ARN")
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "missing AWS_STREAM_ARN in env"))?;

    let stream_secret = std::env::var("AWS_STREAM_SECRET_KEY")
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "missing AWS_STREAM_SECRET_KEY in env"))?;

    let stream_key = std::env::var("AWS_STREAM_ACCESS_KEY")
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "missing AWS_STREAM_ACCESS_KEY in env"))?;

    let stream_region = std::env::var("AWS_STREAM_REGION")
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "missing AWS_STREAM_REGION in env"))?;

    Ok(Self {
      key: stream_key,
      region: stream_region,
      secret: stream_secret,
      stream_name,
      stream_arn,
    })
  }
}

/// This method is used to fetch the URL that we should open our TCP/HTTP connection against.
pub async fn fetch_data_endpoint(credentials: &KVSCredentials) -> anyhow::Result<String> {
  let creds =
    aws_sdk_kinesisvideo::config::Credentials::new(&credentials.key, &credentials.secret, None, None, "");

  log::debug!("fetching data endpoint from kvs region '{}'", credentials.region);

  let region = aws_sdk_kinesisvideo::config::Region::new(credentials.region.clone());
  let config = aws_sdk_kinesisvideo::config::Config::builder()
    .region(region)
    .stalled_stream_protection(aws_sdk_kinesisvideo::config::StalledStreamProtectionConfig::disabled())
    .identity_cache(aws_sdk_kinesisvideo::config::IdentityCache::no_cache())
    .behavior_version(aws_sdk_kinesisvideo::config::BehaviorVersion::latest())
    .credentials_provider(creds.clone())
    .build();

  let client = aws_sdk_kinesisvideo::client::Client::from_conf(config);

  client
    .get_data_endpoint()
    .api_name(aws_sdk_kinesisvideo::types::ApiName::PutMedia)
    .stream_name(&credentials.stream_name)
    .send()
    .await
    .with_context(|| "Unable to fetch endpoint")?
    .data_endpoint
    .ok_or_else(|| anyhow::Error::msg("No data endpoint found for stream"))
}

/// This method is used to build the information we will need when establishing our http connection
/// with aws for the actual, underlying tcp connetion that will be sending bytes of image data over
/// the wire.
pub fn build_signed_request(
  kvs_credentials: &KVSCredentials,
  endpoint: &str,
) -> io::Result<http::Request<()>> {
  let credentials = aws_credential_types::Credentials::new(
    kvs_credentials.key.clone(),
    kvs_credentials.secret.clone(),
    None,
    None,
    "",
  );

  let identity = aws_smithy_runtime_api::client::identity::Identity::from(credentials);

  let signing_settings = aws_sigv4::http_request::SigningSettings::default();

  let signing_params = aws_sigv4::sign::v4::SigningParams::builder()
    .identity(&identity)
    .region(&kvs_credentials.region)
    .name(AWS_KINESIS_SERVICE_NAME)
    .time(std::time::SystemTime::now())
    .settings(signing_settings)
    .build()
    .map_err(|error| {
      io::Error::new(
        io::ErrorKind::Other,
        format!("unable to created awks signing params - {error:?}"),
      )
    })?
    .into();

  let endpoint = format!("{endpoint}/putMedia");

  let timestamp = chrono::offset::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

  let headers = [
    (AWS_PRODUCER_START_TIMESTAMP_HEADER, timestamp.as_str()),
    (AWS_PRODUCER_TIMECODE_TYPE_HEADER, TIMECODE_TYPE),
    (AWS_PRODUCER_STREAM_ARN_HEADER, &kvs_credentials.stream_arn),
  ];

  let signable_request = aws_sigv4::http_request::SignableRequest::new(
    "POST",
    &endpoint,
    headers.into_iter(),
    aws_sigv4::http_request::SignableBody::Bytes(&[]),
  )
  .map_err(|error| {
    io::Error::new(
      io::ErrorKind::Other,
      format!("failed creation of signable aws request - {error:?}"),
    )
  })?;

  let (signing_instructions, _signature) = aws_sigv4::http_request::sign(signable_request, &signing_params)
    .map_err(|error| {
      io::Error::new(
        io::ErrorKind::Other,
        format!("failed creating signed request - {error}"),
      )
    })?
    .into_parts();

  let mut signed_request = http::Request::post(&endpoint).body(()).map_err(|error| {
    io::Error::new(
      io::ErrorKind::Other,
      format!("unable to create streaming url - {error}"),
    )
  })?;

  log::trace!("our instructions are {signing_instructions:?}");

  signing_instructions.apply_to_request_http0x(&mut signed_request);

  for (k, v) in headers {
    let value = http::HeaderValue::from_str(v).map_err(|error| {
      io::Error::new(
        io::ErrorKind::Other,
        format!("unable to format '{v}' as an http header - {error:?}"),
      )
    })?;

    signed_request.headers_mut().insert(k, value);
  }

  Ok(signed_request)
}
