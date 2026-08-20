//! The orchestrator: one view of the whole network's capacity, and the
//! placement decisions that follow from it.
//!
//! Every node already broadcasts what it has — GPUs, memory, threads, the
//! models it can run, the tasks its owner has declared, and how busy it is.
//! The orchestrator pools that into a single picture and answers one question
//! well: *given this piece of work, which machine should do it?*
//!
//! # Why every node runs one
//!
//! There is no elected leader and no central bus process. A single coordinator
//! would be a single point of failure on a network whose whole premise is that
//! machines come and go — close the laptop that happens to be coordinating and
//! the cluster stops. Instead each node schedules the work *it* submits, using
//! the capacity everyone advertises. Placement is therefore always local,
//! always instant, and survives any other machine disappearing.
//!
//! Two nodes can pick the same worker at the same moment. That is handled
//! where it must be — the worker enforces its own concurrency limit and
//! refuses the surplus, and the submitting node tries its next choice. Trying
//! to prevent it with distributed locking would cost far more than the
//! occasional retry.
//!
//! # Priorities
//!
//! Work is queued, not fired blindly. A queue lets an interactive prompt
//! overtake an overnight render, which is the difference between a cluster
//! that feels responsive and one that merely has capacity.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::compute::protocol::{Capabilities, JobState};
use crate::net::peers::Peer;

/// A placement scoring below this cannot actually run the work.
///
/// The scale is built so that a node able to run something at all clears
/// 1000; everything below is a node that lacks the model, the task, or the
/// GPU it needs.
const PLACEABLE_SCORE: i64 = 1000;

/// Workers to try before declaring a job unplaceable.
const MAX_ATTEMPTS: usize = 4;

/// How urgent a piece of work is.
///
/// Ordered so that `Interactive > Normal > Background`, because the queue is a
/// max-heap by priority and then by arrival.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Overnight work. Yields to everything else.
    Background = 0,
    /// The default.
    #[default]
    Normal = 1,
    /// Someone is waiting on the answer — a prompt typed into a dashboard.
    Interactive = 2,
}

/// What is being asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// Run a prompt through a model.
    Inference {
        model: String,
        prompt: String,
        #[serde(default)]
        system: Option<String>,
        #[serde(default)]
        temperature: Option<f32>,
    },
    /// Run a task the target has declared.
    Task {
        name: String,
        #[serde(default)]
        parameters: HashMap<String, String>,
        /// Placement should prefer a machine with a real GPU.
        #[serde(default)]
        needs_gpu: bool,
    },
}

impl Work {
    /// A short label for the dashboard.
    pub fn label(&self) -> String {
        match self {
            Self::Inference { model, .. } => model.clone(),
            Self::Task { name, .. } => name.clone(),
        }
    }

    /// Can `capabilities` run this at all?
    fn can_run(&self, capabilities: &Capabilities) -> bool {
        match self {
            Self::Inference { model, .. } => capabilities.has_model(model),
            Self::Task {
                name, needs_gpu, ..
            } => {
                let declared = capabilities.tasks.iter().any(|t| &t.name == name);
                declared && (!needs_gpu || capabilities.accelerator().is_some())
            }
        }
    }
}

/// A queued or running piece of work, as the submitting node sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub work: Work,
    pub priority: Priority,
    /// The node chosen to run it, once one has been.
    #[serde(default)]
    pub worker: Option<String>,
    #[serde(default)]
    pub worker_name: Option<String>,
    pub state: JobState,
    pub submitted_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub error: Option<String>,
    /// Workers already tried and found unavailable.
    #[serde(default)]
    pub attempts: Vec<String>,
}

impl ScheduledJob {
    pub fn is_live(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Running)
    }

    pub fn duration_secs(&self) -> i64 {
        let from = self.started_at.unwrap_or(self.submitted_at);
        (self.finished_at.unwrap_or_else(Utc::now) - from).num_seconds()
    }
}

/// A candidate worker and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub node_id: String,
    pub display_name: String,
    pub score: i64,
    /// Human-readable justification, shown in the dashboard.
    pub reason: String,
}

/// Everything the network can currently do, pooled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterCapacity {
    /// Nodes offering compute, including this one.
    pub nodes: usize,
    pub gpus: usize,
    pub accelerators: usize,
    pub total_vram_mb: u64,
    pub cpu_threads: usize,
    pub ram_mb: u64,
    pub running_jobs: usize,
    pub free_slots: usize,
    /// Every distinct model anywhere on the network, sorted.
    pub models: Vec<String>,
    /// Every distinct declared task, sorted.
    pub tasks: Vec<String>,
}

