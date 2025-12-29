mod proto;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use proto::cookiejar::v1::{GetCookiesRequest, cookie_service_client::CookieServiceClient};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let otlp_exporter = SpanExporter::builder()
        .with_tonic()
        .build()
        .expect("Failed to create OTLP exporter");
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(otlp_exporter)
        .build();
    let tracer = provider.tracer("claude-usage-metrics");

    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true)
        .with_file(true)
        .with_line_number(true);

    tracing_subscriber::registry()
        .with(telemetry)
        .with(fmt_layer)
        .with(EnvFilter::from_default_env())
        .init();

    info!("Starting the application");

    let endpoint = std::env::var("COOKIEJAR_URL")?;
    let mut client = CookieServiceClient::connect(endpoint).await?;

    let request = GetCookiesRequest {
        host: ".claude.ai".to_string(),
    };
    let response: tonic::Response<proto::cookiejar::v1::GetCookiesResponse> = client.get_cookies(request).await?;

    dbg!(&response);

    Ok(())
}
