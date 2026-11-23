//! OTLP metrics over HTTP (protobuf). Fire-and-forget: request handlers push an event
//! into a bounded channel and move on; a dedicated thread aggregates and sends under a
//! deadline, and an event that finds the channel full is dropped.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use margin_core::Clock;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, InstrumentationScope, KeyValue, any_value::Value,
};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint,
    ResourceMetrics, ScopeMetrics, Sum, metric::Data, number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;

const CHANNEL_CAPACITY: usize = 1024;
const FLUSH_EVERY: Duration = Duration::from_secs(15);
const SEND_DEADLINE: Duration = Duration::from_secs(2);
/// Upper bounds (seconds) of the latency histogram buckets.
const BOUNDS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 3.0, 10.0, 30.0,
];

pub const REQUESTS: &str = "margin_http_requests";
pub const REQUEST_SECONDS: &str = "margin_http_request_seconds";
pub const RENDER_SECONDS: &str = "margin_page_render_seconds";
pub const QUEUE_FULL: &str = "margin_db_queue_full";
pub const COMMENTS: &str = "margin_comments";
pub const FEED_WAIT_SECONDS: &str = "margin_feed_wait_seconds";
pub const FEED_LAG_SECONDS: &str = "margin_feed_lag_seconds";

#[derive(Debug, Clone)]
pub enum Event {
    Request {
        route: String,
        status: u16,
        seconds: f64,
    },
    Render {
        seconds: f64,
    },
    QueueFull,
    /// A comment was written: by `reader` or an `agent`, `now` or `held`.
    Comment {
        author_kind: &'static str,
        visibility: &'static str,
    },
    /// A feed call was answered after holding the request this long.
    FeedWait {
        seconds: f64,
    },
    /// A change reached a feed answer this long after it was made.
    FeedLag {
        seconds: f64,
    },
}

#[derive(Clone)]
pub struct Metrics {
    tx: SyncSender<Event>,
}

impl Metrics {
    /// Start the sending thread for `endpoint`.
    pub fn start(endpoint: String, clock: Arc<dyn Clock>) -> Metrics {
        let (tx, rx) = sync_channel::<Event>(CHANNEL_CAPACITY);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(SEND_DEADLINE))
            .http_status_as_error(false)
            .build()
            .into();
        let spawned = std::thread::Builder::new()
            .name("margin-metrics".into())
            .spawn(move || {
                let mut agg = Aggregate::default();
                let mut dirty = false;
                loop {
                    match rx.recv_timeout(FLUSH_EVERY) {
                        Ok(ev) => {
                            agg.add(&ev);
                            dirty = true;
                            // Drain what is already queued so a burst costs one wake-up.
                            while let Ok(ev) = rx.try_recv() {
                                agg.add(&ev);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if dirty {
                        let body = agg.to_request(clock.now_unix()).encode_to_vec();
                        let sent = agent
                            .post(&endpoint)
                            .header("Content-Type", "application/x-protobuf")
                            .send(&body[..]);
                        if let Err(e) = sent {
                            eprintln!("margin: metrics push failed: {e}");
                        }
                        dirty = false;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("margin: metrics thread did not start: {e}");
        }
        Metrics { tx }
    }

    /// A handle that records nowhere; for tests that do not look at metrics.
    pub fn detached() -> Metrics {
        let (tx, _rx) = sync_channel(1);
        Metrics { tx }
    }

    /// A handle plus the receiving end, for tests that assert on what was pushed.
    pub fn capture() -> (Metrics, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        (Metrics { tx }, rx)
    }

    pub fn push(&self, ev: Event) {
        match self.tx.try_send(ev) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {}
        }
    }
}

#[derive(Default)]
struct Hist {
    buckets: [u64; BOUNDS.len() + 1],
    count: u64,
    sum: f64,
}

impl Hist {
    fn add(&mut self, seconds: f64) {
        let i = BOUNDS
            .iter()
            .position(|b| seconds <= *b)
            .unwrap_or(BOUNDS.len());
        self.buckets[i] += 1;
        self.count += 1;
        self.sum += seconds;
    }
}

/// Cumulative totals since start; each flush exports them whole.
#[derive(Default)]
pub struct Aggregate {
    requests: BTreeMap<(String, u16), u64>,
    request_seconds: BTreeMap<String, Hist>,
    render_seconds: Hist,
    queue_full: u64,
    comments: BTreeMap<(&'static str, &'static str), u64>,
    feed_wait_seconds: Hist,
    feed_lag_seconds: Hist,
}

fn attr(key: &str, value: String) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value)),
        }),
    }
}

