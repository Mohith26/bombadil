use anyhow::{Context, Result};
use cdp_protocol::cdp::browser_protocol::target::{
    self, FilterEntry, SessionId, TargetFilter,
};
use cdp_protocol::cdp::js_protocol::debugger::CallFrameId;
use cdp_protocol::cdp::js_protocol::runtime;
use serde_json as json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::browser::evaluation::evaluate_script_in_debugger;
use crate::instrumentation::js::{
    EDGE_MAP_SIZE, EDGES_CURRENT, EDGES_PREVIOUS, NAMESPACE,
};

/// How long a state capture waits for workers to hand over their coverage.
/// A worker busy with a long task answers late, and its hits are then picked
/// up by a later capture instead.
const COLLECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Dedicated workers attached to the page, and for each one the last
/// coverage transfer it has confirmed.
#[derive(Clone, Default)]
pub struct Workers {
    inner: Arc<Mutex<WorkersInner>>,
}

#[derive(Default)]
struct WorkersInner {
    acknowledged: HashMap<SessionId, u64>,
    sequence: u64,
}

impl Workers {
    /// Auto-attach to the page's dedicated workers and keep track of them.
    pub fn track(
        connection: &cdp::Connection,
        session_id: &SessionId,
    ) -> Result<Self> {
        let workers = Workers::default();

        let attached = connection
            .events
            .subscribe::<target::EventAttachedToTarget>();
        let detached = connection
            .events
            .subscribe::<target::EventDetachedFromTarget>();

        {
            let workers = workers.clone();
            thread::spawn(move || {
                while let Ok(Some(event)) = attached.next() {
                    if event.target_info.r#type == "worker" {
                        log::debug!(
                            "attached to worker: {}",
                            event.target_info.url
                        );
                        workers.lock().acknowledged.insert(event.session_id, 0);
                    }
                }
            });
        }
        {
            let workers = workers.clone();
            thread::spawn(move || {
                while let Ok(Some(event)) = detached.next() {
                    workers.lock().acknowledged.remove(&event.session_id);
                }
            });
        }

        connection
            .send(
                target::SetAutoAttachParams::builder()
                    .auto_attach(true)
                    .wait_for_debugger_on_start(false)
                    .flatten(true)
                    .filter(TargetFilter::new(vec![
                        FilterEntry::builder()
                            .exclude(false)
                            .r#type("worker")
                            .build(),
                        FilterEntry::builder().exclude(true).build(),
                    ]))
                    .build()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "failed building SetAutoAttachParams: {error}"
                        )
                    })?,
                Some(session_id),
            )
            .context("failed enabling auto-attach to workers")?;

        Ok(workers)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkersInner> {
        self.inner
            .lock()
            .expect("failed to acquire lock for workers")
    }

    /// Move the coverage hits recorded in workers since the last capture into
    /// the page's coverage map, so that the next state capture sees them.
    /// `call_frame_id` is the page's paused capture frame.
    ///
    /// A worker keeps the hits it hands over until a later transfer confirms
    /// they arrived, so a response that times out loses nothing.
    pub fn transfer_coverage(
        &self,
        connection: &cdp::Connection,
        session_id: &SessionId,
        call_frame_id: &CallFrameId,
    ) {
        let (hits, sequence, received) = self.collect_coverage(connection);
        if !hits.is_empty() {
            let merged = json::to_string(&hits)
                .map_err(anyhow::Error::from)
                .and_then(|hits| {
                    evaluate_script_in_debugger(
                        connection,
                        session_id,
                        call_frame_id,
                        merge_script(&hits),
                    )
                });
            if let Err(error) = merged {
                log::warn!("failed merging worker coverage: {error:#}");
                return;
            }
        }
        let mut inner = self.lock();
        for session_id in received {
            if let Some(acknowledged) = inner.acknowledged.get_mut(&session_id)
            {
                *acknowledged = sequence;
            }
        }
    }

    /// Returns the hits from all workers that answered in time, as a flat
    /// list of edge index and hit count pairs, along with the sequence number
    /// of this collection and the workers that answered.
    fn collect_coverage(
        &self,
        connection: &cdp::Connection,
    ) -> (Vec<u32>, u64, Vec<SessionId>) {
        let (sequence, requests) = {
            let mut inner = self.lock();
            if inner.acknowledged.is_empty() {
                return (vec![], 0, vec![]);
            }
            inner.sequence += 1;
            let sequence = inner.sequence;
            let requests: Vec<_> = inner
                .acknowledged
                .iter()
                .filter_map(|(session_id, acknowledged)| {
                    let params = runtime::EvaluateParams::builder()
                        .expression(collect_expression(*acknowledged, sequence))
                        .return_by_value(true)
                        .build()
                        .ok()?;
                    connection
                        .request(params, Some(session_id))
                        .ok()
                        .map(|pending| (session_id.clone(), pending))
                })
                .collect();
            (sequence, requests)
        };

        let deadline = Instant::now() + COLLECT_TIMEOUT;
        let mut hits = vec![];
        let mut received = vec![];
        for (session_id, pending) in requests {
            let result = pending.wait_until(deadline).and_then(|returns| {
                if let Some(exception) = returns.exception_details {
                    anyhow::bail!("{}", exception.text);
                }
                Ok(json::from_value::<Vec<u32>>(
                    returns.result.value.unwrap_or_default(),
                )?)
            });
            match result {
                Ok(worker_hits) => {
                    hits.extend(worker_hits);
                    received.push(session_id);
                }
                Err(error) => {
                    log::debug!(
                        "worker coverage not collected this time ({session_id:?}): {error:#}"
                    );
                }
            }
        }
        (hits, sequence, received)
    }
}

