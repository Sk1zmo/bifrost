//! The compute service: answering for this node, and asking of others.
//!
//! One TCP connection carries one request and however many response frames it
//! produces, so a long job streams its output back as it goes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use crate::compute::jobs::{JobEvent, TaskRegistry};
use crate::compute::ollama::Ollama;
use crate::compute::pairing::TrustStore;
use crate::compute::protocol::*;
use crate::events::{EventBus, NodeEvent};
use crate::net::frame::{read_frame, write_frame};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs work on behalf of others, and asks others to run work.
#[derive(Clone)]
pub struct ComputeService {
    node_id: String,
    display_name: Arc<RwLock<String>>,
    pub trust: TrustStore,
    pub tasks: TaskRegistry,
    pub ollama: Ollama,
    events: EventBus,
    /// Jobs running here on behalf of peers.
    running: Arc<RwLock<Vec<JobStatus>>>,
    max_concurrent: Arc<RwLock<usize>>,
    enabled: Arc<RwLock<bool>>,
}

impl std::fmt::Debug for ComputeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputeService")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl ComputeService {
    pub fn new(
        node_id: String,
        display_name: String,
        require_pairing: bool,
        max_concurrent: usize,
        enabled: bool,
        events: EventBus,
    ) -> Self {
        Self {
            node_id,
            display_name: Arc::new(RwLock::new(display_name)),
            trust: TrustStore::new(require_pairing),
            tasks: TaskRegistry::new(),
            ollama: Ollama::new(),
            events,
            running: Arc::new(RwLock::new(Vec::new())),
            max_concurrent: Arc::new(RwLock::new(max_concurrent.max(1))),
            enabled: Arc::new(RwLock::new(enabled)),
        }
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.read().await
    }

    pub async fn set_enabled(&self, enabled: bool) {
        *self.enabled.write().await = enabled;
    }

    pub async fn set_display_name(&self, name: String) {
        *self.display_name.write().await = name;
    }

    /// What this node currently offers, for the beacon.
    pub async fn capabilities(&self, cpu_threads: usize, ram_mb: u64) -> Capabilities {
        let enabled = self.is_enabled().await;
        if !enabled {
            return Capabilities {
                enabled: false,
                ..Capabilities::default()
            };
        }

        Capabilities {
            enabled: true,
            gpus: crate::resources::hardware::detect_gpus(),
            models: self.ollama.models().await,
            tasks: self.tasks.describe().await,
            cpu_threads,
            ram_mb,
            running_jobs: self.running.read().await.len(),
            max_concurrent_jobs: *self.max_concurrent.read().await,
            requires_pairing: self.trust.requires_pairing().await,
        }
    }

    pub async fn running_jobs(&self) -> Vec<JobStatus> {
        self.running.read().await.clone()
    }

    async fn has_capacity(&self) -> bool {
        self.running.read().await.len() < *self.max_concurrent.read().await
    }

    // ── Serving ──────────────────────────────────────────────────────────────

    /// Accept compute requests until `shutdown` fires.
    pub async fn serve(
        &self,
        bind: SocketAddr,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("failed to bind the compute port {bind}"))?;
        tracing::info!(%bind, "compute service listening");

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, from)) => {
                            let service = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = service.handle(stream).await {
                                    tracing::debug!(%from, error = %e, "compute request ended");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "compute accept failed");
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("compute service shutting down");
                    return Ok(());
                }
            }
        }
    }

    async fn handle(&self, mut stream: TcpStream) -> Result<()> {
        let _ = stream.set_nodelay(true);

        let Some(request) = read_frame::<_, ComputeRequest>(&mut stream).await? else {
            return Ok(());
        };

        // Capability queries are open: a peer has to be able to see what a node
        // offers before it can sensibly ask to be paired with it.
        match request {
            ComputeRequest::Describe => {
                let capabilities = self.capabilities(0, 0).await;
                write_frame(
                    &mut stream,
                    &ComputeResponse::Capabilities(Box::new(capabilities)),
                )
                .await?;
            }

            ComputeRequest::ListJobs => {
                let jobs = self.running_jobs().await;
                write_frame(&mut stream, &ComputeResponse::Jobs { jobs }).await?;
            }

            ComputeRequest::Pair(request) => {
                let response = match self
                    .trust
                    .complete_pairing(&request.node_id, &request.display_name, &request.code)
                    .await
                {
                    Ok(token) => {
                        self.events.publish(NodeEvent::success(format!(
                            "{} may now use this device's resources",
                            request.display_name
                        )));
                        ComputeResponse::Paired { token }
                    }
                    Err(e) => ComputeResponse::Refused {
                        reason: e.to_string(),
                    },
                };
                write_frame(&mut stream, &response).await?;
            }

            ComputeRequest::Infer(request) => {
                self.serve_inference(&mut stream, request).await?;
            }

            ComputeRequest::RunTask(request) => {
                self.serve_task(&mut stream, request).await?;
            }

            ComputeRequest::Cancel { job_id } => {
                // Cancellation is best-effort: the process is left to finish,
                // but it stops being reported as ours.
                self.running.write().await.retain(|j| j.job_id != job_id);
                write_frame(
                    &mut stream,
                    &ComputeResponse::Finished(JobResult {
                        job_id,
                        success: true,
                        exit_code: None,
                        error: None,
                        duration_ms: 0,
                        tokens: None,
                    }),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Common checks before any work is accepted.
    async fn admit(&self, requester: &str, token: Option<&str>) -> Result<(), String> {
        if !self.is_enabled().await {
            return Err("this device is not sharing its resources".into());
        }
        if !self.has_capacity().await {
            return Err("this device is already at its job limit".into());
        }
        // A machine does not pair with itself. The orchestrator places work
        // locally whenever nothing better is free — and on a single machine
        // with a GPU and a model, that is *every* job — so without this, the
        // ordinary one-computer case fails with "not paired with that one",
        // naming the device you are sitting at.
        if requester == self.node_id {
            return Ok(());
        }
        self.trust
            .authorise(requester, token)
            .await
            .map_err(|e| e.to_string())
    }

    async fn serve_inference(&self, stream: &mut TcpStream, request: InferRequest) -> Result<()> {
        let requester = if request.requester.is_empty() {
            "anonymous".to_string()
        } else {
            request.requester.clone()
        };

        if let Err(reason) = self.admit(&requester, request.token.as_deref()).await {
            write_frame(stream, &ComputeResponse::Refused { reason }).await?;
            return Ok(());
        }

        let job_id = Uuid::new_v4().to_string();
        let started = Instant::now();

        self.begin_job(&job_id, &request.model, &requester).await;
        write_frame(
            stream,
            &ComputeResponse::Started {
                job_id: job_id.clone(),
                started_at: Utc::now(),
            },
        )
        .await?;

        let (tx, mut rx) = mpsc::channel::<String>(64);
        let ollama = self.ollama.clone();
        let model = request.model.clone();
        let generation = tokio::spawn(async move {
            ollama
                .generate(
                    &model,
                    &request.prompt,
                    request.system.as_deref(),
                    request.temperature,
                    tx,
                )
                .await
        });

        while let Some(text) = rx.recv().await {
            write_frame(
                stream,
                &ComputeResponse::Output {
                    job_id: job_id.clone(),
                    text,
                },
            )
            .await?;
        }

        let (success, error, tokens) = match generation.await {
            Ok(Ok(tokens)) => (true, None, Some(tokens)),
            Ok(Err(e)) => (false, Some(format!("{e:#}")), None),
            Err(e) => (false, Some(e.to_string()), None),
        };

        self.end_job(&job_id).await;
        write_frame(
            stream,
            &ComputeResponse::Finished(JobResult {
                job_id,
                success,
                exit_code: None,
                error,
                duration_ms: started.elapsed().as_millis() as u64,
                tokens,
            }),
        )
        .await?;

        Ok(())
    }

    async fn serve_task(&self, stream: &mut TcpStream, request: TaskRequest) -> Result<()> {
        let requester = if request.requester.is_empty() {
            "anonymous".to_string()
        } else {
            request.requester.clone()
        };

        if let Err(reason) = self.admit(&requester, request.token.as_deref()).await {
            write_frame(stream, &ComputeResponse::Refused { reason }).await?;
            return Ok(());
        }

        let job_id = Uuid::new_v4().to_string();
        let started = Instant::now();

        self.begin_job(&job_id, &request.task, &requester).await;
        write_frame(
            stream,
            &ComputeResponse::Started {
                job_id: job_id.clone(),
                started_at: Utc::now(),
            },
        )
        .await?;

        let (tx, mut rx) = mpsc::channel::<JobEvent>(64);
        let tasks = self.tasks.clone();
        let name = request.task.clone();
        let parameters = request.parameters.clone();
        let execution = tokio::spawn(async move { tasks.run(&name, &parameters, tx).await });

        let mut exit_code = None;
        let mut error = None;

        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Output(line) => {
                    write_frame(
                        stream,
                        &ComputeResponse::Output {
                            job_id: job_id.clone(),
                            text: format!("{line}\n"),
                        },
                    )
                    .await?;
                }
                JobEvent::Finished {
                    exit_code: code,
                    error: why,
                } => {
                    exit_code = code;
                    error = why;
                }
            }
        }

        // A task that could not even be started reports through the join.
        if let Ok(Err(e)) = execution.await {
            error = Some(format!("{e:#}"));
        }

        self.end_job(&job_id).await;
        write_frame(
            stream,
            &ComputeResponse::Finished(JobResult {
                job_id,
                success: error.is_none(),
                exit_code,
                error,
                duration_ms: started.elapsed().as_millis() as u64,
                tokens: None,
            }),
        )
        .await?;

        Ok(())
    }

    async fn begin_job(&self, job_id: &str, kind: &str, requester: &str) {
        let status = JobStatus {
            job_id: job_id.to_string(),
            kind: kind.to_string(),
            requester: requester.to_string(),
            worker: self.node_id.clone(),
            state: JobState::Running,
            started_at: Utc::now(),
            finished_at: None,
            output: String::new(),
            error: None,
        };
        self.running.write().await.push(status);
        self.events
            .publish(NodeEvent::info(format!("Running \"{kind}\" for a peer")));
    }

    async fn end_job(&self, job_id: &str) {
        self.running.write().await.retain(|j| j.job_id != job_id);
    }

    // ── Asking others ────────────────────────────────────────────────────────

    /// Ask a peer what it can do.
    pub async fn describe_peer(&self, addr: SocketAddr) -> Result<Capabilities> {
        let mut stream = connect(addr).await?;
        write_frame(&mut stream, &ComputeRequest::Describe).await?;

        match read_frame::<_, ComputeResponse>(&mut stream).await? {
            Some(ComputeResponse::Capabilities(capabilities)) => Ok(*capabilities),
            Some(ComputeResponse::Refused { reason }) => anyhow::bail!("{reason}"),
            _ => anyhow::bail!("unexpected reply"),
        }
    }

    /// Pair with a peer, using the code shown on its screen.
    pub async fn pair_with(&self, addr: SocketAddr, code: &str) -> Result<String> {
        let mut stream = connect(addr).await?;
        write_frame(
            &mut stream,
            &ComputeRequest::Pair(PairRequest {
                node_id: self.node_id.clone(),
                display_name: self.display_name.read().await.clone(),
                code: code.trim().to_string(),
            }),
        )
        .await?;

        match read_frame::<_, ComputeResponse>(&mut stream).await? {
            Some(ComputeResponse::Paired { token }) => Ok(token),
            Some(ComputeResponse::Refused { reason }) => anyhow::bail!("{reason}"),
            _ => anyhow::bail!("unexpected reply"),
        }
    }

    /// Send a request to a peer and stream its output back through `output`.
    ///
    /// Returns the final result.
    pub async fn dispatch(
        &self,
        addr: SocketAddr,
        request: ComputeRequest,
        output: mpsc::Sender<String>,
    ) -> Result<JobResult> {
        let mut stream = connect(addr).await?;
        write_frame(&mut stream, &request).await?;

        loop {
            match read_frame::<_, ComputeResponse>(&mut stream).await? {
                Some(ComputeResponse::Started { .. }) => continue,
                Some(ComputeResponse::Output { text, .. }) => {
                    if output.send(text).await.is_err() {
                        anyhow::bail!("the caller stopped listening");
                    }
                }
                Some(ComputeResponse::Finished(result)) => return Ok(result),
                Some(ComputeResponse::Refused { reason }) => anyhow::bail!("{reason}"),
                Some(other) => anyhow::bail!("unexpected reply: {other:?}"),
                None => anyhow::bail!("the peer closed the connection early"),
            }
        }
    }
}

async fn connect(addr: SocketAddr) -> Result<TcpStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("could not reach the compute service at {addr}"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A running service, plus the shutdown sender the caller must hold.
    ///
    /// Dropping the sender closes the channel, which the serve loop reads as
    /// "shut down now" — so it has to outlive the test, not the helper.
    async fn service(
        enabled: bool,
        require_pairing: bool,
    ) -> (
        ComputeService,
        SocketAddr,
        tokio::sync::broadcast::Sender<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let service = ComputeService::new(
            "worker".into(),
            "worker".into(),
            require_pairing,
            2,
            enabled,
            EventBus::new(),
        );

        let serving = service.clone();
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        tokio::spawn(async move {
            let _ = serving.serve(addr, rx).await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        (service, addr, shutdown)
    }

    #[tokio::test]
    async fn describe_is_open_so_a_peer_can_see_what_is_offered() {
        // Capabilities have to be readable before pairing, or a user could
        // never tell which machine to ask for a code.
        let (service, addr, _shutdown) = service(true, true).await;
        service
            .tasks
            .set(
                HashMap::from([(
                    "render".to_string(),
                    crate::compute::TaskDefinition {
                        command: "echo".into(),
                        args: vec!["{frame}".into()],
                        description: "Render".into(),
                        working_dir: None,
                        needs_gpu: false,
                        timeout_secs: 30,
                    },
                )]),
                None,
            )
            .await;

        let capabilities = service.describe_peer(addr).await.unwrap();
        assert!(capabilities.enabled);
        assert!(capabilities.requires_pairing);
        assert_eq!(capabilities.tasks.len(), 1);
        assert_eq!(capabilities.tasks[0].name, "render");
    }

    #[tokio::test]
    async fn work_from_an_unpaired_peer_is_refused() {
        let (_service, addr, _shutdown) = service(true, true).await;
        let client = ComputeService::new(
            "asker".into(),
            "asker".into(),
            false,
            2,
            true,
            EventBus::new(),
        );

        let (tx, _rx) = mpsc::channel::<String>(8);
        let error = client
            .dispatch(
                addr,
                ComputeRequest::RunTask(TaskRequest {
                    requester: "asker".into(),
                    token: None,
                    task: "render".into(),
                    parameters: HashMap::new(),
                }),
                tx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("paired"), "got: {error}");
    }

    #[tokio::test]
    async fn a_node_does_not_have_to_pair_with_itself() {
        // The orchestrator places work locally whenever nothing better is
        // free, and on a single machine that is every job. Requiring the node
        // to pair with itself made the ordinary one-computer case fail with
        // "this device has not been paired with that one", naming the machine
        // you are sitting at.
        let (_service, addr, _shutdown) = service(true, true).await;

        let itself = ComputeService::new(
            "worker".into(),
            "worker".into(),
            false,
            2,
            true,
            EventBus::new(),
        );

        let (tx, _rx) = mpsc::channel::<String>(8);
        let response = itself
            .dispatch(
                addr,
                ComputeRequest::RunTask(TaskRequest {
                    requester: "worker".into(),
                    token: None,
                    task: "render".into(),
                    parameters: HashMap::new(),
                }),
                tx,
            )
            .await;

        // It may fail for want of the program; it must not fail for pairing.
        if let Err(e) = &response {
            let text = e.to_string();
            assert!(!text.contains("paired"), "refused its own work: {text}");
        }
    }

    #[tokio::test]
    async fn a_paired_peer_can_run_a_declared_task_and_gets_its_output() {
        let (service, addr, _shutdown) = service(true, true).await;

        let (command, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec![
                    "/C".to_string(),
                    "echo".to_string(),
                    "{message}".to_string(),
                ],
            )
        } else {
            ("echo".to_string(), vec!["{message}".to_string()])
        };
        service
            .tasks
            .set(
                HashMap::from([(
                    "say".to_string(),
                    crate::compute::TaskDefinition {
                        command,
                        args,
                        description: "Say".into(),
                        working_dir: None,
                        needs_gpu: false,
                        timeout_secs: 30,
                    },
                )]),
                None,
            )
            .await;

        // Pair, as a user reading the code off the other machine would.
        let code = service.trust.begin_pairing().await;
        let client = ComputeService::new(
            "asker".into(),
            "asker".into(),
            false,
            2,
            true,
            EventBus::new(),
        );
        let token = client.pair_with(addr, &code).await.unwrap();

        let (tx, mut rx) = mpsc::channel::<String>(32);
        let collector = tokio::spawn(async move {
            let mut collected = String::new();
            while let Some(text) = rx.recv().await {
                collected.push_str(&text);
            }
            collected
        });

        let result = client
            .dispatch(
                addr,
                ComputeRequest::RunTask(TaskRequest {
                    requester: "asker".into(),
                    token: Some(token),
                    task: "say".into(),
                    parameters: HashMap::from([(
                        "message".to_string(),
                        "work ran on another machine".to_string(),
                    )]),
                }),
                tx,
            )
            .await
            .unwrap();

        assert!(result.success, "job failed: {:?}", result.error);
        assert_eq!(result.exit_code, Some(0));
        assert!(collector
            .await
            .unwrap()
            .contains("work ran on another machine"));
    }

    #[tokio::test]
    async fn a_node_with_sharing_off_refuses_everything() {
        let (service, addr, _shutdown) = service(false, false).await;
        service.trust.set_require_pairing(false).await;

        let client = ComputeService::new(
            "asker".into(),
            "asker".into(),
            false,
            2,
            true,
            EventBus::new(),
        );
        let (tx, _rx) = mpsc::channel::<String>(8);

        let error = client
            .dispatch(
                addr,
                ComputeRequest::RunTask(TaskRequest {
                    requester: "asker".into(),
                    token: None,
                    task: "anything".into(),
                    parameters: HashMap::new(),
                }),
                tx,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("not sharing"), "got: {error}");
    }

    #[tokio::test]
    async fn an_undeclared_task_fails_even_for_a_paired_peer() {
        // Pairing grants access to what is declared, not to the machine.
        let (service, addr, _shutdown) = service(true, true).await;
        let code = service.trust.begin_pairing().await;

        let client = ComputeService::new(
            "asker".into(),
            "asker".into(),
            false,
            2,
            true,
            EventBus::new(),
        );
        let token = client.pair_with(addr, &code).await.unwrap();

        let (tx, _rx) = mpsc::channel::<String>(8);
        let result = client
            .dispatch(
                addr,
                ComputeRequest::RunTask(TaskRequest {
                    requester: "asker".into(),
                    token: Some(token),
                    task: "rm".into(),
                    parameters: HashMap::new(),
                }),
                tx,
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("does not offer"));
    }
}