impl Aggregate {
    pub fn add(&mut self, ev: &Event) {
        match ev {
            Event::Request {
                route,
                status,
                seconds,
            } => {
                *self.requests.entry((route.clone(), *status)).or_default() += 1;
                self.request_seconds
                    .entry(route.clone())
                    .or_default()
                    .add(*seconds);
            }
            Event::Render { seconds } => self.render_seconds.add(*seconds),
            Event::QueueFull => self.queue_full += 1,
            Event::Comment {
                author_kind,
                visibility,
            } => *self.comments.entry((author_kind, visibility)).or_default() += 1,
            Event::FeedWait { seconds } => self.feed_wait_seconds.add(*seconds),
            Event::FeedLag { seconds } => self.feed_lag_seconds.add(*seconds),
        }
    }

    pub fn to_request(&self, now_unix: i64) -> ExportMetricsServiceRequest {
        let now = (now_unix.max(0) as u64) * 1_000_000_000;
        let hist_point = |h: &Hist, attributes: Vec<KeyValue>| HistogramDataPoint {
            attributes,
            time_unix_nano: now,
            count: h.count,
            sum: Some(h.sum),
            bucket_counts: h.buckets.to_vec(),
            explicit_bounds: BOUNDS.to_vec(),
            ..Default::default()
        };
        let sum_point = |v: u64, attributes: Vec<KeyValue>| NumberDataPoint {
            attributes,
            time_unix_nano: now,
            value: Some(number_data_point::Value::AsInt(v as i64)),
            ..Default::default()
        };
        let metric = |name: &str, data: Data| Metric {
            name: name.into(),
            data: Some(data),
            ..Default::default()
        };
        let cumulative = AggregationTemporality::Cumulative as i32;
        let metrics = vec![
            metric(
                REQUESTS,
                Data::Sum(Sum {
                    data_points: self
                        .requests
                        .iter()
                        .map(|((route, status), n)| {
                            sum_point(
                                *n,
                                vec![
                                    attr("route", route.clone()),
                                    attr("status", status.to_string()),
                                ],
                            )
                        })
                        .collect(),
                    aggregation_temporality: cumulative,
                    is_monotonic: true,
                }),
            ),
            metric(
                REQUEST_SECONDS,
                Data::Histogram(Histogram {
                    data_points: self
                        .request_seconds
                        .iter()
                        .map(|(route, h)| hist_point(h, vec![attr("route", route.clone())]))
                        .collect(),
                    aggregation_temporality: cumulative,
                }),
            ),
            metric(
                RENDER_SECONDS,
                Data::Histogram(Histogram {
                    data_points: vec![hist_point(&self.render_seconds, vec![])],
                    aggregation_temporality: cumulative,
                }),
            ),
            metric(
                QUEUE_FULL,
                Data::Sum(Sum {
                    data_points: vec![sum_point(self.queue_full, vec![])],
                    aggregation_temporality: cumulative,
                    is_monotonic: true,
                }),
            ),
            metric(
                COMMENTS,
                Data::Sum(Sum {
                    data_points: self
                        .comments
                        .iter()
                        .map(|((kind, visibility), n)| {
                            sum_point(
                                *n,
                                vec![
                                    attr("author_kind", kind.to_string()),
                                    attr("visibility", visibility.to_string()),
                                ],
                            )
                        })
                        .collect(),
                    aggregation_temporality: cumulative,
                    is_monotonic: true,
                }),
            ),
            metric(
                FEED_WAIT_SECONDS,
                Data::Histogram(Histogram {
                    data_points: vec![hist_point(&self.feed_wait_seconds, vec![])],
                    aggregation_temporality: cumulative,
                }),
            ),
            metric(
                FEED_LAG_SECONDS,
                Data::Histogram(Histogram {
                    data_points: vec![hist_point(&self.feed_lag_seconds, vec![])],
                    aggregation_temporality: cumulative,
                }),
            ),
        ];
        envelope(metrics)
    }
}

