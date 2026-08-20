//! Wire types for shared compute, on TCP port 9006.
//!
//! Two kinds of work travel over this: **model inference**, handed to a local
//! Ollama, and **declared tasks** — named commands the owner of a machine has
//! written into its own config. Nothing else runs. A remote node can invoke
//! `render` on a machine whose owner defined `render`; it cannot invent work,
//! and it cannot reach a shell.
//!
//! That constraint is the whole security model, and it is deliberate. A LAN
//! service that executed arbitrary commands would mean any device on the
//! network — or any device later compromised — owned every machine that
//! trusted it. Declared tasks keep the decision with the person whose hardware
//! it is.
//!
//! Framing is the same length-prefixed JSON used everywhere else, so a job's
//! output can stream back as a series of frames rather than arriving all at
//! once when it finishes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::resources::hardware::Gpu;

/// TCP port for compute requests.
pub const DEFAULT_COMPUTE_PORT: u16 = 9006;

/// A job producing more output than this is cut off.
///
/// Bounded because the output is buffered for the requesting node, and a task
/// stuck in a logging loop must not exhaust either machine's memory.
pub const MAX_JOB_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// How long a pairing code is valid.
pub const PAIRING_CODE_TTL_SECS: i64 = 180;

// ─── What a node offers ──────────────────────────────────────────────────────

/// A node's compute capabilities, carried in its discovery beacon so the whole
/// network knows what is available without asking.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// False when the owner has switched sharing off entirely.
    pub enabled: bool,
    /// Graphics adapters, most capable first.
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    /// Models this node can run right now, via a local Ollama.
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    /// Task names the owner has declared. Values are descriptions, for the UI.
    #[serde(default)]
    pub tasks: Vec<TaskInfo>,
    /// Logical CPUs offered.
    #[serde(default)]
    pub cpu_threads: usize,
    /// Memory offered, in MB.
    #[serde(default)]
    pub ram_mb: u64,
    /// Jobs currently running here.
    #[serde(default)]
    pub running_jobs: usize,
    /// Jobs this node will run at once.
    #[serde(default)]
    pub max_concurrent_jobs: usize,
    /// True when a peer must pair before submitting work.
    #[serde(default)]
    pub requires_pairing: bool,
}

impl Capabilities {
    /// Is this node worth sending work to at all?
    pub fn is_useful(&self) -> bool {
        self.enabled && (!self.models.is_empty() || !self.tasks.is_empty())
    }

    /// Is it accepting more work right now?
    pub fn has_capacity(&self) -> bool {
        self.enabled && self.running_jobs < self.max_concurrent_jobs.max(1)
    }

    pub fn has_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m.name == model)
    }

    /// The best accelerator, if any.
    pub fn accelerator(&self) -> Option<&Gpu> {
        self.gpus.iter().find(|g| g.vendor.is_accelerator())
    }
}

/// A model available through the local runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// As the runtime names it, e.g. `llama3.2:3b`.
    pub name: String,
    /// On-disk size in bytes, where known.
    #[serde(default)]
    pub size_bytes: u64,
    /// Parameter count as reported, e.g. `"3.2B"`.
    #[serde(default)]
    pub parameters: Option<String>,
    /// Quantisation, e.g. `"Q4_K_M"`.
    #[serde(default)]
    pub quantization: Option<String>,
}

impl ModelInfo {
    pub fn size_gb(&self) -> f64 {
        self.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

/// A task this node's owner has declared it will run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    /// The name a remote node asks for.
    pub name: String,
    /// What it does, shown in the dashboard.
    #[serde(default)]
    pub description: String,
    /// Parameter names the task's template expects.
    #[serde(default)]
    pub parameters: Vec<String>,
    /// True when the task wants a GPU, so work can be routed accordingly.
    #[serde(default)]
    pub needs_gpu: bool,
}

// ─── Requests ────────────────────────────────────────────────────────────────

/// Everything a node can be asked to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ComputeRequest {
    /// What can you do?
    Describe,
    /// Begin pairing. The user reads a code off the target node's screen.
    Pair(PairRequest),
    /// Run a model.
    Infer(InferRequest),
    /// Run a declared task.
    RunTask(TaskRequest),
    /// Stop a running job.
    Cancel { job_id: String },
    /// What is running here?
    ListJobs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub node_id: String,
    pub display_name: String,
    /// The six digits shown on the other machine.
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferRequest {
    /// Who is asking. The token proves it; this says which peer to check it
    /// against, and gives the worker something meaningful to attribute the
    /// job to in its own dashboard.
    #[serde(default)]
    pub requester: String,
    /// Proof of an earlier successful pairing.
    #[serde(default)]
    pub token: Option<String>,
    pub model: String,
    pub prompt: String,
    /// Optional system prompt.
    #[serde(default)]
    pub system: Option<String>,
    /// 0.0–2.0; the runtime's default when absent.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Cap on generated tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    /// Who is asking; see [`InferRequest::requester`].
    #[serde(default)]
    pub requester: String,
    #[serde(default)]
    pub token: Option<String>,
    /// A name from the target's declared tasks.
    pub task: String,
    /// Values for the template's placeholders.
    ///
    /// Substituted as whole argv entries, never through a shell, so a value
    /// containing `; rm -rf /` is passed to the program as that literal
    /// string and cannot become another command.
    #[serde(default)]
    pub parameters: std::collections::HashMap<String, String>,
}

// ─── Responses ───────────────────────────────────────────────────────────────

/// Replies, several of which stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ComputeResponse {
    /// Answer to [`ComputeRequest::Describe`].
    Capabilities(Box<Capabilities>),
    /// Pairing succeeded; keep the token.
    Paired { token: String },
    /// The request was refused, with a reason fit to show a user.
    Refused { reason: String },
    /// Work has started.
    Started {
        job_id: String,
        started_at: DateTime<Utc>,
    },
    /// A chunk of output. Sent repeatedly.
    Output { job_id: String, text: String },
    /// The job ended.
    Finished(JobResult),
    /// Answer to [`ComputeRequest::ListJobs`].
    Jobs { jobs: Vec<JobStatus> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: String,
    pub success: bool,
    /// Exit status for a task; absent for inference.
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Tokens generated, for inference.
    #[serde(default)]
    pub tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Finished,
    Failed,
    Cancelled,
}

/// A job, as seen from either end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub job_id: String,
    /// What was asked for: a model name, or a task name.
    pub kind: String,
    /// Which node asked.
    pub requester: String,
    /// Which node is running it.
    pub worker: String,
    pub state: JobState,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// Output so far, capped at [`MAX_JOB_OUTPUT_BYTES`].
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl JobStatus {
    pub fn is_live(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Running)
    }

