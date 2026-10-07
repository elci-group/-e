// SPDX-License-Identifier: MIT
//! ELCI inference provider. One envelope in, one worker result out.
//! Provider identity, the contract and the model stay in separate fields.
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use colony_core::{Artifact, Budget, Error, FailureKind, Result, WorkerResult};
use colony_provider::{InferenceEnvelope, InferenceProvider, Resource};
use mesut::CancellationToken;

/// Where an ELCI model call actually goes.
#[derive(Debug, Clone)]
pub enum ElciEndpoint {
    /// Local process. The host chooses the program; the work unit cannot.
    Command { program: PathBuf, args: Vec<String> },
    /// OpenAI-compatible `POST /v1/chat/completions` on cleartext HTTP.
    Http {
        base: String,
        api_key: Option<String>,
    },
}

/// Runs one assigned model call and writes the contract's expected outputs.
pub struct ElciProvider {
    endpoint: ElciEndpoint,
    resource: Resource,
    root: PathBuf,
    work_dir: PathBuf,
    cancel: CancellationToken,
}

impl ElciProvider {
    pub fn new(
        endpoint: ElciEndpoint,
        resource: Resource,
        root: impl Into<PathBuf>,
        work_dir: impl Into<PathBuf>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            endpoint,
            resource,
            root: root.into(),
            work_dir: work_dir.into(),
            cancel,
        }
    }
}

impl InferenceProvider for ElciProvider {
    fn resource(&self) -> &Resource {
        &self.resource
    }

    fn infer(&mut self, envelope: InferenceEnvelope<'_>) -> Result<WorkerResult> {
        if self.cancel.is_cancelled() {
            return Err(Error::new(
                FailureKind::Cancelled,
                "inference cancelled before it started",
            ));
        }
        fs::create_dir_all(&self.work_dir).map_err(|error| io_err("create attempt dir", error))?;
        let snapshot = self.work_dir.join("snapshot");
        fs::create_dir_all(&snapshot).map_err(|error| io_err("create snapshot", error))?;
        for rel in &envelope.contract.context.relevant_paths {
            copy_one(&self.root, rel, &snapshot)?;
        }
        write_envelope(&self.work_dir, &envelope)?;
        match &self.endpoint {
            ElciEndpoint::Command { program, args } => {
                run_command(
                    program,
                    args,
                    &envelope,
                    &self.work_dir,
                    &snapshot,
                    &self.cancel,
                )?;
            }
            ElciEndpoint::Http { base, api_key } => {
                let body = http_chat(base, api_key.as_deref(), &envelope, &self.cancel)?;
                let text = message_body(&body);
                write_model_body(
                    &self.work_dir,
                    envelope.contract.expected_outputs.as_slice(),
                    text,
                )?;
            }
        }
        let mut artifacts = Vec::new();
        let mut output_bytes = 0u64;
        for name in &envelope.contract.expected_outputs {
            let path = self.work_dir.join(name);
            let bytes = fs::read(&path).map_err(|_| {
                Error::new(
                    FailureKind::InvalidArtifact,
                    format!("ELCI provider did not write {name}"),
                )
            })?;
            if bytes.is_empty() {
                return Err(Error::new(
                    FailureKind::InvalidArtifact,
                    format!("ELCI provider wrote an empty {name}"),
                ));
            }
            output_bytes = output_bytes.saturating_add(bytes.len() as u64);
            artifacts.push(Artifact {
                name: name.clone(),
                reference: path.display().to_string(),
                source_sha: envelope.contract.context.source_sha.clone(),
                result_sha: None,
                changed_paths: Vec::new(),
            });
        }
        let output_tokens = output_bytes.saturating_add(3) / 4;
        let tokens = envelope
            .contract
            .context
            .input_tokens
            .checked_add(output_tokens)
            .ok_or_else(|| Error::new(FailureKind::BudgetExhausted, "token usage overflow"))?;
        let usage = Budget {
            money_micros: self.resource.cost(envelope.contract)?,
            tokens,
            calls: 1,
        };
        if !usage.fits(envelope.contract.budget) {
            return Err(Error::new(
                FailureKind::BudgetExhausted,
                "ELCI usage exceeds the unit reservation",
            ));
        }
        Ok(WorkerResult {
            work_unit: envelope.contract.id.clone(),
            provider: envelope.provider.to_string(),
            model: envelope.model.to_string(),
            artifacts,
            usage,
        })
    }
}

fn io_err(context: &str, error: std::io::Error) -> Error {
    Error::new(
        FailureKind::InfrastructureFailure,
        format!("{context}: {error}"),
    )
}

fn copy_one(root: &Path, rel: &str, snapshot: &Path) -> Result<()> {
    let src = root.join(rel);
    if !src.is_file() {
        return Ok(());
    }
    let dest = snapshot.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| io_err("create snapshot parent", error))?;
    }
    fs::copy(&src, &dest).map_err(|error| io_err("copy snapshot", error))?;
    Ok(())
}