fn envelope(metrics: Vec<Metric>) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![attr("service.name", "margin".into())],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(InstrumentationScope {
                    name: "margin".into(),
                    ..Default::default()
                }),
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

pub const BACKUP_RUNS: &str = "margin_backup_runs";
pub const BACKUP_SECONDS: &str = "margin_backup_seconds";
pub const BACKUP_BYTES: &str = "margin_backup_bytes";
pub const BACKUP_LAST_SUCCESS: &str = "margin_backup_last_success_timestamp_seconds";

/// What one `margin backup` run reports about itself.
pub struct BackupRun {
    pub ok: bool,
    pub seconds: f64,
    /// Compressed size of the uploaded snapshot; meaningful only when `ok`.
    pub bytes: u64,
    pub finished_unix: i64,
}

/// The series for one backup run. A failed run reports no size and no last-success time, so
/// the last-success gauge keeps the time of the last run that did upload.
pub fn backup_request(run: &BackupRun) -> ExportMetricsServiceRequest {
    let now = (run.finished_unix.max(0) as u64) * 1_000_000_000;
    let point = |value: number_data_point::Value, attributes: Vec<KeyValue>| NumberDataPoint {
        attributes,
        time_unix_nano: now,
        value: Some(value),
        ..Default::default()
    };
    let metric = |name: &str, data: Data| Metric {
        name: name.into(),
        data: Some(data),
        ..Default::default()
    };
    let gauge = |name: &str, value: number_data_point::Value| {
        metric(
            name,
            Data::Gauge(Gauge {
                data_points: vec![point(value, vec![])],
            }),
        )
    };
    let result = if run.ok { "ok" } else { "error" };
    // One process is one run, so the counter is a delta of one; sum it over a window to count.
    let mut metrics = vec![
        metric(
            BACKUP_RUNS,
            Data::Sum(Sum {
                data_points: vec![point(
                    number_data_point::Value::AsInt(1),
                    vec![attr("result", result.into())],
                )],
                aggregation_temporality: AggregationTemporality::Delta as i32,
                is_monotonic: true,
            }),
        ),
        gauge(
            BACKUP_SECONDS,
            number_data_point::Value::AsDouble(run.seconds),
        ),
    ];
    if run.ok {
        metrics.push(gauge(
            BACKUP_BYTES,
            number_data_point::Value::AsInt(run.bytes as i64),
        ));
        metrics.push(gauge(
            BACKUP_LAST_SUCCESS,
            number_data_point::Value::AsInt(run.finished_unix),
        ));
    }
    envelope(metrics)
}