    pub fn duration_secs(&self) -> i64 {
        (self.finished_at.unwrap_or_else(Utc::now) - self.started_at).num_seconds()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::hardware::GpuVendor;

    fn gpu(name: &str, vendor: GpuVendor) -> Gpu {
        Gpu {
            name: name.into(),
            vram_mb: Some(8192),
            vendor,
        }
    }

    #[test]
    fn a_node_with_nothing_declared_is_not_worth_asking() {
        let mut caps = Capabilities {
            enabled: true,
            ..Capabilities::default()
        };
        assert!(
            !caps.is_useful(),
            "no models and no tasks is nothing to offer"
        );

        caps.models.push(ModelInfo {
            name: "llama3.2:3b".into(),
            size_bytes: 2_000_000_000,
            parameters: Some("3.2B".into()),
            quantization: Some("Q4_K_M".into()),
        });
        assert!(caps.is_useful());

        // Switching sharing off withdraws the offer regardless.
        caps.enabled = false;
        assert!(!caps.is_useful());
    }

    #[test]
    fn capacity_respects_the_concurrency_limit() {
        let mut caps = Capabilities {
            enabled: true,
            max_concurrent_jobs: 2,
            running_jobs: 0,
            ..Capabilities::default()
        };
        assert!(caps.has_capacity());

        caps.running_jobs = 2;
        assert!(!caps.has_capacity(), "at the limit, no more work");

        // An unset limit must not mean "never accept anything".
        let unset = Capabilities {
            enabled: true,
            max_concurrent_jobs: 0,
            running_jobs: 0,
            ..Capabilities::default()
        };
        assert!(unset.has_capacity());
    }

    #[test]
    fn the_accelerator_is_the_discrete_gpu_not_the_integrated_one() {
        let caps = Capabilities {
            enabled: true,
            gpus: vec![
                gpu("NVIDIA GeForce RTX 4060 Laptop GPU", GpuVendor::Nvidia),
                gpu("Intel(R) UHD Graphics", GpuVendor::Intel),
            ],
            ..Capabilities::default()
        };
        assert_eq!(
            caps.accelerator().map(|g| g.name.as_str()),
            Some("NVIDIA GeForce RTX 4060 Laptop GPU")
        );

        let integrated_only = Capabilities {
            enabled: true,
            gpus: vec![gpu("Intel(R) UHD Graphics", GpuVendor::Intel)],
            ..Capabilities::default()
        };
        assert!(integrated_only.accelerator().is_none());
    }

    #[test]
    fn requests_round_trip_through_json() {
        let request = ComputeRequest::Infer(InferRequest {
            requester: "asker".into(),
            token: Some("t".into()),
            model: "llama3.2:3b".into(),
            prompt: "hello".into(),
            system: None,
            temperature: Some(0.7),
            max_tokens: Some(256),
        });

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"Infer\""));

        match serde_json::from_str::<ComputeRequest>(&json).unwrap() {
            ComputeRequest::Infer(r) => {
                assert_eq!(r.model, "llama3.2:3b");
                assert_eq!(r.temperature, Some(0.7));
            }
            other => panic!("round-tripped into {other:?}"),
        }
    }

    #[test]
    fn a_running_job_is_live_and_a_finished_one_is_not() {
        let mut job = JobStatus {
            job_id: "j1".into(),
            kind: "render".into(),
            requester: "a".into(),
            worker: "b".into(),
            state: JobState::Running,
            started_at: Utc::now(),
            finished_at: None,
            output: String::new(),
            error: None,
        };
        assert!(job.is_live());

        for state in [JobState::Finished, JobState::Failed, JobState::Cancelled] {
            job.state = state;
            assert!(!job.is_live(), "{state:?} should not be live");
        }
    }
}
