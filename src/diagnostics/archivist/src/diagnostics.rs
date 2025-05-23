// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect::{
    ExponentialHistogramParams, HistogramProperty, LinearHistogramParams, Node, NumericProperty,
    UintExponentialHistogramProperty, UintLinearHistogramProperty, UintProperty,
};
use fuchsia_sync::Mutex;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use zx::{self as zx, MonotonicDuration};

pub(crate) static TRACE_CATEGORY: &CStr = c"archivist";

// Exponential histograms for time in microseconds contains power-of-two intervals
static TIME_USEC_PARAMS: ExponentialHistogramParams<u64> =
    ExponentialHistogramParams { floor: 0, initial_step: 1, step_multiplier: 2, buckets: 26 };

// Linear histogram for max snapshot size in bytes requested by clients.
// Divide configs into 10kb buckets, from 0mb to 1mb.
static MAX_SNAPSHOT_SIZE_BYTES_PARAMS: LinearHistogramParams<u64> =
    LinearHistogramParams { floor: 0, step_size: 10000, buckets: 100 };

// Linear histogram tracking percent of schemas truncated for a given snapshot.
// Divide configs into 5% buckets, from 0% to 100%.
static SNAPSHOT_SCHEMA_TRUNCATION_PARAMS: LinearHistogramParams<u64> =
    LinearHistogramParams { floor: 0, step_size: 5, buckets: 20 };

pub struct AccessorStats {
    /// Inspect node for tracking usage/health metrics of diagnostics platform.
    _node: Node,

    /// Metrics aggregated across all client connections.
    pub global_stats: Arc<GlobalAccessorStats>,

    /// Global stats tracking the usages of StreamDiagnostics for
    /// exfiltrating inspect data.
    pub inspect_stats: Arc<GlobalConnectionStats>,

    /// Global stats tracking the usages of StreamDiagnostics for
    /// exfiltrating logs.
    pub logs_stats: Arc<GlobalConnectionStats>,
}

pub struct GlobalAccessorStats {
    /// Property tracking number of opening connections to any archive_accessor instance.
    pub connections_opened: UintProperty,
    /// Property tracking number of closing connections to any archive_accessor instance.
    pub connections_closed: UintProperty,
    /// Number of requests to a single ArchiveAccessor to StreamDiagnostics, starting a
    /// new inspect ReaderServer.
    pub stream_diagnostics_requests: UintProperty,
}

impl AccessorStats {
    pub fn new(node: Node) -> Self {
        let connections_opened = node.create_uint("connections_opened", 0);
        let connections_closed = node.create_uint("connections_closed", 0);

        let stream_diagnostics_requests = node.create_uint("stream_diagnostics_requests", 0);

        let inspect_stats = Arc::new(GlobalConnectionStats::new(node.create_child("inspect")));
        let logs_stats = Arc::new(GlobalConnectionStats::new(node.create_child("logs")));

        AccessorStats {
            _node: node,
            global_stats: Arc::new(GlobalAccessorStats {
                connections_opened,
                connections_closed,
                stream_diagnostics_requests,
            }),
            inspect_stats,
            logs_stats,
        }
    }

    pub fn new_inspect_batch_iterator(&self) -> BatchIteratorConnectionStats {
        self.inspect_stats.new_batch_iterator_connection()
    }

    pub fn new_logs_batch_iterator(&self) -> BatchIteratorConnectionStats {
        self.logs_stats.new_batch_iterator_connection()
    }
}

pub struct GlobalConnectionStats {
    /// Weak clone of the node that stores stats, used for on-demand population.
    node: Node,
    /// Number of DiagnosticsServers created in response to an StreamDiagnostics
    /// client request.
    reader_servers_constructed: UintProperty,
    /// Number of DiagnosticsServers destroyed in response to falling out of scope.
    reader_servers_destroyed: UintProperty,
    /// Stats about BatchIterator connections.
    batch_iterator: GlobalBatchIteratorStats,
    /// Property tracking number of times a future to retrieve diagnostics data for a component
    /// timed out.
    component_timeouts_count: UintProperty,
    /// Number of times a diagnostics schema had to be truncated because it would otherwise
    /// cause a component to exceed its configured size budget.
    schema_truncation_count: UintProperty,
    /// Optional histogram of processing times for individual components in GetNext
    component_time_usec: OnceLock<UintExponentialHistogramProperty>,
    /// Histogram of max aggregated snapshot sizes for overall Snapshot requests.
    max_snapshot_sizes_bytes: UintLinearHistogramProperty,
    /// Percentage of schemas in a single snapshot that got truncated.
    snapshot_schema_truncation_percentage: UintLinearHistogramProperty,
    /// Longest processing times for individual components, with timestamps.
    processing_time_tracker: OnceLock<Mutex<ProcessingTimeTracker>>,
    /// Node under which the batch iterator connections stats are created.
    batch_iterator_connections: Node,
    /// The id of the next BatchIterator connection.
    next_connection_id: AtomicUsize,
}

