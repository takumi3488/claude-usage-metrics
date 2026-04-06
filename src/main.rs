mod proto;

use anyhow::Context;
use chrono::Utc;
use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use proto::cookiejar::v1::{GetCookiesRequest, cookie_service_client::CookieServiceClient};
use seher::claude::ClaudeClient;
use seher::copilot::CopilotClient;
use seher::openrouter::OpenRouterClient;
use tracing::{error, info, instrument};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

// ============================================================================
// Claude Types
// ============================================================================

#[derive(Debug)]
struct UsageMetric {
    name: String,
    utilization: f64,
    seconds_to_reset: Option<i64>,
}

// ============================================================================
// Conversion Functions
// ============================================================================

fn convert_claude_usage(response: &seher::UsageResponse) -> Vec<UsageMetric> {
    let now = Utc::now();
    response
        .all_windows()
        .into_iter()
        .map(|(name, window)| {
            let seconds_to_reset = window
                .resets_at
                .map(|reset_time| (reset_time - now).num_seconds().max(0));
            UsageMetric {
                name: name.to_string(),
                utilization: window.utilization.unwrap_or(0.0),
                seconds_to_reset,
            }
        })
        .collect()
}

// ============================================================================
// Telemetry
// ============================================================================

struct TelemetryProviders {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
}

fn init_telemetry() -> Result<TelemetryProviders, anyhow::Error> {
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "claude-usage-metrics".to_string());
    let resource = Resource::builder().with_service_name(service_name).build();

    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:4317".to_string());

    // Create OTLP span exporter using gRPC (tonic)
    let otlp_exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&otlp_endpoint)
        .with_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to create OTLP span exporter")?;

    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(otlp_exporter)
        .with_resource(resource.clone())
        .build();

    // Create metric exporter using gRPC
    let metric_exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(&otlp_endpoint)
        .with_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to create metric exporter")?;

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();

    global::set_meter_provider(meter_provider.clone());
    global::set_tracer_provider(tracer_provider.clone());

    // Initialize tracing subscriber
    let tracer = tracer_provider.tracer("claude-usage-metrics");
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

    Ok(TelemetryProviders {
        tracer_provider,
        meter_provider,
    })
}

// ============================================================================
// Helpers
// ============================================================================

async fn get_cookies(host: &str) -> anyhow::Result<String> {
    let endpoint =
        std::env::var("COOKIEJAR_URL").context("COOKIEJAR_URL environment variable not set")?;
    let channel = tonic::transport::Channel::from_shared(endpoint.into_bytes())
        .context("Invalid COOKIEJAR_URL")?
        .connect_timeout(std::time::Duration::from_secs(10))
        .connect()
        .await
        .context("Failed to connect to cookie service")?;
    let mut client = CookieServiceClient::new(channel);

    let response = client
        .get_cookies(GetCookiesRequest {
            host: host.to_string(),
        })
        .await
        .context("Failed to get cookies")?;
    Ok(response.into_inner().cookies)
}

// ============================================================================
// Claude Metrics Collection
// ============================================================================

#[instrument(name = "claude_usage_metrics_run", skip_all, err)]
async fn run_claude() -> anyhow::Result<()> {
    info!("Fetching Claude usage metrics");

    let cookies = get_cookies(".claude.ai").await?;

    let org_id = std::env::var("CLAUDE_ORGANIZATION_ID")
        .context("CLAUDE_ORGANIZATION_ID environment variable not set")?;

    let usage_response = ClaudeClient::fetch_usage_with_header(&cookies, &org_id)
        .await
        .context("Failed to fetch Claude usage")?;
    let usage_metrics = convert_claude_usage(&usage_response);

    let meter = global::meter("claude-usage-metrics");
    let utilization_gauge = meter
        .f64_gauge("claude.usage.utilization")
        .with_description("Current Claude usage utilization rate")
        .with_unit("ratio")
        .build();
    let seconds_to_reset_gauge = meter
        .i64_gauge("claude.usage.seconds_to_reset")
        .with_description("Seconds until usage limit resets")
        .with_unit("s")
        .build();

    for metric in &usage_metrics {
        utilization_gauge.record(
            metric.utilization / 100.0,
            &[KeyValue::new("metric_name", metric.name.clone())],
        );
        if let Some(seconds) = metric.seconds_to_reset {
            seconds_to_reset_gauge.record(
                seconds,
                &[KeyValue::new("metric_name", metric.name.clone())],
            );
        }
        info!(
            metric_name = %metric.name,
            utilization = %(metric.utilization / 100.0),
            seconds_to_reset = ?metric.seconds_to_reset,
            "Recorded usage metric"
        );
    }

    Ok(())
}

// ============================================================================
// OpenRouter Metrics Collection
// ============================================================================

