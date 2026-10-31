//! Opt-in ONNX Runtime node-placement diagnostics (`YAPSTACK_ORT_PLACEMENT=1`).
//!
//! The accelerator label says which execution provider *registered*; it
//! cannot say where ORT actually *placed* the graph's nodes. ORT only reports
//! placement at verbose severity, from `VerifyEachNodeIsAssignedToAnEp`, once
//! per session at load. Turning on verbose logging globally (`ORT_LOG=verbose`
//! plus `RUST_LOG=ort=trace`) floods the log with thousands of per-inference
//! lines and aborts the process at exit (ort's tracing bridge logs during the
//! session drop, after thread-local teardown). So this recorder attaches a
//! *session-scoped* logger instead: it raises only the instrumented
//! sessions to verbose and keeps only the placement lines, without going
//! through `tracing` at all.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// ORT code location of the placement report; every placement line carries it.
const PLACEMENT_LOCATION: &str = "VerifyEachNodeIsAssignedToAnEp";

/// Node placement of one ORT session, as ORT reported it at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPlacement {
    /// Node count per execution provider (e.g. `WebGpuExecutionProvider`).
    pub nodes_by_ep: BTreeMap<String, usize>,
    /// MatMul/Conv-family node count per execution provider — the ops that
    /// carry the compute. Empty when ORT put every node on one EP, because
    /// ORT then reports only the total, not the per-node list.
    pub heavy_by_ep: BTreeMap<String, usize>,
}

impl SessionPlacement {
    /// Total MatMul/Conv-family nodes, or `None` when ORT reported a single
    /// EP without the per-node list (every node, heavy or not, is on it).
    fn heavy_total(&self) -> Option<usize> {
        if self.heavy_by_ep.is_empty() && self.nodes_by_ep.len() == 1 {
            None
        } else {
            Some(self.heavy_by_ep.values().sum())
        }
    }

    /// `on/total` heavy nodes on `ep`, or `all/all` when ORT put every node
    /// on `ep` without listing them.
    pub fn heavy_on_label(&self, ep: &str) -> String {
        match self.heavy_total() {
            Some(total) => {
                let on = self.heavy_by_ep.get(ep).copied().unwrap_or(0);
                format!("{on}/{total}")
            }
            None if self.nodes_by_ep.contains_key(ep) => "all/all".to_string(),
            None => "0/all".to_string(),
        }
    }

    /// `(heavy nodes on ep, heavy nodes total)`; a single-EP report counts
    /// every node as on that EP.
    #[cfg(test)]
    fn heavy_on(&self, ep: &str) -> (usize, usize) {
        match self.heavy_total() {
            Some(total) => (self.heavy_by_ep.get(ep).copied().unwrap_or(0), total),
            None => {
                let n = self.nodes_by_ep.get(ep).copied().unwrap_or(0);
                (n, n)
            }
        }
    }

    /// Fraction of MatMul/Conv-family nodes placed on `ep` (0.0 when the
    /// session has none).
    #[cfg(test)]
    pub fn heavy_share(&self, ep: &str) -> f64 {
        match self.heavy_on(ep) {
            (_, 0) => 0.0,
            (on, total) => on as f64 / total as f64,
        }
    }
}

/// The ops whose placement decides whether a GPU EP is doing the work.
fn is_heavy_op(op: &str) -> bool {
    op.contains("MatMul") || op.contains("Conv") || op == "Gemm"
}

/// Collects ORT placement lines from every session it is attached to, one
/// [`SessionPlacement`] per session in load order.
#[derive(Debug, Clone, Default)]
pub struct PlacementRecorder {
    inner: Arc<Mutex<Recorded>>,
}

#[derive(Debug, Default)]
struct Recorded {
    sessions: Vec<SessionPlacement>,
    /// EP of the per-node list currently being read.
    current_ep: Option<String>,
}

impl PlacementRecorder {
    /// A recorder when `YAPSTACK_ORT_PLACEMENT` is set to a truthy value.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("YAPSTACK_ORT_PLACEMENT").unwrap_or_default();
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "off" => None,
            _ => Some(Self::default()),
        }
    }

    /// Feed one ORT log message. Anything other than a placement line is
    /// ignored, so the recorder can see the session's whole verbose stream.
    pub fn record(&self, code_location: &str, message: &str) {
        if !code_location.contains(PLACEMENT_LOCATION) {
            return;
        }
        let mut rec = self.inner.lock().expect("placement recorder poisoned");
        let line = message.trim();
        if line == "Node placements" {
            rec.sessions.push(SessionPlacement::default());
            rec.current_ep = None;
            return;
        }
        if let Some((ep, count)) = parse_placed_on(line) {
            if rec.sessions.is_empty() {
                rec.sessions.push(SessionPlacement::default());
            }
            let session = rec.sessions.last_mut().expect("pushed above");
            session.nodes_by_ep.insert(ep.to_string(), count);
            rec.current_ep = Some(ep.to_string());
            return;
        }
        // Per-node line: `<OpType> (<node name>)`.
        let op = line.split_whitespace().next().unwrap_or_default();
        if !is_heavy_op(op) {
            return;
        }
        let Some(ep) = rec.current_ep.clone() else {
            return;
        };
        if let Some(session) = rec.sessions.last_mut() {
            *session.heavy_by_ep.entry(ep).or_insert(0) += 1;
        }
    }

    /// Drain what has been recorded so far (one entry per session).
    pub fn take(&self) -> Vec<SessionPlacement> {
        let mut rec = self.inner.lock().expect("placement recorder poisoned");
        rec.current_ep = None;
        std::mem::take(&mut rec.sessions)
    }

    /// Raise this session to verbose and route its log stream through the
    /// recorder. Applied via parakeet-rs's `with_custom_configure` hook.
    pub fn attach(
        &self,
        builder: ort::session::builder::SessionBuilder,
    ) -> ort::Result<ort::session::builder::SessionBuilder> {
        let recorder = self.clone();
        Ok(builder
            .with_log_level(ort::logging::LogLevel::Verbose)?
            .with_logger(Arc::new(
                move |_level, _category, _id, location, message| recorder.record(location, message),
            ))?)
    }
}