impl GlobalConnectionStats {
    pub fn new(node: Node) -> Self {
        let reader_servers_constructed = node.create_uint("reader_servers_constructed", 0);
        let reader_servers_destroyed = node.create_uint("reader_servers_destroyed", 0);

        let batch_iterator = GlobalBatchIteratorStats::new(&node);
        let component_timeouts_count = node.create_uint("component_timeouts_count", 0);

        let max_snapshot_sizes_bytes = node.create_uint_linear_histogram(
            "max_snapshot_sizes_bytes",
            MAX_SNAPSHOT_SIZE_BYTES_PARAMS.clone(),
        );

        let snapshot_schema_truncation_percentage = node.create_uint_linear_histogram(
            "snapshot_schema_truncation_percentage",
            SNAPSHOT_SCHEMA_TRUNCATION_PARAMS.clone(),
        );

        let schema_truncation_count = node.create_uint("schema_truncation_count", 0);
        let batch_iterator_connections = node.create_child("batch_iterator_connections");

        GlobalConnectionStats {
            node,
            reader_servers_constructed,
            reader_servers_destroyed,
            batch_iterator,
            batch_iterator_connections,
            component_timeouts_count,
            max_snapshot_sizes_bytes,
            snapshot_schema_truncation_percentage,
            schema_truncation_count,
            component_time_usec: OnceLock::new(),
            processing_time_tracker: OnceLock::new(),
            next_connection_id: AtomicUsize::new(0),
        }
    }

    fn new_batch_iterator_connection(self: &Arc<Self>) -> BatchIteratorConnectionStats {
        let node = self
            .batch_iterator_connections
            .create_child(self.next_connection_id.fetch_add(1, Ordering::Relaxed).to_string());
        BatchIteratorConnectionStats::new(node, Arc::clone(self))
    }

    pub fn add_timeout(&self) {
        self.component_timeouts_count.add(1);
    }

    pub fn record_percent_truncated_schemas(&self, percent_truncated_schemas: u64) {
        self.snapshot_schema_truncation_percentage.insert(percent_truncated_schemas);
    }

    pub fn record_max_snapshot_size_config(&self, max_snapshot_size_config: u64) {
        self.max_snapshot_sizes_bytes.insert(max_snapshot_size_config);
    }

    /// Record the duration of a whole request to GetNext.
    pub fn record_batch_duration(&self, duration: MonotonicDuration) {
        let micros = duration.into_micros();
        if micros >= 0 {
            self.batch_iterator.get_next.time_usec.insert(micros as u64);
        }
    }

    /// Record the duration of obtaining data from a single component.
    pub fn record_component_duration(&self, moniker: impl AsRef<str>, duration: MonotonicDuration) {
        let nanos = duration.into_nanos();
        if nanos >= 0 {
            // Lazily initialize stats that may not be needed for all diagnostics types.

            let component_time_usec = self.component_time_usec.get_or_init(|| {
                self.node.create_uint_exponential_histogram(
                    "component_time_usec",
                    TIME_USEC_PARAMS.clone(),
                )
            });

            let processing_time_tracker = self.processing_time_tracker.get_or_init(|| {
                Mutex::new(ProcessingTimeTracker::new(
                    self.node.create_child("longest_processing_times"),
                ))
            });

            component_time_usec.insert(nanos as u64 / 1000);
            processing_time_tracker.lock().track(moniker.as_ref(), nanos as u64);
        }
    }
}

struct GlobalBatchIteratorStats {
    _node: Node,
    /// Property tracking number of opening connections to any batch iterator instance.
    connections_opened: UintProperty,
    /// Property tracking number of closing connections to any batch iterator instance.
    connections_closed: UintProperty,
    get_next: GlobalBatchIteratorGetNextStats,
}

impl GlobalBatchIteratorStats {
    fn new(parent: &Node) -> Self {
        let node = parent.create_child("batch_iterator");
        let connections_opened = node.create_uint("connections_opened", 0);
        let connections_closed = node.create_uint("connections_closed", 0);
        let get_next = GlobalBatchIteratorGetNextStats::new(&node);
        Self { _node: node, connections_opened, connections_closed, get_next }
    }
}

