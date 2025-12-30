mod proto;

use chrono::{DateTime, Utc};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::SpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use proto::cookiejar::v1::{GetCookiesRequest, cookie_service_client::CookieServiceClient};
use serde::Deserialize;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Deserialize)]
struct UsageInfo {
    utilization: f32,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageInfo>,
    seven_day: Option<UsageInfo>,
    seven_day_oauth_apps: Option<UsageInfo>,
    seven_day_opus: Option<UsageInfo>,
    seven_day_sonnet: Option<UsageInfo>,
    iguana_necktie: Option<UsageInfo>,
    extra_usage: Option<UsageInfo>,
}

#[derive(Debug)]
struct UsageMetric {
    name: String,
    utilization: f32,
    minutes_to_reset: Option<i64>,
}

impl From<UsageResponse> for Vec<UsageMetric> {
    fn from(response: UsageResponse) -> Self {
        let now = Utc::now();
        let fields: [(&str, Option<UsageInfo>); 7] = [
            ("five_hour", response.five_hour),
            ("seven_day", response.seven_day),
            ("seven_day_oauth_apps", response.seven_day_oauth_apps),
            ("seven_day_opus", response.seven_day_opus),
            ("seven_day_sonnet", response.seven_day_sonnet),
            ("iguana_necktie", response.iguana_necktie),
            ("extra_usage", response.extra_usage),
        ];

        fields
            .into_iter()
            .filter_map(|(name, info)| {
                info.map(|i| {
                    let minutes_to_reset = i.resets_at.and_then(|reset_str| {
                        DateTime::parse_from_rfc3339(&reset_str)
                            .ok()
                            .map(|reset_time| {
                                let duration = reset_time.with_timezone(&Utc) - now;
                                duration.num_minutes().max(0)
                            })
                    });
                    UsageMetric {
                        name: name.to_string(),
                        utilization: i.utilization,
                        minutes_to_reset,
                    }
                })
            })
            .collect()
    }
}

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
    let response: tonic::Response<proto::cookiejar::v1::GetCookiesResponse> =
        client.get_cookies(request).await?;

    let cookies = response.into_inner().cookies;

    let org_id = std::env::var("CLAUDE_ORGANIZATION_ID")?;
    let url = format!("https://claude.ai/api/organizations/{org_id}/usage");

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let usage_response = http_client
        .get(&url)
        .header("Cookie", cookies)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .send()
        .await?
        .json::<UsageResponse>()
        .await?;
    let usage_metrics: Vec<UsageMetric> = usage_response.into();
    for metric in usage_metrics {
        println!(
            "Metric: {}, Utilization: {}, Minutes to Reset: {:?}",
            metric.name, metric.utilization, metric.minutes_to_reset
        );
    }

    Ok(())
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
                utilization: 0.5,
                resets_at: None,
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert_eq!(metrics[0].utilization, 0.5);
        assert!(metrics[0].minutes_to_reset.is_none());
    }

    #[test]
    fn test_single_field_with_future_reset_time() {
        let future_time = Utc::now() + Duration::minutes(30);
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: 0.75,
                resets_at: Some(future_time.to_rfc3339()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "five_hour");
        assert_eq!(metrics[0].utilization, 0.75);
        // Allow 1 minute margin for test execution time
        let minutes = metrics[0].minutes_to_reset.unwrap();
        assert!(minutes >= 29 && minutes <= 30);
    }

    #[test]
    fn test_past_reset_time_returns_zero() {
        let past_time = Utc::now() - Duration::minutes(10);
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: 1.0,
                resets_at: Some(past_time.to_rfc3339()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics[0].minutes_to_reset, Some(0));
    }

    #[test]
    fn test_invalid_reset_time_format_returns_none() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: 0.5,
                resets_at: Some("invalid-date-format".to_string()),
            }),
            seven_day: None,
            seven_day_oauth_apps: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            iguana_necktie: None,
            extra_usage: None,
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].minutes_to_reset.is_none());
    }

    #[test]
    fn test_multiple_fields_preserves_order() {
        let response = UsageResponse {
            five_hour: Some(UsageInfo {
                utilization: 0.1,
                resets_at: None,
            }),
            seven_day: Some(UsageInfo {
                utilization: 0.2,
                resets_at: None,
            }),
            seven_day_oauth_apps: None,
            seven_day_opus: Some(UsageInfo {
                utilization: 0.3,
                resets_at: None,
            }),
            seven_day_sonnet: None,
            iguana_necktie: None,
            extra_usage: Some(UsageInfo {
                utilization: 0.4,
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
                utilization: 0.1,
                resets_at: None,
            }),
            seven_day: Some(UsageInfo {
                utilization: 0.2,
                resets_at: None,
            }),
            seven_day_oauth_apps: Some(UsageInfo {
                utilization: 0.3,
                resets_at: None,
            }),
            seven_day_opus: Some(UsageInfo {
                utilization: 0.4,
                resets_at: None,
            }),
            seven_day_sonnet: Some(UsageInfo {
                utilization: 0.5,
                resets_at: None,
            }),
            iguana_necktie: Some(UsageInfo {
                utilization: 0.6,
                resets_at: None,
            }),
            extra_usage: Some(UsageInfo {
                utilization: 0.7,
                resets_at: None,
            }),
        };
        let metrics: Vec<UsageMetric> = response.into();
        assert_eq!(metrics.len(), 7);
    }
}