fn write_envelope(dir: &Path, envelope: &InferenceEnvelope<'_>) -> Result<()> {
    let body = serde_json::json!({
        "contract_id": envelope.contract.id,
        "objective": envelope.contract.objective,
        "provider": envelope.provider,
        "model": envelope.model,
        "temperature": envelope.temperature,
        "source_sha": envelope.contract.context.source_sha,
        "relevant_paths": envelope.contract.context.relevant_paths,
        "expected_outputs": envelope.contract.expected_outputs,
    });
    fs::write(
        dir.join("colony-envelope.json"),
        serde_json::to_vec_pretty(&body).map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("encode envelope: {error}"),
            )
        })?,
    )
    .map_err(|error| io_err("write envelope", error))?;
    Ok(())
}

fn run_command(
    program: &Path,
    args: &[String],
    envelope: &InferenceEnvelope<'_>,
    work_dir: &Path,
    snapshot: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(work_dir)
        .env("COLONY_UNIT", &envelope.contract.id)
        .env("COLONY_OBJECTIVE", &envelope.contract.objective)
        .env("COLONY_PROVIDER", envelope.provider)
        .env("COLONY_MODEL", envelope.model)
        .env("COLONY_SOURCE_SHA", &envelope.contract.context.source_sha)
        .env(
            "COLONY_PATHS",
            envelope.contract.context.relevant_paths.join("\n"),
        )
        .env(
            "COLONY_OUTPUTS",
            envelope.contract.expected_outputs.join("\n"),
        )
        .env("COLONY_OUT", work_dir)
        .env("COLONY_SNAPSHOT", snapshot)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(work_dir.join("stderr.txt"))
                .map_err(|error| io_err("open stderr", error))?,
        ))
        .spawn()
        .map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("spawn {}: {error}", program.display()),
            )
        })?;
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(
                FailureKind::Cancelled,
                "mesut cancelled the ELCI process",
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let stderr = fs::read_to_string(work_dir.join("stderr.txt")).unwrap_or_default();
                let tail: String = stderr.chars().take(500).collect();
                return Err(Error::new(
                    FailureKind::InfrastructureFailure,
                    format!("ELCI process exited {status}: {tail}"),
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(io_err("wait for ELCI process", error)),
        }
    }
}

fn message_body(content: &str) -> &str {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest
        .strip_prefix("json")
        .unwrap_or(rest)
        .trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

fn write_model_body(dir: &Path, outputs: &[String], body: &str) -> Result<()> {
    if outputs.len() == 1 {
        fs::write(dir.join(&outputs[0]), body)
            .map_err(|error| io_err("write model output", error))?;
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        Error::new(
            FailureKind::InvalidArtifact,
            format!("ELCI response is not a JSON object of outputs: {error}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        Error::new(
            FailureKind::InvalidArtifact,
            "ELCI response must be a JSON object when a unit expects several outputs",
        )
    })?;
    for name in outputs {
        let encoded = serde_json::to_vec(object.get(name).ok_or_else(|| {
            Error::new(
                FailureKind::InvalidArtifact,
                format!("ELCI response is missing output {name}"),
            )
        })?)
        .map_err(|error| Error::new(FailureKind::InfrastructureFailure, error.to_string()))?;
        fs::write(dir.join(name), encoded).map_err(|error| io_err("write model output", error))?;
    }
    Ok(())
}

fn http_chat(
    base: &str,
    api_key: Option<&str>,
    envelope: &InferenceEnvelope<'_>,
    cancel: &CancellationToken,
) -> Result<String> {
    if cancel.is_cancelled() {
        return Err(Error::new(FailureKind::Cancelled, "inference cancelled"));
    }
    let (host, port, path) = split_http(base)?;
    let paths = envelope
        .contract
        .context
        .relevant_paths
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let payload = serde_json::json!({
        "model": envelope.model,
        "temperature": envelope.temperature,
        "messages": [
            {
                "role": "system",
                "content": "Return one JSON object and nothing else: {\"findings\":[{\"reference\":\"<one supplied path>\",\"detail\":\"<short observation>\"}]}. Use only the supplied paths. Do not claim the work is verified."
            },
            {
                "role": "user",
                "content": format!("{}\n\npaths:\n{paths}", envelope.contract.objective)
            }
        ]
    });
    let body = serde_json::to_vec(&payload).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("encode chat request: {error}"),
        )
    })?;
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(key) = api_key {
        request.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = TcpStream::connect((host.as_str(), port)).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("ELCI endpoint {host}:{port}: {error}"),
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| io_err("set read timeout", error))?;
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| io_err("write chat request", error))?;
    let mut response = Vec::new();
    stream
        .take(1_048_576)
        .read_to_end(&mut response)
        .map_err(|error| io_err("read chat response", error))?;
    let text = String::from_utf8_lossy(&response);
    let (head, raw_body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        Error::new(
            FailureKind::InfrastructureFailure,
            "ELCI response has no HTTP body",
        )
    })?;
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200 ") {
        return Err(Error::new(
            FailureKind::InfrastructureFailure,
            format!("ELCI HTTP status `{status}`"),
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(raw_body.trim()).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("ELCI response is not JSON: {error}"),
        )
    })?;
    parsed
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::new(
                FailureKind::InvalidArtifact,
                "ELCI response is missing choices[0].message.content",
            )
        })
}