struct GlobalBatchIteratorGetNextStats {
    _node: Node,
    /// Number of times "GetNext" was called
    requests: UintProperty,
    /// Number of times a "GetNext" response was sent
    responses: UintProperty,
    /// Number of items returned in batches from "GetNext"
    result_count: UintProperty,
    /// Number of items returned in batches from "GetNext" that contained errors
    result_errors: UintProperty,
    /// Histogram of processing times for overall "GetNext" requests.
    time_usec: UintExponentialHistogramProperty,
}

impl GlobalBatchIteratorGetNextStats {
    fn new(parent: &Node) -> Self {
        let node = parent.create_child("get_next");
        let requests = node.create_uint("requests", 0);
        let responses = node.create_uint("responses", 0);
        let result_count = node.create_uint("result_count", 0);
        let result_errors = node.create_uint("result_errors", 0);
        let time_usec =
            node.create_uint_exponential_histogram("time_usec", TIME_USEC_PARAMS.clone());
        Self { _node: node, requests, responses, result_count, result_errors, time_usec }
    }
}

const PROCESSING_TIME_COMPONENT_COUNT_LIMIT: usize = 20;

/// Holds stats on the longest processing times for individual components' data.
struct ProcessingTimeTracker {
    /// The node holding all properties for the tracker.
    node: Node,
    /// Map from component moniker to a tuple of its time and a node containing the stats about it.
    longest_times_by_component: BTreeMap<String, (u64, Node)>,
    /// The shortest time seen so far. If a new component is being
    /// recorded and its time is greater than this, we need to pop the
    /// entry containing this time.
    shortest_time_ns: u64,
}

impl ProcessingTimeTracker {
    fn new(node: Node) -> Self {
        Self { node, longest_times_by_component: BTreeMap::new(), shortest_time_ns: u64::MAX }
    }
    fn track(&mut self, moniker: &str, time_ns: u64) {
        let at_capacity =
            self.longest_times_by_component.len() >= PROCESSING_TIME_COMPONENT_COUNT_LIMIT;

        // Do nothing if the container it as the limit and the new time doesn't need to get
        // inserted.
        if at_capacity && time_ns < self.shortest_time_ns {
            return;
        }

        let parent_node = &self.node;

        let make_entry = || {
            let n = parent_node.create_child(moniker.to_string());
            n.record_int("@time", zx::BootInstant::get().into_nanos());
            n.record_double("duration_seconds", time_ns as f64 / 1e9);
            (time_ns, n)
        };

        self.longest_times_by_component
            .entry(moniker.to_string())
            .and_modify(move |v| {
                if v.0 < time_ns {
                    *v = make_entry();
                }
            })
            .or_insert_with(make_entry);

        // Repeatedly find the key for the smallest time and remove it until we are under the
        // limit.
        while self.longest_times_by_component.len() > PROCESSING_TIME_COMPONENT_COUNT_LIMIT {
            let mut key = "".to_string();
            for (k, (val, _)) in &self.longest_times_by_component {
                if *val == self.shortest_time_ns {
                    key.clone_from(k);
                    break;
                }
            }
            self.longest_times_by_component.remove(&key);
            self.shortest_time_ns =
                self.longest_times_by_component.values().map(|v| v.0).min().unwrap_or(u64::MAX);
        }

        self.shortest_time_ns = std::cmp::min(self.shortest_time_ns, time_ns);
    }
}

pub struct BatchIteratorConnectionStats {
    /// Inspect node for tracking usage/health metrics of a single connection to a batch iterator.
    _node: Node,
    /// Global stats for connections to the BatchIterator protocol.
    global_stats: Arc<GlobalConnectionStats>,
    /// Property tracking number of requests to the BatchIterator instance this struct is tracking.
    get_next_requests: UintProperty,
    /// Property tracking number of responses from the BatchIterator instance this struct is tracking.
    get_next_responses: UintProperty,
    /// Property tracking number of times the batch iterator has served a terminal batch signalling that
    /// the client has reached the end of the iterator and should terminate their connection.
    get_next_terminal_responses: UintProperty,
}

impl BatchIteratorConnectionStats {
    fn new(node: Node, global_stats: Arc<GlobalConnectionStats>) -> Self {
        // we'll decrement these on drop
        global_stats.reader_servers_constructed.add(1);

        let get_next = node.create_child("get_next");
        let get_next_requests = get_next.create_uint("requests", 0);
        let get_next_responses = get_next.create_uint("responses", 0);
        let get_next_terminal_responses = get_next.create_uint("terminal_responses", 0);
        node.record(get_next);

        Self {
            _node: node,
            global_stats,
            get_next_requests,
            get_next_responses,
            get_next_terminal_responses,
        }
    }

