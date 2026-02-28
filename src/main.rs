mod proto;

use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use proto::cookiejar::v1::{GetCookiesRequest, cookie_service_client::CookieServiceClient};
use serde::Deserialize;
use tracing::{error, info, instrument};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

// ============================================================================
// Claude Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct UsageInfo {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageInfo>,
    seven_day: Option<UsageInfo>,
    seven_day_oauth_apps: Option<UsageInfo>,
    seven_day_opus: Option<UsageInfo>,
    seven_day_sonnet: Option<UsageInfo>,
    seven_day_cowork: Option<UsageInfo>,
    iguana_necktie: Option<UsageInfo>,
    extra_usage: Option<UsageInfo>,
}

#[derive(Debug)]
struct UsageMetric {
    name: String,
    utilization: f64,
    seconds_to_reset: Option<i64>,
}

impl From<UsageResponse> for Vec<UsageMetric> {
    fn from(response: UsageResponse) -> Self {
        let now = Utc::now();
        let fields: [(&str, Option<UsageInfo>); 8] = [
            ("five_hour", response.five_hour),
            ("seven_day", response.seven_day),
            ("seven_day_oauth_apps", response.seven_day_oauth_apps),
            ("seven_day_opus", response.seven_day_opus),
            ("seven_day_sonnet", response.seven_day_sonnet),
            ("seven_day_cowork", response.seven_day_cowork),
            ("iguana_necktie", response.iguana_necktie),
            ("extra_usage", response.extra_usage),
        ];

        fields
            .into_iter()
            .filter_map(|(name, info)| {
                info.and_then(|i| {
                    i.utilization.map(|utilization| {
                        let seconds_to_reset = i.resets_at.and_then(|reset_str| {
                            DateTime::parse_from_rfc3339(&reset_str)
                                .ok()
                                .map(|reset_time| {
                                    let duration = reset_time.with_timezone(&Utc) - now;
                                    duration.num_seconds().max(0)
                                })
                        });
                        UsageMetric {
                            name: name.to_string(),
                            utilization,
                            seconds_to_reset,
                        }
                    })
                })
            })
            .collect()
    }
}

// ============================================================================
// OpenRouter Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct OpenRouterCreditsData {
    total_credits: f64,
    total_usage: f64,
}

#[derive(Debug, Deserialize)]
struct OpenRouterCreditsResponse {
    data: OpenRouterCreditsData,
}

#[derive(Debug)]
struct OpenRouterMetrics {
    total_credits: f64,
    total_usage: f64,
    remaining: f64,
}

impl From<OpenRouterCreditsResponse> for OpenRouterMetrics {
    fn from(response: OpenRouterCreditsResponse) -> Self {
        Self {
            total_credits: response.data.total_credits,
            total_usage: response.data.total_usage,
            remaining: response.data.total_credits - response.data.total_usage,
        }
    }
}

// ============================================================================
// GitHub Copilot Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct GithubCopilotQuotaRemaining {
    #[serde(rename = "chatPercentage")]
    chat_percentage: f64,
    #[serde(rename = "premiumInteractionsPercentage")]
    premium_interactions_percentage: f64,
}

#[derive(Debug, Deserialize)]
struct GithubCopilotQuotas {
    remaining: GithubCopilotQuotaRemaining,
    #[serde(rename = "resetDate")]
    reset_date: String,
}

