//! Prometheus metrics: counters and histograms the service updates as it works, and gauges read when scraped,
//! in the text exposition format (served on a listener of its own, `--metrics-listen`).

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

/// Upper bounds of the duration histograms, in seconds.
const BUCKETS: [f64; 12] = [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 5.0, 30.0, 300.0];

type Labels = Vec<(&'static str, String)>;

/// A gauge family: name, help, and a value per label set.
pub type Gauge<'a> = (&'a str, &'a str, Vec<(Labels, f64)>);

#[derive(Default)]
struct Histogram { counts: [u64; BUCKETS.len()], sum: f64, count: u64 }

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<(&'static str, Labels), u64>>,
    histograms: Mutex<BTreeMap<(&'static str, Labels), Histogram>>,
}

/// What a family is, for its `# HELP` line.
fn help(name: &str) -> &'static str {
    match name {
        "selfdb_actions_total" => "Actions run, by tool and outcome.",
        "selfdb_action_seconds" => "Time an action took, by tool.",
        "selfdb_session_start_seconds" => "Time a session took to start.",
        "selfdb_sessions_started_total" => "Sessions started, by kind.",
        "selfdb_sessions_ended_total" => "Sessions ended, by reason.",
        "selfdb_forks_total" => "Fork requests.",
        "selfdb_checkpoints_total" => "Checkpoints made.",
        "selfdb_judgements_total" => "Judge runs, by outcome.",
        "selfdb_template_fetches_total" => "Templates fetched from other nodes, by outcome.",
        "selfdb_forwarded_total" => "Requests the coordinator passed to nodes, by outcome.",
        "selfdb_placement_refusals_total" => "Session requests the coordinator refused, by reason.",
        _ => "",
    }
}

fn label_text(labels: &[(&'static str, String)]) -> String {
    if labels.is_empty() { return String::new(); }
    let parts: Vec<String> = labels.iter().map(|(k, v)| format!("{}=\"{}\"", k, v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))).collect();
    format!("{{{}}}", parts.join(","))
}

impl Metrics {
    pub fn inc(&self, name: &'static str, labels: &[(&'static str, &str)]) {
        let key = (name, labels.iter().map(|(k, v)| (*k, v.to_string())).collect());
        *self.counters.lock().unwrap().entry(key).or_insert(0) += 1;
    }

    pub fn observe(&self, name: &'static str, labels: &[(&'static str, &str)], seconds: f64) {
        let key = (name, labels.iter().map(|(k, v)| (*k, v.to_string())).collect());
        let mut hs = self.histograms.lock().unwrap();
        let h = hs.entry(key).or_default();
        for (i, b) in BUCKETS.iter().enumerate() { if seconds <= *b { h.counts[i] += 1; } }
        h.sum += seconds;
        h.count += 1;
    }

    /// The counters and histograms, then `gauges` (name, help, labelled values).
    pub fn render(&self, gauges: &[Gauge]) -> String {
        let mut out = String::new();
        let mut last = "";
        for ((name, labels), v) in self.counters.lock().unwrap().iter() {
            if *name != last { let _ = writeln!(out, "# HELP {} {}\n# TYPE {} counter", name, help(name), name); last = name; }
            let _ = writeln!(out, "{}{} {}", name, label_text(labels), v);
        }
        last = "";
        for ((name, labels), h) in self.histograms.lock().unwrap().iter() {
            if *name != last { let _ = writeln!(out, "# HELP {} {}\n# TYPE {} histogram", name, help(name), name); last = name; }
            for (i, b) in BUCKETS.iter().enumerate() {
                let mut l = labels.clone();
                l.push(("le", format!("{}", b)));
                let _ = writeln!(out, "{}_bucket{} {}", name, label_text(&l), h.counts[i]);
            }
            let mut l = labels.clone();
            l.push(("le", "+Inf".into()));
            let _ = writeln!(out, "{}_bucket{} {}", name, label_text(&l), h.count);
            let _ = writeln!(out, "{}_sum{} {}\n{}_count{} {}", name, label_text(labels), h.sum, name, label_text(labels), h.count);
        }
        for (name, help, values) in gauges {
            let _ = writeln!(out, "# HELP {} {}\n# TYPE {} gauge", name, help, name);
            for (labels, v) in values { let _ = writeln!(out, "{}{} {}", name, label_text(labels), v); }
        }
        out
    }
}

/// Serve `render` at GET /metrics on `listen` (no authentication: bind it to an internal address).
pub async fn serve(listen: String, render: std::sync::Arc<dyn Fn() -> String + Send + Sync>) -> anyhow::Result<()> {
    use axum::routing::get;
    let app = axum::Router::new().route("/metrics", get(move || {
        let r = render.clone();
        async move {
            let text = tokio::task::spawn_blocking(move || r()).await.unwrap_or_default();
            ([("content-type", "text/plain; version=0.0.4")], text)
        }
    }));
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
