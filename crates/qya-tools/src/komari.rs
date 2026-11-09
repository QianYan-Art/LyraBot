//! komari_report：只读汇总 Komari 公开可见节点近 24 小时流量（移植自 Hermes scripts/komari_traffic_daily.py，口径不变）。

use async_trait::async_trait;
use jiff::{Timestamp, ToSpan};
use qya_agent::{Concurrency, Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

const METRICS: [&str; 2] = ["traffic.up", "traffic.down"];
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub struct KomariReport {
    spec: ToolSpec,
    config: PathBuf,
}

impl KomariReport {
    pub fn new(config: PathBuf) -> Self {
        let spec = ToolSpec {
            name: "komari_report".into(),
            description: "生成流量日报：只读查询 Komari 探针里全部公开可见节点（各台服务器）近 24 小时（UTC）的上下行流量。\
                          阿颜说“流量日报”“流量报告”“服务器流量”“探针流量”时调用，把返回的 report 原样交回；缺数据的节点不记为零。\
                          网站访问量、访客统计是另一回事，只有阿颜明确说网站或博客时才去看 nginx 日志。".into(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        };
        Self { spec, config }
    }

    fn base_url(&self) -> Result<String, String> {
        let text = std::fs::read_to_string(&self.config).map_err(|_| "invalid_komari_config")?;
        let t: toml::Table = text.parse().map_err(|_| "invalid_komari_config")?;
        let base = t.get("base_url").and_then(|v| v.as_str()).ok_or("invalid_komari_config")?.trim_end_matches('/').to_owned();
        let u = reqwest::Url::parse(&base).map_err(|_| "invalid_komari_config")?;
        let ok = u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && matches!(u.path(), "" | "/");
        if ok { Ok(base) } else { Err("invalid_komari_config".into()) }
    }

    async fn fetch(&self) -> Result<Value, String> {
        let base = self.base_url()?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let end = Timestamp::now();
        let start = end - 24.hours();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "komari_unavailable")?;
        let listing = request(http.get(format!("{base}/api/nodes"))).await?;
        if listing.get("status").and_then(Value::as_str) != Some("success") {
            return Err("invalid_komari_response".into());
        }
        let body = json!({
            "jsonrpc": "2.0", "method": "public:queryMetrics", "id": 1,
            "params": {
                "metric_keys": METRICS,
                "start": start.to_string(), "end": end.to_string(),
                "aggregation_by_metric": {"traffic.up": "sum", "traffic.down": "sum"},
                "max_points": 1000
            }
        });
        let result = request(http.post(format!("{base}/api/rpc2")).json(&body)).await?;
        if result.get("error").is_some_and(|e| !e.is_null()) || result.get("result").is_none() {
            return Err("komari_metrics_failed".into());
        }
        build_report(listing.get("data").unwrap_or(&Value::Null), &result["result"], start, end)
    }
}

async fn request(rb: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = rb.send().await.map_err(|e| if e.is_timeout() { "komari_timeout" } else { "komari_unavailable" })?;
    if !resp.status().is_success() {
        return Err("komari_unavailable".into());
    }
    let bytes = resp.bytes().await.map_err(|_| "komari_unavailable")?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("response_too_large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid_komari_response".into())
}

fn human(mut v: f64) -> String {
    for unit in ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] {
        if v < 1024.0 || unit == "PiB" {
            return format!("{v:.2} {unit}");
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// 纯函数：由节点列表与指标序列生成日报（与 Python 版逐条一致）。
pub fn build_report(nodes: &Value, metrics: &Value, start: Timestamp, end: Timestamp) -> Result<Value, String> {
    let bad = || "invalid_komari_response".to_owned();
    let nodes = nodes.as_array().filter(|n| !n.is_empty()).ok_or("no_visible_nodes")?;
    let series = metrics.get("series").and_then(Value::as_array).ok_or_else(bad)?;
    let mut totals: BTreeMap<(String, String), f64> = BTreeMap::new();
    for s in series {
        let key = s.get("metric_key").and_then(Value::as_str).filter(|k| METRICS.contains(k)).ok_or_else(bad)?;
        let entity = s.get("entity_id").and_then(Value::as_str).ok_or_else(bad)?;
        let points = match s.get("points") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(a)) => a.as_slice(),
            _ => return Err(bad()),
        };
        if s.get("downsampled").and_then(Value::as_bool).unwrap_or(false)
            && s.get("downsample_algorithm").and_then(Value::as_str) != Some("sum")
        {
            return Err("unexpected_aggregation".into());
        }
        let mut values = Vec::new();
        for p in points {
            let p = p.as_object().ok_or_else(bad)?;
            match p.get("value") {
                None | Some(Value::Null) => continue,
                Some(Value::Number(n)) => {
                    let v = n.as_f64().filter(|v| v.is_finite() && *v >= 0.0).ok_or("invalid_traffic_value")?;
                    values.push(v);
                }
                Some(_) => return Err("invalid_traffic_value".into()),
            }
        }
        if !values.is_empty() {
            *totals.entry((entity.to_owned(), key.to_owned())).or_default() += values.iter().sum::<f64>();
        }
    }
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for n in nodes {
        let uuid = n.get("uuid").and_then(Value::as_str).ok_or_else(bad)?;
        if !seen.insert(uuid.to_owned()) {
            return Err("duplicate_node".into());
        }
        let name = n.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(uuid).replace(['\r', '\n'], " ");
        let up = totals.get(&(uuid.to_owned(), METRICS[0].to_owned())).copied();
        let down = totals.get(&(uuid.to_owned(), METRICS[1].to_owned())).copied();
        rows.push((name, up, down));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let fmt = |t: Timestamp| t.strftime("%Y-%m-%d %H:%M").to_string();
    let mut lines = vec!["流量日报（近 24 小时，Komari 公开可见节点）".to_owned(), format!("{} 至 {} UTC", fmt(start), fmt(end))];
    let mut complete = 0;
    let mut total = 0.0;
    for (name, up, down) in &rows {
        match (up, down) {
            (Some(u), Some(d)) => {
                complete += 1;
                total += u + d;
                lines.push(format!("{name}：↑{} ↓{} 合计 {}", human(*u), human(*d), human(u + d)));
            }
            _ => lines.push(format!("{name}：数据不足，不记为零")),
        }
    }
    lines.push(format!("有效数据 {complete}/{} 台，已知合计 {}", rows.len(), human(total)));
    let nodes_json: Vec<Value> = rows.iter().map(|(n, u, d)| json!({"name": n, "up_bytes": u, "down_bytes": d})).collect();
    Ok(json!({"ok": true, "nodes": nodes_json, "report": lines.join("\n")}))
}

#[async_trait]
impl Tool for KomariReport {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn call(&self, _ctx: ToolCtx, _args: Value) -> ToolOutput {
        match self.fetch().await {
            Ok(v) => ToolOutput::ok(v["report"].as_str().unwrap_or_default().to_owned()),
            Err(code) => ToolOutput::err(format!("流量日报失败：{code}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_matches_python_rules() {
        let nodes = json!([{"uuid": "b", "name": "Beta"}, {"uuid": "a", "name": "Alpha"}, {"uuid": "c", "name": ""}]);
        let metrics = json!({"series": [
            {"metric_key": "traffic.up", "entity_id": "a", "points": [{"value": 1024}, {"value": null}, {"value": 1024}]},
            {"metric_key": "traffic.down", "entity_id": "a", "points": [{"value": 1048576}]},
            {"metric_key": "traffic.up", "entity_id": "b", "points": [{"value": 10}]},
            {"metric_key": "traffic.down", "entity_id": "c", "points": []}
        ]});
        let (s, e): (Timestamp, Timestamp) = ("2026-11-06T00:00:00Z".parse().unwrap(), "2026-11-07T00:00:00Z".parse().unwrap());
        let r = build_report(&nodes, &metrics, s, e).unwrap();
        let text = r["report"].as_str().unwrap();
        assert!(text.contains("2026-11-06 00:00 至 2026-11-07 00:00 UTC"));
        assert!(text.contains("Alpha：↑2.00 KiB ↓1.00 MiB 合计 1.00 MiB"), "{text}");
        assert!(text.contains("Beta：数据不足，不记为零"));
        assert!(text.contains("c：数据不足"), "空名字回退到 uuid");
        assert!(text.ends_with("有效数据 1/3 台，已知合计 1.00 MiB"));
        assert_eq!(build_report(&json!([]), &metrics, s, e).unwrap_err(), "no_visible_nodes");
        let dup = json!([{"uuid": "a"}, {"uuid": "a"}]);
        assert_eq!(build_report(&dup, &metrics, s, e).unwrap_err(), "duplicate_node");
        let neg = json!({"series": [{"metric_key": "traffic.up", "entity_id": "a", "points": [{"value": -1}]}]});
        assert_eq!(build_report(&nodes, &neg, s, e).unwrap_err(), "invalid_traffic_value");
    }
}