/// Parses ` Node(s) placed on [EP]. Number of nodes: N` and
/// ` All nodes placed on [EP]. Number of nodes: N`.
fn parse_placed_on(line: &str) -> Option<(&str, usize)> {
    let rest = line
        .strip_prefix("Node(s) placed on [")
        .or_else(|| line.strip_prefix("All nodes placed on ["))?;
    let (ep, rest) = rest.split_once(']')?;
    let count = rest.rsplit(':').next()?.trim().parse().ok()?;
    Some((ep, count))
}

/// ORT's provider name for an accelerator label.
pub fn ort_ep_name(accel_label: &str) -> &'static str {
    match accel_label {
        "webgpu" => "WebGpuExecutionProvider",
        "coreml" => "CoreMLExecutionProvider",
        _ => "CPUExecutionProvider",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOC: &str = "session_state.cc:1315 VerifyEachNodeIsAssignedToAnEp";

    fn feed(rec: &PlacementRecorder, lines: &[&str]) {
        for l in lines {
            rec.record(LOC, l);
        }
    }

    #[test]
    fn splits_sessions_and_counts_heavy_ops_per_ep() {
        let rec = PlacementRecorder::default();
        // Shape of ORT 1.24's report for the shipped int8 encoder.
        feed(
            &rec,
            &[
                "Node placements",
                " Node(s) placed on [WebGpuExecutionProvider]. Number of nodes: 3",
                "  MatMul (/layers.0/MatMul)",
                "  Unsqueeze (/pre_encode/Unsqueeze)",
                "  Add (/pre_encode/Add)",
                " Node(s) placed on [CPUExecutionProvider]. Number of nodes: 4",
                "  MatMulInteger (/layers.0/MatMul_quant)",
                "  ConvInteger (/pre_encode/conv/Conv_quant)",
                "  DynamicQuantizeLinear (/x_QuantizeLinear)",
                "  Cast (/pre_encode/Cast)",
                "Node placements",
                " All nodes placed on [CPUExecutionProvider]. Number of nodes: 12",
            ],
        );
        let sessions = rec.take();
        assert_eq!(sessions.len(), 2);

        let enc = &sessions[0];
        assert_eq!(enc.nodes_by_ep["WebGpuExecutionProvider"], 3);
        assert_eq!(enc.nodes_by_ep["CPUExecutionProvider"], 4);
        assert_eq!(enc.heavy_on("WebGpuExecutionProvider"), (1, 3));
        assert_eq!(enc.heavy_on_label("WebGpuExecutionProvider"), "1/3");
        assert!((enc.heavy_share("WebGpuExecutionProvider") - 1.0 / 3.0).abs() < 1e-9);

        // Single-EP report has no per-node list: everything is on that EP.
        let dec = &sessions[1];
        assert_eq!(dec.heavy_on("CPUExecutionProvider"), (12, 12));
        assert_eq!(dec.heavy_share("CPUExecutionProvider"), 1.0);
        assert_eq!(dec.heavy_share("WebGpuExecutionProvider"), 0.0);
        assert_eq!(dec.heavy_on_label("CPUExecutionProvider"), "all/all");
        assert_eq!(dec.heavy_on_label("WebGpuExecutionProvider"), "0/all");

        assert!(rec.take().is_empty(), "take drains");
    }

    #[test]
    fn ignores_lines_from_other_code_locations() {
        let rec = PlacementRecorder::default();
        rec.record("webgpu_context.cc:812 Run", "Node placements");
        rec.record("graph.cc:1 Resolve", "  MatMul (x)");
        assert!(rec.take().is_empty());
    }

    #[test]
    fn maps_accel_labels_to_ort_provider_names() {
        assert_eq!(ort_ep_name("webgpu"), "WebGpuExecutionProvider");
        assert_eq!(ort_ep_name("coreml"), "CoreMLExecutionProvider");
        assert_eq!(ort_ep_name("cpu"), "CPUExecutionProvider");
    }
}