/// Push one request and wait for the answer, under the same deadline as the serving thread.
pub fn send_once(endpoint: &str, req: &ExportMetricsServiceRequest) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(SEND_DEADLINE))
        .http_status_as_error(false)
        .build()
        .into();
    let body = req.encode_to_vec();
    let response = agent
        .post(endpoint)
        .header("Content-Type", "application/x-protobuf")
        .send(&body[..])
        .map_err(|e| e.to_string())?;
    match response.status().as_u16() {
        200..=299 => Ok(()),
        status => Err(format!("{endpoint} answered {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    fn names(req: &ExportMetricsServiceRequest) -> Vec<String> {
        req.resource_metrics[0].scope_metrics[0]
            .metrics
            .iter()
            .map(|m| m.name.clone())
            .collect()
    }

    #[test_id(tqubqxq)]
    #[test]
    fn export_carries_the_series_the_dashboard_graphs() {
        let mut agg = Aggregate::default();
        agg.add(&Event::Request {
            route: "/c/{id}".into(),
            status: 200,
            seconds: 0.02,
        });
        agg.add(&Event::Render { seconds: 0.03 });
        agg.add(&Event::QueueFull);
        agg.add(&Event::Comment {
            author_kind: "agent",
            visibility: "held",
        });
        agg.add(&Event::FeedWait { seconds: 12.0 });
        agg.add(&Event::FeedLag { seconds: 0.4 });
        let req = agg.to_request(1_700_000_000);
        assert_eq!(
            names(&req),
            [
                REQUESTS,
                REQUEST_SECONDS,
                RENDER_SECONDS,
                QUEUE_FULL,
                COMMENTS,
                FEED_WAIT_SECONDS,
                FEED_LAG_SECONDS
            ]
        );
        let bytes = req.encode_to_vec();
        assert!(ExportMetricsServiceRequest::decode(&bytes[..]).is_ok());
    }

    #[test_id(t02ub9b)]
    #[test]
    fn requests_are_counted_per_route_and_status() {
        let mut agg = Aggregate::default();
        for status in [200, 200, 404] {
            agg.add(&Event::Request {
                route: "/c/{id}".into(),
                status,
                seconds: 0.001,
            });
        }
        let req = agg.to_request(0);
        let Some(Data::Sum(sum)) = &req.resource_metrics[0].scope_metrics[0].metrics[0].data else {
            panic!("not a sum")
        };
        let mut counts: Vec<i64> = sum
            .data_points
            .iter()
            .map(|p| match p.value {
                Some(number_data_point::Value::AsInt(n)) => n,
                _ => -1,
            })
            .collect();
        counts.sort();
        assert_eq!(counts, [1, 2]);
    }

    #[test_id(t7n95zp)]
    #[test]
    fn a_full_channel_drops_the_event_without_blocking() {
        let (tx, _rx) = sync_channel(1);
        let m = Metrics { tx };
        m.push(Event::QueueFull);
        m.push(Event::QueueFull); // full: dropped, returns at once
    }

    fn point_names(req: &ExportMetricsServiceRequest) -> Vec<String> {
        names(req)
    }

    #[test_id(tkbqe2v)]
    #[test]
    fn a_failed_backup_run_reports_no_last_success() {
        let run = |ok| BackupRun {
            ok,
            seconds: 2.0,
            bytes: 10,
            finished_unix: 1_700_000_000,
        };
        assert_eq!(
            point_names(&backup_request(&run(true))),
            [
                BACKUP_RUNS,
                BACKUP_SECONDS,
                BACKUP_BYTES,
                BACKUP_LAST_SUCCESS
            ]
        );
        assert_eq!(
            point_names(&backup_request(&run(false))),
            [BACKUP_RUNS, BACKUP_SECONDS]
        );
    }

    #[test_id(tis33u9)]
    #[test]
    fn send_once_posts_one_decodable_request_and_reports_a_refusal() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = conn.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_lowercase();
                    if let Some(head) = text.find("\r\n\r\n") {
                        let want = text
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= head + 4 + want {
                            break;
                        }
                    }
                }
                seen.push(buf);
                write!(
                    conn,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            seen
        });
        let req = backup_request(&BackupRun {
            ok: true,
            seconds: 1.0,
            bytes: 5,
            finished_unix: 1_700_000_000,
        });
        let url = format!("http://{addr}/opentelemetry/v1/metrics");
        assert!(send_once(&url, &req).is_ok());
        assert!(send_once(&url, &req).unwrap_err().contains("503"));
        let seen = server.join().unwrap();
        let text = String::from_utf8_lossy(&seen[0]).to_string();
        assert!(text.starts_with("POST /opentelemetry/v1/metrics"), "{text}");
        assert!(text.contains("margin_backup_last_success_timestamp_seconds"));
    }
}