/// Adds worker hits to the page's coverage map, creating the map if no page
/// script has been instrumented.
fn merge_script(hits: &str) -> String {
    format!(
        "const coverage = (window.{NAMESPACE} ??= {{
            {EDGES_PREVIOUS}: new Uint8Array({EDGE_MAP_SIZE}),
            {EDGES_CURRENT}: new Uint8Array({EDGE_MAP_SIZE}),
            previous: 0,
        }});
        const hits = {hits};
        for (let i = 0; i < hits.length; i += 2) {{
            const edge = hits[i];
            coverage.{EDGES_CURRENT}[edge] = Math.min(
                255,
                coverage.{EDGES_CURRENT}[edge] + hits[i + 1],
            );
        }}"
    )
}

/// Moves the worker's current hits into a batch numbered `sequence`, drops
/// the batches up to `acknowledged`, and returns all batches still kept.
fn collect_expression(acknowledged: u64, sequence: u64) -> String {
    format!(
        "((acknowledged, sequence) => {{
            const coverage = globalThis.{NAMESPACE};
            if (!coverage) return [];
            const batches = (coverage.batches ??= new Map());
            for (const key of batches.keys()) {{
                if (key <= acknowledged) batches.delete(key);
            }}
            const edges = coverage.{EDGES_CURRENT};
            const hits = [];
            for (let edge = 0; edge < edges.length; edge++) {{
                if (edges[edge] === 0) continue;
                hits.push(edge, edges[edge]);
                edges[edge] = 0;
            }}
            if (hits.length > 0) batches.set(sequence, hits);
            return Array.from(batches.values()).flat();
        }})({acknowledged}, {sequence})"
    )
}

#[cfg(test)]
mod tests {
    use super::{collect_expression, merge_script};
    use crate::instrumentation::js::{EDGE_MAP_SIZE, EDGES_CURRENT, NAMESPACE};
    use boa_engine::{Context, Source};
    use serde_json as json;

    fn eval(context: &mut Context, code: &str) -> json::Value {
        context
            .eval(Source::from_bytes(code))
            .unwrap()
            .to_json(context)
            .unwrap()
            .unwrap_or(json::Value::Null)
    }

    fn collect(
        context: &mut Context,
        acknowledged: u64,
        sequence: u64,
    ) -> Vec<u32> {
        json::from_value(eval(
            context,
            &collect_expression(acknowledged, sequence),
        ))
        .unwrap()
    }

    fn hit(context: &mut Context, edge: usize, count: u8) {
        eval(
            context,
            &format!(
                "globalThis.{NAMESPACE}.{EDGES_CURRENT}[{edge}] += {count}"
            ),
        );
    }

    #[test]
    fn collect_without_coverage_returns_nothing() {
        let mut context = Context::default();
        assert_eq!(collect(&mut context, 0, 1), Vec::<u32>::new());
    }

    #[test]
    fn collect_takes_hits_and_resends_until_acknowledged() {
        let mut context = Context::default();
        eval(
            &mut context,
            &format!(
                "globalThis.{NAMESPACE} = {{ {EDGES_CURRENT}: new Uint8Array({EDGE_MAP_SIZE}) }}"
            ),
        );

        hit(&mut context, 5, 2);
        assert_eq!(collect(&mut context, 0, 1), vec![5, 2]);

        // The response to collection 1 never arrived, so 1 stays
        // unacknowledged and its hits are sent again.
        hit(&mut context, 7, 1);
        assert_eq!(collect(&mut context, 0, 2), vec![5, 2, 7, 1]);

        // Once collection 2 is acknowledged, nothing is sent twice.
        assert_eq!(collect(&mut context, 2, 3), Vec::<u32>::new());
        hit(&mut context, 5, 1);
        hit(&mut context, 65535, 3);
        assert_eq!(collect(&mut context, 3, 4), vec![5, 1, 65535, 3]);
        assert_eq!(collect(&mut context, 4, 5), Vec::<u32>::new());
    }

    fn merge(context: &mut Context, hits: &str) {
        eval(context, &format!("(() => {{ {} }})()", merge_script(hits)));
    }

    fn page_edges(context: &mut Context, edges: &[usize]) -> Vec<u32> {
        edges
            .iter()
            .map(|edge| {
                eval(
                    context,
                    &format!("window.{NAMESPACE}.{EDGES_CURRENT}[{edge}]"),
                )
                .as_f64()
                .unwrap() as u32
            })
            .collect()
    }

    #[test]
    fn merge_adds_worker_hits_to_page_hits() {
        let mut context = Context::default();
        eval(
            &mut context,
            &format!(
                "var window = globalThis;
                 window.{NAMESPACE} = {{ {EDGES_CURRENT}: new Uint8Array({EDGE_MAP_SIZE}) }};
                 window.{NAMESPACE}.{EDGES_CURRENT}[3] = 1;
                 window.{NAMESPACE}.{EDGES_CURRENT}[9] = 200;"
            ),
        );

        merge(&mut context, "[3, 2, 9, 100, 12, 1, 12, 1]");
        assert_eq!(page_edges(&mut context, &[3, 9, 12]), vec![3, 255, 2]);
    }

    #[test]
    fn merge_creates_page_coverage_when_missing() {
        let mut context = Context::default();
        eval(&mut context, "var window = globalThis;");

        merge(&mut context, "[42, 1]");
        assert_eq!(page_edges(&mut context, &[41, 42]), vec![0, 1]);
        assert_eq!(
            eval(
                &mut context,
                &format!("window.{NAMESPACE}.edges_previous.length")
            )
            .as_f64(),
            Some(EDGE_MAP_SIZE as f64)
        );
    }
}