#[instrument(name = "openrouter_credits_run", skip_all, err)]
async fn run_openrouter() -> anyhow::Result<()> {
    info!("Fetching OpenRouter credits");

    let api_key = std::env::var("OPENROUTER_API_KEY")
        .context("OPENROUTER_API_KEY environment variable not set")?;

    let response = OpenRouterClient::fetch_credits(&api_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch OpenRouter credits: {e}"))?;

    // Record metrics
    let meter = global::meter("openrouter-credits");

    let total_gauge = meter
        .f64_gauge("openrouter.credits.total")
        .with_description("Total OpenRouter credits purchased")
        .with_unit("USD")
        .build();
    let usage_gauge = meter
        .f64_gauge("openrouter.credits.usage")
        .with_description("Total OpenRouter credits used")
        .with_unit("USD")
        .build();
    let remaining_gauge = meter
        .f64_gauge("openrouter.credits.remaining")
        .with_description("Remaining OpenRouter credits")
        .with_unit("USD")
        .build();

    let remaining = response.data.total_credits - response.data.total_usage;
    total_gauge.record(response.data.total_credits, &[]);
    usage_gauge.record(response.data.total_usage, &[]);
    remaining_gauge.record(remaining, &[]);

    info!(
        total_credits = %response.data.total_credits,
        total_usage = %response.data.total_usage,
        remaining = %remaining,
        "Recorded OpenRouter credits metrics"
    );

    Ok(())
}

// ============================================================================
// GitHub Copilot Metrics Collection
// ============================================================================

#[instrument(name = "github_copilot_quota_run", skip_all, err)]
async fn run_github_copilot() -> anyhow::Result<()> {
    info!("Fetching GitHub Copilot quota");

    let cookies = get_cookies("github.com").await?;

    let quota = CopilotClient::fetch_quota_with_header(&cookies)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch GitHub Copilot quota: {e}"))?;

    let chat_utilization = quota.chat_utilization / 100.0;
    let premium_utilization = quota.premium_utilization / 100.0;
    let seconds_to_reset = quota
        .reset_time
        .map(|rt| (rt - Utc::now()).num_seconds().max(0));

    let meter = global::meter("github-copilot-quota");
    let utilization_gauge = meter
        .f64_gauge("github_copilot.usage.utilization")
        .with_description("GitHub Copilot usage utilization rate")
        .with_unit("ratio")
        .build();
    let seconds_to_reset_gauge = meter
        .i64_gauge("github_copilot.usage.seconds_to_reset")
        .with_description("Seconds until GitHub Copilot quota resets")
        .with_unit("s")
        .build();

    utilization_gauge.record(chat_utilization, &[KeyValue::new("metric_name", "chat")]);
    utilization_gauge.record(
        premium_utilization,
        &[KeyValue::new("metric_name", "premium_interactions")],
    );
    if let Some(seconds) = seconds_to_reset {
        seconds_to_reset_gauge.record(seconds, &[]);
    }

    info!(
        chat_utilization = %chat_utilization,
        premium_interactions_utilization = %premium_utilization,
        seconds_to_reset = ?seconds_to_reset,
        "Recorded GitHub Copilot usage metrics"
    );

    Ok(())
}

// ============================================================================
// Main Run Function
// ============================================================================

#[instrument(name = "all_metrics_run", skip_all, err)]
async fn run() -> anyhow::Result<()> {
    info!("Starting metrics collection");

    let (claude_result, openrouter_result, github_copilot_result) =
        tokio::join!(run_claude(), run_openrouter(), run_github_copilot());

    // Log errors and return combined error if any failed
    let mut errors = Vec::new();
    if let Err(ref e) = claude_result {
        error!(error = %e, "Claude metrics collection failed");
        errors.push(format!("Claude: {e}"));
    }
    if let Err(ref e) = openrouter_result {
        error!(error = %e, "OpenRouter metrics collection failed");
        errors.push(format!("OpenRouter: {e}"));
    }
    if let Err(ref e) = github_copilot_result {
        error!(error = %e, "GitHub Copilot metrics collection failed");
        errors.push(format!("GitHub Copilot: {e}"));
    }

    if !errors.is_empty() {
        anyhow::bail!("Metrics collection failed: {}", errors.join("; "));
    }

    Ok(())
}

// ============================================================================
// Entry Point
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Phase 1: Initialize telemetry (pre-tracing errors go to stderr)
    let providers = match init_telemetry() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Failed to initialize telemetry: {e:#}");
            return Err(e);
        }
    };

    // Phase 2: Run with tracing enabled (errors recorded as spans)
    let result = run().await;
    if let Err(ref e) = result {
        error!(error = %e, "Application error");
    }

    // Phase 3: Shutdown providers (flushes pending data)
    if let Err(e) = providers.tracer_provider.shutdown() {
        eprintln!("Error shutting down tracer provider: {e:?}");
    }
    if let Err(e) = providers.meter_provider.shutdown() {
        eprintln!("Error shutting down meter provider: {e:?}");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use seher::{UsageResponse, UsageWindow};

    #[test]
    fn test_empty_response_returns_empty_vec() {
        let response = UsageResponse {
            five_hour: None,
            seven_day: None,
            seven_day_sonnet: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics = convert_claude_usage(&response);
        assert!(metrics.is_empty());
    }

    #[test]
    fn test_single_field_with_no_reset_time() {
        let response = UsageResponse {
            five_hour: Some(UsageWindow {
                utilization: Some(0.5),
                resets_at: None,
            }),
            seven_day: None,
            seven_day_sonnet: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics = convert_claude_usage(&response);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert!((metrics[0].utilization - 0.5_f64).abs() < f64::EPSILON);
        assert!(metrics[0].seconds_to_reset.is_none());
    }

    #[test]
    fn test_single_field_with_future_reset_time() {
        let future_time = Utc::now() + Duration::seconds(1800);
        let response = UsageResponse {
            five_hour: Some(UsageWindow {
                utilization: Some(0.75),
                resets_at: Some(future_time),
            }),
            seven_day: None,
            seven_day_sonnet: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics = convert_claude_usage(&response);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert!((metrics[0].utilization - 0.75_f64).abs() < f64::EPSILON);
        // Allow 1 second margin for test execution time
        let Some(seconds) = metrics[0].seconds_to_reset else {
            panic!("seconds_to_reset should be Some");
        };
        assert!((1799..=1800).contains(&seconds));
    }

    #[test]
    fn test_past_reset_time_returns_zero() {
        let past_time = Utc::now() - Duration::minutes(10);
        let response = UsageResponse {
            five_hour: Some(UsageWindow {
                utilization: Some(1.0),
                resets_at: Some(past_time),
            }),
            seven_day: None,
            seven_day_sonnet: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics = convert_claude_usage(&response);
        assert_eq!(metrics[0].seconds_to_reset, Some(0));
    }

    #[test]
    fn test_multiple_fields_preserves_order() {
        let response = UsageResponse {
            five_hour: Some(UsageWindow {
                utilization: Some(0.1),
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: Some(0.2),
                resets_at: None,
            }),
            seven_day_sonnet: None,
            seven_day_oauth_apps: None,
            seven_day_opus: Some(UsageWindow {
                utilization: Some(0.3),
                resets_at: None,
            }),
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: Some(UsageWindow {
                utilization: Some(0.4),
                resets_at: None,
            }),
        };
        let metrics = convert_claude_usage(&response);
        assert_eq!(metrics.len(), 4);
        assert_eq!(metrics[0].name, "five_hour");
        assert!((metrics[0].utilization - 0.1_f64).abs() < f64::EPSILON);
        assert_eq!(metrics[1].name, "seven_day");
        assert!((metrics[1].utilization - 0.2_f64).abs() < f64::EPSILON);
        assert_eq!(metrics[2].name, "seven_day_opus");
        assert!((metrics[2].utilization - 0.3_f64).abs() < f64::EPSILON);
        assert_eq!(metrics[3].name, "extra_usage");
        assert!((metrics[3].utilization - 0.4_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn test_all_fields_present() {
        let response = UsageResponse {
            five_hour: Some(UsageWindow {
                utilization: Some(0.1),
                resets_at: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: Some(0.2),
                resets_at: None,
            }),
            seven_day_sonnet: Some(UsageWindow {
                utilization: Some(0.3),
                resets_at: None,
            }),
            seven_day_oauth_apps: Some(UsageWindow {
                utilization: Some(0.4),
                resets_at: None,
            }),
            seven_day_opus: Some(UsageWindow {
                utilization: Some(0.5),
                resets_at: None,
            }),
            seven_day_cowork: Some(UsageWindow {
                utilization: Some(0.6),
                resets_at: None,
            }),
            iguana_necktie: Some(UsageWindow {
                utilization: Some(0.7),
                resets_at: None,
            }),
            extra_usage: Some(UsageWindow {
                utilization: Some(0.8),
                resets_at: None,
            }),
        };
        let metrics = convert_claude_usage(&response);
        assert_eq!(metrics.len(), 8);
    }
}

#[cfg(test)]
mod openrouter_tests {
    use seher::openrouter::CreditsData;

    #[test]
    fn test_openrouter_remaining_calculation() {
        let data = CreditsData {
            total_credits: 100.0,
            total_usage: 25.5,
        };
        let remaining = data.total_credits - data.total_usage;
        assert!((data.total_credits - 100.0_f64).abs() < f64::EPSILON);
        assert!((data.total_usage - 25.5_f64).abs() < f64::EPSILON);
        assert!((remaining - 74.5_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn test_openrouter_zero_usage() {
        let data = CreditsData {
            total_credits: 50.0,
            total_usage: 0.0,
        };
        let remaining = data.total_credits - data.total_usage;
        assert!((remaining - 50.0_f64).abs() < f64::EPSILON);
    }
}
