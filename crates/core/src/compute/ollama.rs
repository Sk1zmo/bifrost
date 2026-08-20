//! Bridge to a local Ollama.
//!
//! Ollama already solves the hard parts of running a model — weights, GPU
//! offload, quantisation, memory — and exposes them over plain HTTP on
//! `127.0.0.1:11434`. Bifrost's job is only to find it, ask what it has, and
//! relay prompts to it from elsewhere on the network. Nothing here loads a
//! model or touches a GPU directly.
//!
//! The HTTP is written by hand rather than pulled from a client crate: these
//! are two request shapes against a fixed local address, and a full client
//! would be a large dependency and a TLS stack for something that never leaves
//! loopback.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::compute::protocol::ModelInfo;

/// Where Ollama listens by default.
pub const DEFAULT_HOST: &str = "127.0.0.1:11434";

/// How long to wait for Ollama to accept a connection.
///
/// Short: this only ever talks to loopback, and a node checking whether a
/// runtime exists must not stall its beacon for seconds when it does not.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);

/// How long to wait for the first byte of a generation.
///
/// Generous: a cold model can take a long while to load into VRAM.
const FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(180);

/// A local model runtime.
#[derive(Debug, Clone)]
pub struct Ollama {
    host: String,
}

impl Default for Ollama {
    fn default() -> Self {
        Self::new()
    }
}

impl Ollama {
    pub fn new() -> Self {
        // Honour Ollama's own environment variable, so a non-standard port
        // works without configuring Bifrost separately.
        let host = std::env::var("OLLAMA_HOST")
            .ok()
            .map(|h| {
                h.trim_start_matches("http://")
                    .trim_start_matches("https://")
                    .to_string()
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());

        Self { host }
    }

    /// Is a runtime answering?
    pub async fn is_available(&self) -> bool {
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.host))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }

    /// Models this machine can run right now.
    ///
    /// An unreachable runtime is not an error: most machines have none, and
    /// the caller advertises an empty list rather than failing to start.
    pub async fn models(&self) -> Vec<ModelInfo> {
        match self.query_models().await {
            Ok(models) => models,
            Err(e) => {
                tracing::debug!(error = %e, "no local model runtime");
                Vec::new()
            }
        }
    }

    async fn query_models(&self) -> Result<Vec<ModelInfo>> {
        let body = self.get("/api/tags").await?;

        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            #[serde(default)]
            size: u64,
            #[serde(default)]
            details: Option<Details>,
        }
        #[derive(Deserialize)]
        struct Details {
            #[serde(default)]
            parameter_size: Option<String>,
            #[serde(default)]
            quantization_level: Option<String>,
        }

        let tags: Tags = serde_json::from_str(&body).context("unexpected reply from Ollama")?;

        let mut models: Vec<ModelInfo> = tags
            .models
            .into_iter()
            .map(|entry| ModelInfo {
                name: entry.name,
                size_bytes: entry.size,
                parameters: entry
                    .details
                    .as_ref()
                    .and_then(|d| d.parameter_size.clone()),
                quantization: entry.details.and_then(|d| d.quantization_level),
            })
            .collect();

        models.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(models)
    }

    /// Run a prompt, streaming the answer.
    ///
    /// Each token is forwarded as it arrives, so a remote dashboard shows the
    /// answer appearing rather than waiting for the whole thing.
    pub async fn generate(
        &self,
        model: &str,
        prompt: &str,
        system: Option<&str>,
        temperature: Option<f32>,
        output: mpsc::Sender<String>,
    ) -> Result<u32> {
        let mut request = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "stream": true,
        });
        if let Some(system) = system {
            request["system"] = serde_json::Value::String(system.to_string());
        }
        if let Some(temperature) = temperature {
            request["options"] = serde_json::json!({ "temperature": temperature });
        }

        let payload = serde_json::to_vec(&request)?;
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.host))
            .await
            .context("no model runtime is reachable on this device")?
            .context("could not connect to the model runtime")?;

        let head = format!(
            "POST /api/generate HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.host,
            payload.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);

        // Skip the response head; the body is newline-delimited JSON.
        let mut status = String::new();
        tokio::time::timeout(FIRST_TOKEN_TIMEOUT, reader.read_line(&mut status))
            .await
            .context("the model runtime did not answer")??;
        if !status.contains("200") {
            bail!("the model runtime refused: {}", status.trim());
        }

        loop {
            let mut header = String::new();
            reader.read_line(&mut header).await?;
            if header.trim().is_empty() {
                break;
            }
        }

        let mut tokens = 0u32;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(FIRST_TOKEN_TIMEOUT, reader.read_line(&mut line))
                .await
                .context("the model stopped responding")??;
            if read == 0 {
                break;
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            #[derive(Deserialize)]
            struct Chunk {
                #[serde(default)]
                response: String,
                #[serde(default)]
                done: bool,
                #[serde(default)]
                error: Option<String>,
            }

            // Chunked transfer encoding interleaves size lines with JSON; a
            // line that does not parse is one of those, and is skipped.
            let Ok(chunk) = serde_json::from_str::<Chunk>(line) else {
                continue;
            };

            if let Some(error) = chunk.error {
                bail!("{error}");
            }
            if !chunk.response.is_empty() {
                tokens += 1;
                // A receiver that has gone away means the requester
                // disconnected; stop generating rather than talking to nobody.
                if output.send(chunk.response).await.is_err() {
                    break;
                }
            }
            if chunk.done {
                break;
            }
        }

        Ok(tokens)
    }

    /// A minimal GET against the local runtime.
    async fn get(&self, path: &str) -> Result<String> {
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.host))
            .await
            .context("the model runtime is not reachable")?
            .context("could not connect to the model runtime")?;

        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.host
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let mut raw = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut raw).await?;
        let text = String::from_utf8_lossy(&raw);

        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .context("malformed reply")?;

        // With `Connection: close` Ollama may still use chunked encoding;
        // find the JSON rather than trusting the framing.
        let start = body.find('{').context("no JSON in the reply")?;
        let end = body.rfind('}').context("no JSON in the reply")?;
        Ok(body[start..=end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_honours_ollamas_own_environment_variable() {
        // Set and cleared within the test; other tests must not see it.
        std::env::set_var("OLLAMA_HOST", "http://192.168.1.50:11434");
        assert_eq!(Ollama::new().host, "192.168.1.50:11434");

        std::env::set_var("OLLAMA_HOST", "");
        assert_eq!(Ollama::new().host, DEFAULT_HOST);

        std::env::remove_var("OLLAMA_HOST");
        assert_eq!(Ollama::new().host, DEFAULT_HOST);
    }

    #[tokio::test]
    async fn a_missing_runtime_reports_no_models_rather_than_failing() {
        // Nothing listens here; most machines have no Ollama and must still
        // start and advertise themselves normally.
        let ollama = Ollama {
            host: "127.0.0.1:1".to_string(),
        };

        assert!(!ollama.is_available().await);
        assert!(ollama.models().await.is_empty());
    }

    #[tokio::test]
    async fn generating_against_a_missing_runtime_reports_why() {
        let ollama = Ollama {
            host: "127.0.0.1:1".to_string(),
        };
        let (tx, _rx) = mpsc::channel(4);

        let error = ollama
            .generate("llama3.2:3b", "hello", None, None, tx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("runtime"),
            "the message should name the runtime, got: {error}"
        );
    }
}