    pub fn open_connection(&self) {
        self.global_stats.batch_iterator.connections_opened.add(1);
    }

    pub fn close_connection(&self) {
        self.global_stats.batch_iterator.connections_closed.add(1);
    }

    pub fn global_stats(&self) -> &Arc<GlobalConnectionStats> {
        &self.global_stats
    }

    pub fn add_request(&self) {
        self.global_stats.batch_iterator.get_next.requests.add(1);
        self.get_next_requests.add(1);
    }

    pub fn add_response(&self) {
        self.global_stats.batch_iterator.get_next.responses.add(1);
        self.get_next_responses.add(1);
    }

    pub fn add_terminal(&self) {
        self.get_next_terminal_responses.add(1);
    }

    pub fn add_result(&self) {
        self.global_stats.batch_iterator.get_next.result_count.add(1);
    }

    pub fn add_result_error(&self) {
        self.global_stats.batch_iterator.get_next.result_errors.add(1);
    }

    pub fn add_schema_truncated(&self) {
        self.global_stats.schema_truncation_count.add(1);
    }
}

impl Drop for BatchIteratorConnectionStats {
    fn drop(&mut self) {
        self.global_stats.reader_servers_destroyed.add(1);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use diagnostics_assertions::{assert_data_tree, AnyProperty};
    use fuchsia_inspect::health::Reporter;
    use fuchsia_inspect::{component, Inspector};

    #[fuchsia::test]
    async fn health() {
        component::health().set_ok();
        assert_data_tree!(component::inspector(),
        root: {
            "fuchsia.inspect.Health": {
                status: "OK",
                start_timestamp_nanos: AnyProperty,
            }
        });

        component::health().set_unhealthy("Bad state");
        assert_data_tree!(component::inspector(),
        root: contains {
            "fuchsia.inspect.Health": {
                status: "UNHEALTHY",
                message: "Bad state",
                start_timestamp_nanos: AnyProperty,
            }
        });

        component::health().set_ok();
        assert_data_tree!(component::inspector(),
        root: contains {
            "fuchsia.inspect.Health": {
                status: "OK",
                start_timestamp_nanos: AnyProperty,
            }
        });
    }

    #[fuchsia::test]
    async fn processing_time_tracker() {
        let inspector = Inspector::default();
        let mut tracker = ProcessingTimeTracker::new(inspector.root().create_child("test"));

        tracker.track("a", 1e9 as u64);
        assert_data_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 1f64
                }
            }
        });

        tracker.track("a", 5e8 as u64);
        assert_data_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 1f64
                }
            }
        });

        tracker.track("a", 5500e6 as u64);
        assert_data_tree!(inspector,
        root: {
            test: {
                a: {
                    "@time": AnyProperty,
                    duration_seconds: 5.5f64
                }
            }
        });

        for time in 0..60 {
            tracker.track(&format!("b{time}"), time * 1e9 as u64);
        }

        assert_data_tree!(inspector,
        root: {
            test: {
                b40: { "@time": AnyProperty, duration_seconds: 40f64 },
                b41: { "@time": AnyProperty, duration_seconds: 41f64 },
                b42: { "@time": AnyProperty, duration_seconds: 42f64 },
                b43: { "@time": AnyProperty, duration_seconds: 43f64 },
                b44: { "@time": AnyProperty, duration_seconds: 44f64 },
                b45: { "@time": AnyProperty, duration_seconds: 45f64 },
                b46: { "@time": AnyProperty, duration_seconds: 46f64 },
                b47: { "@time": AnyProperty, duration_seconds: 47f64 },
                b48: { "@time": AnyProperty, duration_seconds: 48f64 },
                b49: { "@time": AnyProperty, duration_seconds: 49f64 },
                b50: { "@time": AnyProperty, duration_seconds: 50f64 },
                b51: { "@time": AnyProperty, duration_seconds: 51f64 },
                b52: { "@time": AnyProperty, duration_seconds: 52f64 },
                b53: { "@time": AnyProperty, duration_seconds: 53f64 },
                b54: { "@time": AnyProperty, duration_seconds: 54f64 },
                b55: { "@time": AnyProperty, duration_seconds: 55f64 },
                b56: { "@time": AnyProperty, duration_seconds: 56f64 },
                b57: { "@time": AnyProperty, duration_seconds: 57f64 },
                b58: { "@time": AnyProperty, duration_seconds: 58f64 },
                b59: { "@time": AnyProperty, duration_seconds: 59f64 },
            }
        });
    }
}