fn split_http(base: &str) -> Result<(String, u16, String)> {
    let rest = base.strip_prefix("http://").ok_or_else(|| {
        Error::new(
            FailureKind::ContractViolation,
            "ELCI HTTP endpoints must be http://host:port",
        )
    })?;
    let (authority, raw_path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    if authority.is_empty() {
        return Err(Error::new(
            FailureKind::ContractViolation,
            "ELCI HTTP host is empty",
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().map_err(|_| {
                Error::new(
                    FailureKind::ContractViolation,
                    "ELCI HTTP port is not a number",
                )
            })?;
            (host.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    let mut path = raw_path.trim_end_matches('/').to_string();
    if path.is_empty() {
        path = "/v1/chat/completions".to_string();
    } else if !path.ends_with("/chat/completions") {
        path.push_str("/chat/completions");
    }
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use colony_core::*;
    use colony_provider::{Capabilities, InferenceEnvelope};
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn resource() -> Resource {
        Resource {
            provider: "example-local".into(),
            model: "research".into(),
            local: true,
            classifications: BTreeSet::from([Classification::Internal]),
            declared: Capabilities {
                reasoning: 0.5,
                coding: 0.5,
                research: 0.5,
                structured_output: 0.5,
                tool_use: 0.0,
            },
            benchmarked: None,
            observed: None,
            context_window: 8000,
            max_output: 2000,
            latency_ms: 10,
            concurrency: 1,
            available: true,
            remaining_calls: 10,
            input_micros_per_million: 0,
            output_micros_per_million: 0,
            shadow_price: 1.0,
        }
    }

    fn unit() -> WorkUnit {
        serde_json::from_str(
            r#"{
            "id": "dependency-audit",
            "objective": "Produce the dependency-audit report",
            "rationale": "bounded",
            "dependencies": [],
            "expected_outputs": ["dependency-audit.json"],
            "acceptance": ["Every finding has a source reference"],
            "context": {
                "source_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "relevant_paths": ["Cargo.toml"],
                "symbols": [],
                "ontology_entities": ["dependency-audit"],
                "architectural_constraints": [],
                "trusted_context": [],
                "retrieved_untrusted": [],
                "input_tokens": 10
            },
            "demand": {
                "reasoning": 0.2, "coding": 0.2, "research": 0.9,
                "structured_output": 0.8, "tool_use": 0.0, "output_tokens": 20
            },
            "classification": "INTERNAL",
            "mutation": null,
            "relational_density": 0.1,
            "uncertainty": 0.2,
            "consequence": 0.2,
            "budget": {"money_micros": 1000, "tokens": 1000, "calls": 1},
            "estimated_ms": 10,
            "verification": {"deterministic_checks": ["report-schema"], "semantic_review": false}
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn http_endpoint_returns_model_content_as_the_artifact() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Read the whole request before answering. Closing with unread
            // bytes resets the connection and flakes the client read.
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let headers = String::from_utf8_lossy(&buf[..header_end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if buf.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = r#"{"choices":[{"message":{"content":"{\"findings\":[{\"reference\":\"Cargo.toml\",\"detail\":\"observed\"}]}"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.shutdown(std::net::Shutdown::Write);
        });
        let root = std::env::temp_dir().join(format!("colony-elci-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "name = \"x\"\n").unwrap();
        let work = root.join("work");
        let mut provider = ElciProvider::new(
            ElciEndpoint::Http {
                base: format!("http://127.0.0.1:{port}/v1"),
                api_key: None,
            },
            resource(),
            &root,
            &work,
            CancellationToken::new(),
        );
        let contract = unit();
        let result = provider
            .infer(InferenceEnvelope {
                contract: &contract,
                temperature: 0.2,
                provider: "example-local",
                model: "research",
            })
            .unwrap();
        assert_eq!(result.provider, "example-local");
        assert_eq!(result.model, "research");
        let text = fs::read_to_string(work.join("dependency-audit.json")).unwrap();
        assert!(text.contains("Cargo.toml"));
        let _ = fs::remove_dir_all(&root);
    }
}