/// One device offering compute, as the dashboard lists it.
///
/// The pooled totals answer "what can the network do"; this answers "which
/// machine is it on", which is what you need before you can decide to pair
/// with something, or work out why a model is missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCapacity {
    pub node_id: String,
    pub display_name: String,
    /// True for the node serving this dashboard.
    pub is_self: bool,
    pub capabilities: Capabilities,
}

/// Schedules work across the network.
#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<RwLock<Inner>>,
    node_id: String,
}

struct Inner {
    /// Off by default at the node's discretion; when off, nothing is placed.
    enabled: bool,
    queue: VecDeque<ScheduledJob>,
    jobs: HashMap<String, ScheduledJob>,
    /// What each peer last advertised, keyed by node id.
    capacity: HashMap<String, (String, Capabilities)>,
    /// This node's own capabilities, so it can schedule work onto itself.
    local: Capabilities,
}

impl std::fmt::Debug for Orchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchestrator")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl Orchestrator {
    pub fn new(node_id: String, enabled: bool) -> Self {
        Self {
            node_id,
            inner: Arc::new(RwLock::new(Inner {
                enabled,
                queue: VecDeque::new(),
                jobs: HashMap::new(),
                capacity: HashMap::new(),
                local: Capabilities::default(),
            })),
        }
    }

    pub async fn is_enabled(&self) -> bool {
        self.inner.read().await.enabled
    }

    /// Turn scheduling on or off.
    ///
    /// Switching it off leaves running work alone — killing someone's
    /// half-finished render because a toggle moved would be worse than
    /// letting it end — but nothing new is placed.
    pub async fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.write().await;
        inner.enabled = enabled;
        if !enabled {
            inner.queue.clear();
        }
        tracing::info!(enabled, "orchestrator toggled");
    }

    /// Record this node's own capabilities.
    pub async fn set_local_capabilities(&self, capabilities: Capabilities) {
        self.inner.write().await.local = capabilities;
    }

    /// Refresh the cluster view from the peer registry.
    pub async fn observe_peers(&self, peers: &[Peer]) {
        let mut inner = self.inner.write().await;
        inner.capacity.clear();
        for peer in peers.iter().filter(|p| p.online) {
            if peer.compute.enabled {
                inner.capacity.insert(
                    peer.node_id.clone(),
                    (peer.display_name().to_string(), peer.compute.clone()),
                );
            }
        }
    }

    /// Everything the network can do right now.
    pub async fn capacity(&self) -> ClusterCapacity {
        let inner = self.inner.read().await;
        let mut total = ClusterCapacity::default();
        let mut models = std::collections::BTreeSet::new();
        let mut tasks = std::collections::BTreeSet::new();

        let everything = std::iter::once(&inner.local)
            .filter(|c| c.enabled)
            .chain(inner.capacity.values().map(|(_, c)| c));

        for capabilities in everything {
            total.nodes += 1;
            total.gpus += capabilities.gpus.len();
            total.accelerators += capabilities
                .gpus
                .iter()
                .filter(|g| g.vendor.is_accelerator())
                .count();
            total.total_vram_mb += capabilities
                .gpus
                .iter()
                .filter_map(|g| g.vram_mb)
                .sum::<u64>();
            total.cpu_threads += capabilities.cpu_threads;
            total.ram_mb += capabilities.ram_mb;
            total.running_jobs += capabilities.running_jobs;
            total.free_slots += capabilities
                .max_concurrent_jobs
                .max(1)
                .saturating_sub(capabilities.running_jobs);

            models.extend(capabilities.models.iter().map(|m| m.name.clone()));
            tasks.extend(capabilities.tasks.iter().map(|t| t.name.clone()));
        }

        total.models = models.into_iter().collect();
        total.tasks = tasks.into_iter().collect();
        total
    }

    /// The same picture broken down by machine, this node first.
    pub async fn devices(&self) -> Vec<DeviceCapacity> {
        let inner = self.inner.read().await;

        let mut devices: Vec<DeviceCapacity> = inner
            .capacity
            .iter()
            .map(|(node_id, (display_name, capabilities))| DeviceCapacity {
                node_id: node_id.clone(),
                display_name: display_name.clone(),
                is_self: false,
                capabilities: capabilities.clone(),
            })
            .collect();
        devices.sort_by(|a, b| a.display_name.cmp(&b.display_name));

        // This node leads the list whether or not it is sharing: seeing it
        // there switched off is how you discover that it is switched off.
        devices.insert(
            0,
            DeviceCapacity {
                node_id: self.node_id.clone(),
                display_name: "This device".to_string(),
                is_self: true,
                capabilities: inner.local.clone(),
            },
        );
        devices
    }

    /// Choose the best node for a piece of work.
    ///
    /// Candidates are scored rather than filtered to one, so the caller can
    /// fall through to the next if its first choice turns out to be full.
    /// Returns them best-first.
    pub async fn plan(&self, work: &Work, exclude: &[String]) -> Vec<Placement> {
        let inner = self.inner.read().await;

        let mut candidates: Vec<(String, String, &Capabilities)> = inner
            .capacity
            .iter()
            .map(|(id, (name, caps))| (id.clone(), name.clone(), caps))
            .collect();

        // This node is a candidate for its own work: often the right answer,
        // and always the right fallback when nothing else can help.
        if inner.local.enabled {
            candidates.push((
                self.node_id.clone(),
                "this device".to_string(),
                &inner.local,
            ));
        }

        let mut placements: Vec<Placement> = candidates
            .into_iter()
            .filter(|(id, _, caps)| !exclude.contains(id) && work.can_run(caps))
            .map(|(id, name, caps)| {
                let mut score = 0i64;
                let mut reasons = Vec::new();

                // Free capacity dominates: a fast machine already at its limit
                // will finish this job later than a slower idle one.
                let free = caps
                    .max_concurrent_jobs
                    .max(1)
                    .saturating_sub(caps.running_jobs) as i64;
                score += free * 1000;
                if free == 0 {
                    reasons.push("busy".to_string());
                } else {
                    reasons.push(format!("{free} slot(s) free"));
                }

                // A real accelerator is worth a great deal for either kind of
                // work, and VRAM decides which models fit.
                if let Some(gpu) = caps.accelerator() {
                    score += 500;
                    score += (gpu.vram_mb.unwrap_or(0) / 1024) as i64 * 50;
                    reasons.push(gpu.summary());
                }

                score += caps.cpu_threads as i64 * 2;
                score += (caps.ram_mb / 1024) as i64;

                // Prefer someone else, slightly. Offloading is the point, and
                // it keeps the machine the user is sitting at responsive.
                if id == self.node_id {
                    score -= 250;
                    reasons.push("local".to_string());
                }

                Placement {
                    node_id: id,
                    display_name: name,
                    score,
                    reason: reasons.join(" · "),
                }
            })
            .collect();

        placements.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(a.display_name.cmp(&b.display_name))
        });
        placements
    }

    /// Queue a piece of work. Returns its id.
    pub async fn submit(&self, work: Work, priority: Priority) -> anyhow::Result<String> {
        let mut inner = self.inner.write().await;
        if !inner.enabled {
            anyhow::bail!("the orchestrator is switched off on this device");
        }

        let job = ScheduledJob {
            id: Uuid::new_v4().to_string(),
            work,
            priority,
            worker: None,
            worker_name: None,
            state: JobState::Queued,
            submitted_at: Utc::now(),
            started_at: None,
            finished_at: None,
            output: String::new(),
            error: None,
            attempts: Vec::new(),
        };

        let id = job.id.clone();
        inner.jobs.insert(id.clone(), job.clone());

        // Insert by priority, keeping arrival order within a level, so an
        // interactive prompt overtakes a queued render without starving it.
        let position = inner
            .queue
            .iter()
            .position(|queued| queued.priority < priority);
        match position {
            Some(index) => inner.queue.insert(index, job),
            None => inner.queue.push_back(job),
        }

        tracing::info!(%id, ?priority, "work queued");
        Ok(id)
    }

    /// Take the next job that something can run, with where to send it.
    pub async fn next_ready(&self) -> Option<(ScheduledJob, Placement)> {
        let queued = {
            let inner = self.inner.read().await;
            if !inner.enabled {
                return None;
            }
            inner.queue.front().cloned()?
        };

        let placements = self.plan(&queued.work, &queued.attempts).await;
        let placement = placements
            .into_iter()
            .find(|p| p.score >= PLACEABLE_SCORE)?;

        let mut inner = self.inner.write().await;
        inner.queue.pop_front();
        if let Some(job) = inner.jobs.get_mut(&queued.id) {
            job.state = JobState::Running;
            job.worker = Some(placement.node_id.clone());
            job.worker_name = Some(placement.display_name.clone());
            job.started_at = Some(Utc::now());
        }

        Some((queued, placement))
    }

    /// Append output as it streams back.
    pub async fn append_output(&self, job_id: &str, text: &str) {
        if let Some(job) = self.inner.write().await.jobs.get_mut(job_id) {
            job.output.push_str(text);
        }
    }

    /// Mark a job done.
    pub async fn finish(&self, job_id: &str, error: Option<String>) {
        if let Some(job) = self.inner.write().await.jobs.get_mut(job_id) {
            job.state = if error.is_some() {
                JobState::Failed
            } else {
                JobState::Finished
            };
            job.error = error;
            job.finished_at = Some(Utc::now());
        }
    }

    /// Put a job back after a worker refused it, so another can be tried.
    ///
    /// `reason` is why that worker would not take it, and it matters: if no
    /// other device can take the work either, this is what the job fails with.
    /// A job that quietly returns to a queue nothing will ever drain is the
    /// worst outcome available — it looks identical to one that is merely
    /// slow, so the first thing anyone hits (asking a machine they have not
    /// paired with yet) would show as "queued" indefinitely.
    pub async fn requeue(&self, job_id: &str, refused_by: &str, reason: String) {
        // Record the attempt, then release the lock: working out where else it
        // could go needs `plan`, which takes the same lock to read.
        let (work, attempts) = {
            let mut inner = self.inner.write().await;
            let Some(job) = inner.jobs.get_mut(job_id) else {
                return;
            };
            job.attempts.push(refused_by.to_string());
            job.worker = None;
            job.worker_name = None;
            (job.work.clone(), job.attempts.clone())
        };

        // Give up rather than circling a network that has nothing able to take
        // the work.
        let elsewhere = if attempts.len() >= MAX_ATTEMPTS {
            Vec::new()
        } else {
            self.plan(&work, &attempts).await
        };
        let placeable = elsewhere.iter().any(|p| p.score >= PLACEABLE_SCORE);

        let mut inner = self.inner.write().await;
        // Drop any entry still queued under this id, so a requeue can never
        // leave two copies of one job racing each other.
        inner.queue.retain(|queued| queued.id != job_id);

        let Some(job) = inner.jobs.get_mut(job_id) else {
            return;
        };
        // It may have been cancelled while the lock was down.
        if matches!(job.state, JobState::Cancelled) {
            return;
        }

        if !placeable {
            job.state = JobState::Failed;
            job.error = Some(reason);
            job.finished_at = Some(Utc::now());
            return;
        }

        job.state = JobState::Queued;
        let requeued = job.clone();
        let priority = requeued.priority;
        let position = inner.queue.iter().position(|q| q.priority < priority);
        match position {
            Some(index) => inner.queue.insert(index, requeued),
            None => inner.queue.push_back(requeued),
        }
    }

    pub async fn cancel(&self, job_id: &str) -> bool {
        let mut inner = self.inner.write().await;
        inner.queue.retain(|q| q.id != job_id);
        match inner.jobs.get_mut(job_id) {
            Some(job) if job.is_live() => {
                job.state = JobState::Cancelled;
                job.finished_at = Some(Utc::now());
                true
            }
            _ => false,
        }
    }

    pub async fn job(&self, job_id: &str) -> Option<ScheduledJob> {
        self.inner.read().await.jobs.get(job_id).cloned()
    }

    /// Every job, newest first.
    pub async fn jobs(&self) -> Vec<ScheduledJob> {
        let mut jobs: Vec<ScheduledJob> = self.inner.read().await.jobs.values().cloned().collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.submitted_at));
        jobs
    }

    /// Drop finished jobs, keeping the most recent.
    pub async fn prune(&self, keep: usize) {
        let mut inner = self.inner.write().await;
        let mut finished: Vec<(String, DateTime<Utc>)> = inner
            .jobs
            .iter()
            .filter(|(_, j)| !j.is_live())
            .map(|(id, j)| (id.clone(), j.finished_at.unwrap_or(j.submitted_at)))
            .collect();

        finished.sort_by_key(|(_, finished_at)| std::cmp::Reverse(*finished_at));
        for (id, _) in finished.into_iter().skip(keep) {
            inner.jobs.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::protocol::{ModelInfo, TaskInfo};
    use crate::resources::hardware::{Gpu, GpuVendor};

    fn model(name: &str) -> ModelInfo {
        ModelInfo {
            name: name.into(),
            size_bytes: 0,
            parameters: None,
            quantization: None,
        }
    }

    fn capable(gpu: bool, slots: usize, running: usize) -> Capabilities {
        Capabilities {
            enabled: true,
            gpus: if gpu {
                vec![Gpu {
                    name: "NVIDIA GeForce RTX 4060".into(),
                    vram_mb: Some(8192),
                    vendor: GpuVendor::Nvidia,
                }]
            } else {
                vec![]
            },
            models: vec![model("llama3.2:3b")],
            tasks: vec![TaskInfo {
                name: "render".into(),
                description: String::new(),
                parameters: vec![],
                needs_gpu: true,
            }],
            cpu_threads: 8,
            ram_mb: 16384,
            running_jobs: running,
            max_concurrent_jobs: slots,
            requires_pairing: true,
        }
    }

    fn peer(id: &str, name: &str, capabilities: Capabilities) -> Peer {
        let mut peer = crate::net::peers::Peer {
            node_id: id.into(),
            username: name.into(),
            alias: None,
            ip: "192.168.1.10".into(),
            device_type: "linux-laptop".into(),
            os: String::new(),
            app_version: String::new(),
            ports: Default::default(),
            telemetry: Default::default(),
            shared: Default::default(),
            compute: Capabilities::default(),
            first_seen: Utc::now(),
            last_seen: Utc::now(),
            online: true,
            beacons_seen: 1,
            latency_ms: None,
        };
        peer.compute = capabilities;
        peer
    }

    #[tokio::test]
    async fn a_gpu_machine_with_free_slots_wins() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(false, 2, 0))
            .await;
        orchestrator
            .observe_peers(&[
                peer("gpu-box", "workshop-nuc", capable(true, 2, 0)),
                peer("slow-box", "old-laptop", capable(false, 2, 0)),
            ])
            .await;

        let work = Work::Inference {
            model: "llama3.2:3b".into(),
            prompt: "hello".into(),
            system: None,
            temperature: None,
        };

        let plan = orchestrator.plan(&work, &[]).await;
        assert_eq!(plan[0].node_id, "gpu-box", "the accelerator should win");
        assert!(plan[0].reason.contains("RTX 4060"));
        // Every capable node is offered, so the caller can fall through.
        assert_eq!(plan.len(), 3);
    }

    #[tokio::test]
    async fn a_busy_machine_loses_to_an_idle_one_despite_its_gpu() {
        // Free capacity dominates: a fast machine at its limit finishes later
        // than a slower idle one.
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(Capabilities::default())
            .await;
        orchestrator
            .observe_peers(&[
                peer("gpu-busy", "busy-gpu", capable(true, 1, 1)),
                peer("cpu-free", "idle-cpu", capable(false, 4, 0)),
            ])
            .await;

        let work = Work::Inference {
            model: "llama3.2:3b".into(),
            prompt: "hi".into(),
            system: None,
            temperature: None,
        };
        assert_eq!(orchestrator.plan(&work, &[]).await[0].node_id, "cpu-free");
    }

    #[tokio::test]
    async fn work_is_only_placed_where_it_can_actually_run() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(Capabilities::default())
            .await;
        orchestrator
            .observe_peers(&[peer("box", "box", capable(false, 2, 0))])
            .await;

        // A model nobody has.
        let missing = Work::Inference {
            model: "some-model-nobody-has".into(),
            prompt: "hi".into(),
            system: None,
            temperature: None,
        };
        assert!(orchestrator.plan(&missing, &[]).await.is_empty());

        // A GPU task on a machine with no accelerator.
        let gpu_task = Work::Task {
            name: "render".into(),
            parameters: HashMap::new(),
            needs_gpu: true,
        };
        assert!(orchestrator.plan(&gpu_task, &[]).await.is_empty());

        // The same task without the GPU requirement is placeable.
        let cpu_task = Work::Task {
            name: "render".into(),
            parameters: HashMap::new(),
            needs_gpu: false,
        };
        assert_eq!(orchestrator.plan(&cpu_task, &[]).await.len(), 1);
    }

    #[tokio::test]
    async fn interactive_work_overtakes_background_work() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(true, 4, 0))
            .await;

        let render = |n: &str| Work::Task {
            name: "render".into(),
            parameters: HashMap::from([("frame".to_string(), n.to_string())]),
            needs_gpu: false,
        };

        orchestrator
            .submit(render("1"), Priority::Background)
            .await
            .unwrap();
        orchestrator
            .submit(render("2"), Priority::Normal)
            .await
            .unwrap();
        let urgent = orchestrator
            .submit(
                Work::Inference {
                    model: "llama3.2:3b".into(),
                    prompt: "answer me".into(),
                    system: None,
                    temperature: None,
                },
                Priority::Interactive,
            )
            .await
            .unwrap();

        let (first, _) = orchestrator
            .next_ready()
            .await
            .expect("something should be ready");
        assert_eq!(first.id, urgent, "the interactive job must go first");
    }

    #[tokio::test]
    async fn a_refused_job_fails_with_the_reason_once_nowhere_is_left() {
        // One capable node, which then refuses. Nowhere else can take the work,
        // so the job must fail rather than return to a queue that will never
        // drain — and it must fail carrying the refusal, because "not paired"
        // is the whole answer and the alternative shows as "queued" forever.
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(true, 1, 0))
            .await;

        let id = orchestrator
            .submit(
                Work::Inference {
                    model: "llama3.2:3b".into(),
                    prompt: "hi".into(),
                    system: None,
                    temperature: None,
                },
                Priority::Normal,
            )
            .await
            .unwrap();

        orchestrator
            .requeue(&id, "me", "this device has not been paired".into())
            .await;

        let job = orchestrator.job(&id).await.unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(
            job.error.as_deref(),
            Some("this device has not been paired")
        );
        assert!(orchestrator.next_ready().await.is_none());
    }

    #[tokio::test]
    async fn a_refused_job_moves_to_another_device() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(true, 1, 0))
            .await;
        orchestrator
            .observe_peers(&[peer("other", "other", capable(true, 1, 0))])
            .await;

        let id = orchestrator
            .submit(
                Work::Inference {
                    model: "llama3.2:3b".into(),
                    prompt: "hi".into(),
                    system: None,
                    temperature: None,
                },
                Priority::Normal,
            )
            .await
            .unwrap();

        orchestrator.requeue(&id, "other", "busy".into()).await;

        // Still live, and the next placement avoids the device that refused.
        let job = orchestrator.job(&id).await.unwrap();
        assert_eq!(job.state, JobState::Queued);
        let (_, placement) = orchestrator.next_ready().await.expect("placed elsewhere");
        assert_eq!(placement.node_id, "me");
    }

    #[tokio::test]
    async fn switching_the_orchestrator_off_stops_new_work() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(true, 2, 0))
            .await;

        orchestrator.set_enabled(false).await;
        assert!(orchestrator
            .submit(
                Work::Task {
                    name: "render".into(),
                    parameters: HashMap::new(),
                    needs_gpu: false
                },
                Priority::Normal
            )
            .await
            .is_err());
        assert!(orchestrator.next_ready().await.is_none());

        orchestrator.set_enabled(true).await;
        assert!(orchestrator
            .submit(
                Work::Task {
                    name: "render".into(),
                    parameters: HashMap::new(),
                    needs_gpu: false
                },
                Priority::Normal
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn capacity_pools_the_whole_network() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(capable(true, 2, 0))
            .await;
        orchestrator
            .observe_peers(&[
                peer("a", "a", capable(true, 2, 1)),
                peer("b", "b", capable(false, 2, 0)),
            ])
            .await;

        let capacity = orchestrator.capacity().await;
        assert_eq!(capacity.nodes, 3, "this node counts too");
        assert_eq!(capacity.accelerators, 2);
        assert_eq!(capacity.total_vram_mb, 16384);
        assert_eq!(capacity.cpu_threads, 24);
        assert_eq!(capacity.running_jobs, 1);
        assert_eq!(capacity.free_slots, 5);
        assert_eq!(capacity.models, vec!["llama3.2:3b"]);
        assert_eq!(capacity.tasks, vec!["render"]);
    }

    #[tokio::test]
    async fn offline_and_non_sharing_peers_are_not_scheduled_onto() {
        let orchestrator = Orchestrator::new("me".into(), true);
        orchestrator
            .set_local_capabilities(Capabilities::default())
            .await;

        let mut offline = peer("gone", "gone", capable(true, 2, 0));
        offline.online = false;
        let mut declined = peer("declined", "declined", capable(true, 2, 0));
        declined.compute.enabled = false;

        orchestrator.observe_peers(&[offline, declined]).await;
        assert_eq!(orchestrator.capacity().await.nodes, 0);
    }
}