#[derive(Debug, Deserialize)]
struct GithubCopilotResponse {
    quotas: GithubCopilotQuotas,
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
// Claude Metrics Collection
// ============================================================================

#[instrument(name = "claude_usage_metrics_run", skip_all, err)]
async fn run_claude() -> anyhow::Result<()> {
    info!("Fetching Claude usage metrics");

    let endpoint =
        std::env::var("COOKIEJAR_URL").context("COOKIEJAR_URL environment variable not set")?;
    let channel = tonic::transport::Channel::from_shared(endpoint.into_bytes())
        .context("Invalid COOKIEJAR_URL")?
        .connect_timeout(std::time::Duration::from_secs(10))
        .connect()
        .await
        .context("Failed to connect to cookie service")?;
    let mut client = CookieServiceClient::new(channel);

    let request = GetCookiesRequest {
        host: ".claude.ai".to_string(),
    };
    let response: tonic::Response<proto::cookiejar::v1::GetCookiesResponse> = client
        .get_cookies(request)
        .await
        .context("Failed to get cookies")?;

    let cookies = response.into_inner().cookies;

    let org_id = std::env::var("CLAUDE_ORGANIZATION_ID")
        .context("CLAUDE_ORGANIZATION_ID environment variable not set")?;
    let url = format!("https://claude.ai/api/organizations/{org_id}/usage");

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;
    let body = http_client
        .get(&url)
        .header("Cookie", cookies)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .send()
        .await
        .context("Failed to send request to Claude API")?
        .error_for_status()
        .context("Claude API returned non-2xx status")?
        .text()
        .await
        .context("Failed to read response body")?;
    let usage_response = serde_json::from_str::<UsageResponse>(&body)
        .with_context(|| format!("Failed to parse usage response: {}", body))?;
    let usage_metrics: Vec<UsageMetric> = usage_response.into();

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

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let response = http_client
        .get("https://openrouter.ai/api/v1/credits")
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("Failed to send request to OpenRouter API")?
        .error_for_status()
        .context("OpenRouter API returned non-2xx status")?
        .json::<OpenRouterCreditsResponse>()
        .await
        .context("Failed to parse OpenRouter credits response")?;

    let metrics: OpenRouterMetrics = response.into();

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

    total_gauge.record(metrics.total_credits, &[]);
    usage_gauge.record(metrics.total_usage, &[]);
    remaining_gauge.record(metrics.remaining, &[]);

    info!(
        total_credits = %metrics.total_credits,
        total_usage = %metrics.total_usage,
        remaining = %metrics.remaining,
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

    let endpoint =
        std::env::var("COOKIEJAR_URL").context("COOKIEJAR_URL environment variable not set")?;
    let channel = tonic::transport::Channel::from_shared(endpoint.into_bytes())
        .context("Invalid COOKIEJAR_URL")?
        .connect_timeout(std::time::Duration::from_secs(10))
        .connect()
        .await
        .context("Failed to connect to cookie service")?;
    let mut client = CookieServiceClient::new(channel);

    let request = GetCookiesRequest {
        host: "github.com".to_string(),
    };
    let response: tonic::Response<proto::cookiejar::v1::GetCookiesResponse> = client
        .get_cookies(request)
        .await
        .context("Failed to get cookies")?;

    let cookies = response.into_inner().cookies;

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let quota_response = http_client
        .get("https://github.com/github-copilot/chat")
        .header("Cookie", cookies)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("github-verified-fetch", "true")
        .header("x-requested-with", "XMLHttpRequest")
        .header("accept", "application/json")
        .send()
        .await
        .context("Failed to send request to GitHub Copilot API")?
        .error_for_status()
        .context("GitHub Copilot API returned non-2xx status")?
        .json::<GithubCopilotResponse>()
        .await
        .context("Failed to parse GitHub Copilot quota response")?;

    let quotas = quota_response.quotas;

    let now = Utc::now();
    let seconds_to_reset = NaiveDate::parse_from_str(&quotas.reset_date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| {
            let reset_utc = dt.and_utc();
            (reset_utc - now).num_seconds().max(0)
        });

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

    let chat_utilization = 1.0 - quotas.remaining.chat_percentage / 100.0;
    let premium_utilization = 1.0 - quotas.remaining.premium_interactions_percentage / 100.0;

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
        errors.push(format!("Claude: {}", e));
    }
    if let Err(ref e) = openrouter_result {
        error!(error = %e, "OpenRouter metrics collection failed");
        errors.push(format!("OpenRouter: {}", e));
    }
    if let Err(ref e) = github_copilot_result {
        error!(error = %e, "GitHub Copilot metrics collection failed");
        errors.push(format!("GitHub Copilot: {}", e));
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
            eprintln!("Failed to initialize telemetry: {:#}", e);
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
        eprintln!("Error shutting down tracer provider: {:?}", e);
    }
    if let Err(e) = providers.meter_provider.shutdown() {
        eprintln!("Error shutting down meter provider: {:?}", e);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn test_empty_response_returns_empty_vec() {
        let response = UsageResponse {
            five_hour: None,
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert!(metrics.is_empty());
    }

    #[test]
    fn test_single_field_with_no_reset_time() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.5),
                resets_at: None,
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert_eq!(metrics[0].utilization, 0.5);
        assert!(metrics[0].seconds_to_reset.is_none());
    }

    #[test]
    fn test_single_field_with_future_reset_time() {
        let future_time = Utc::now() + Duration::seconds(1800);
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.75),
                resets_at: Some(future_time.to_rfc3339()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert_eq!(metrics[0].utilization, 0.75);
        // Allow 1 second margin for test execution time
        let seconds = metrics[0].seconds_to_reset.unwrap();
        assert!((1799..=1800).contains(&seconds));
    }

    #[test]
    fn test_past_reset_time_returns_zero() {
        let past_time = Utc::now() - Duration::minutes(10);
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(1.0),
                resets_at: Some(past_time.to_rfc3339()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics[0].seconds_to_reset, Some(0));
    }

    #[test]
    fn test_invalid_reset_time_format_returns_none() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.5),
                resets_at: Some("invalid-date-format".to_string()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].seconds_to_reset.is_none());
    }

    #[test]
    fn test_null_utilization_is_skipped() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.5),
                resets_at: None,
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: Some(UsageInfo {
                utilization: None,
                resets_at: None,
            }),
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
    }

    #[test]
    fn test_multiple_fields_preserves_order() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.1),
                resets_at: None,
            }),
            seven_day: Some(UsageInfo {
                utilization: Some(0.2),
                resets_at: None,
            }),
            seven_day_oauth_apps: None,
            seven_day_opus: Some(UsageInfo {
                utilization: Some(0.3),
                resets_at: None,
            }),
            seven_day_sonnet: None,
            seven_day_cowork: None,
            iguana_necktie: None,
            extra_usage: Some(UsageInfo {
                utilization: Some(0.4),
                resets_at: None,
            }),
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 4);
        assert_eq!(metrics[0].name, "five_hour");
        assert_eq!(metrics[0].utilization, 0.1);
        assert_eq!(metrics[1].name, "seven_day");
        assert_eq!(metrics[1].utilization, 0.2);
        assert_eq!(metrics[2].name, "seven_day_opus");
        assert_eq!(metrics[2].utilization, 0.3);
        assert_eq!(metrics[3].name, "extra_usage");
        assert_eq!(metrics[3].utilization, 0.4);
    }

    #[test]
    fn test_all_fields_present() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: Some(0.1),
                resets_at: None,
            }),
            seven_day: Some(UsageInfo {
                utilization: Some(0.2),
                resets_at: None,
            }),
            seven_day_oauth_apps: Some(UsageInfo {
                utilization: Some(0.3),
                resets_at: None,
            }),
            seven_day_opus: Some(UsageInfo {
                utilization: Some(0.4),
                resets_at: None,
            }),
            seven_day_sonnet: Some(UsageInfo {
                utilization: Some(0.5),
                resets_at: None,
            }),
            seven_day_cowork: Some(UsageInfo {
                utilization: Some(0.6),
                resets_at: None,
            }),
            iguana_necktie: Some(UsageInfo {
                utilization: Some(0.7),
                resets_at: None,
            }),
            extra_usage: Some(UsageInfo {
                utilization: Some(0.8),
                resets_at: None,
            }),
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 8);
    }
}

#[cfg(test)]
mod openrouter_tests {
    use super::*;

    #[test]
    fn test_openrouter_metrics_conversion() {
        let response = OpenRouterCreditsResponse {
            data: OpenRouterCreditsData {
                total_credits: 100.0,
                total_usage: 25.5,
            },
        };
        let metrics: OpenRouterMetrics = response.into();
        assert_eq!(metrics.total_credits, 100.0);
        assert_eq!(metrics.total_usage, 25.5);
        assert_eq!(metrics.remaining, 74.5);
    }

    #[test]
    fn test_openrouter_metrics_zero_usage() {
        let response = OpenRouterCreditsResponse {
            data: OpenRouterCreditsData {
                total_credits: 50.0,
                total_usage: 0.0,
            },
        };
        let metrics: OpenRouterMetrics = response.into();
        assert_eq!(metrics.remaining, 50.0);
    }
}
