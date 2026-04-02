//! Konveyor ProviderService gRPC implementation.

use crate::proto::provider_service_server::ProviderService;
use crate::proto::*;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// The frontend analyzer provider.
pub struct FrontendProvider {
    pub config: Arc<Mutex<Option<Config>>>,
    pub project_root: Arc<Mutex<Option<PathBuf>>>,
    /// Number of context lines to include around code snippets.
    pub context_lines: usize,
}

impl FrontendProvider {
    pub fn new(context_lines: usize) -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            project_root: Arc::new(Mutex::new(None)),
            context_lines,
        }
    }
}

type ProgressStream = Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

#[tonic::async_trait]
impl ProviderService for FrontendProvider {
    async fn capabilities(
        &self,
        _request: Request<()>,
    ) -> Result<Response<CapabilitiesResponse>, Status> {
        let capabilities = vec![
            Capability {
                name: "referenced".into(),
                template_context: None,
            },
            Capability {
                name: "cssclass".into(),
                template_context: None,
            },
            Capability {
                name: "cssvar".into(),
                template_context: None,
            },
            Capability {
                name: "dependency".into(),
                template_context: None,
            },
        ];

        Ok(Response::new(CapabilitiesResponse { capabilities }))
    }

    async fn init(&self, request: Request<Config>) -> Result<Response<InitResponse>, Status> {
        let config = request.into_inner();
        let location = config.location.clone();

        tracing::info!("Initializing frontend provider with location: {}", location);

        let root = PathBuf::from(&location);
        if !root.exists() {
            return Ok(Response::new(InitResponse {
                error: format!("Location does not exist: {}", location),
                successful: false,
                id: 0,
                builtin_config: None,
            }));
        }

        // Install npm dependencies so that `npm ls` can resolve the full
        // dependency tree for the `GetDependencies` RPC. Without this,
        // dependency rules (e.g., "update @patternfly/react-core to v6")
        // cannot match because the resolved versions are unknown.
        let pkg_json = root.join("package.json");
        if pkg_json.exists() {
            tracing::info!("Running npm install in {}", location);
            let output = std::process::Command::new("npm")
                .arg("install")
                .arg("--ignore-scripts")
                .arg("--no-audit")
                .arg("--no-fund")
                .current_dir(&root)
                .output();

            match output {
                Ok(result) => {
                    if result.status.success() {
                        tracing::info!("npm install completed successfully");
                    } else {
                        let stderr = String::from_utf8_lossy(&result.stderr);
                        tracing::warn!("npm install failed (non-fatal): {}", stderr.chars().take(500).collect::<String>());
                    }
                }
                Err(e) => {
                    tracing::warn!("npm install could not run (non-fatal): {}", e);
                }
            }
        }

        *self
            .config
            .lock()
            .map_err(|_| Status::internal("Config lock poisoned"))? = Some(config);
        *self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))? = Some(root);

        Ok(Response::new(InitResponse {
            error: String::new(),
            successful: true,
            id: 1,
            builtin_config: None,
        }))
    }

    async fn evaluate(
        &self,
        request: Request<EvaluateRequest>,
    ) -> Result<Response<EvaluateResponse>, Status> {
        let req = request.into_inner();
        let root = self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))?
            .clone()
            .ok_or_else(|| Status::failed_precondition("Provider not initialized"))?;

        tracing::info!(
            "Evaluate request: cap={}, condition_info={}",
            &req.cap,
            &req.condition_info
        );
        match crate::evaluate::evaluate_condition(&root, &req.cap, &req.condition_info) {
            Ok(result) => {
                // Build an error summary if any files could not be parsed.
                let error = if result.parse_errors.is_empty() {
                    String::new()
                } else {
                    let file_list: Vec<String> = result
                        .parse_errors
                        .iter()
                        .map(|e| format!("{}: {}", e.file_path.display(), e.message))
                        .collect();
                    format!(
                        "{} file(s) could not be parsed and were skipped:\n{}",
                        result.parse_errors.len(),
                        file_list.join("\n")
                    )
                };

                Ok(Response::new(EvaluateResponse {
                    error,
                    successful: true,
                    response: Some(result.response),
                }))
            }
            Err(e) => Ok(Response::new(EvaluateResponse {
                error: e.to_string(),
                successful: false,
                response: None,
            })),
        }
    }

    async fn stop(&self, _request: Request<ServiceRequest>) -> Result<Response<()>, Status> {
        tracing::info!("Frontend provider stopping");
        Ok(Response::new(()))
    }

    async fn get_dependencies(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<DependencyResponse>, Status> {
        let root = self
            .project_root
            .lock()
            .map_err(|_| Status::internal("Project root lock poisoned"))?
            .clone()
            .ok_or_else(|| Status::failed_precondition("Provider not initialized"))?;

        let file_uri = format!("file://{}", root.join("package.json").display());

        // Use `npm ls --json --depth=0` to get resolved dependency versions.
        // This requires `npm install` to have run first (done in init).
        let output = std::process::Command::new("npm")
            .args(["ls", "--json", "--depth=0", "--long=false"])
            .current_dir(&root)
            .output();

        let deps = match output {
            Ok(result) => {
                let stdout = String::from_utf8_lossy(&result.stdout);
                match serde_json::from_str::<serde_json::Value>(&stdout) {
                    Ok(tree) => {
                        let mut deps = Vec::new();
                        if let Some(dep_map) = tree.get("dependencies").and_then(|d| d.as_object())
                        {
                            for (name, info) in dep_map {
                                let version = info
                                    .get("version")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("0.0.0")
                                    .to_string();
                                deps.push(Dependency {
                                    name: name.clone(),
                                    version,
                                    classifier: String::new(),
                                    r#type: "dependencies".to_string(),
                                    resolved_identifier: String::new(),
                                    file_uri_prefix: String::new(),
                                    indirect: false,
                                    extras: None,
                                    labels: vec![],
                                });
                            }
                        }
                        deps
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse npm ls output: {}", e);
                        vec![]
                    }
                }
            }
            Err(e) => {
                tracing::warn!("npm ls failed: {}", e);
                vec![]
            }
        };

        tracing::info!("Returning {} resolved dependencies", deps.len());

        let file_dep = vec![FileDep {
            file_uri,
            list: Some(DependencyList { deps }),
        }];

        Ok(Response::new(DependencyResponse {
            successful: true,
            error: String::new(),
            file_dep,
        }))
    }

    async fn get_dependencies_dag(
        &self,
        _request: Request<ServiceRequest>,
    ) -> Result<Response<DependencyDagResponse>, Status> {
        Ok(Response::new(DependencyDagResponse {
            successful: true,
            error: String::new(),
            file_dag_dep: vec![],
        }))
    }

    async fn notify_file_changes(
        &self,
        _request: Request<NotifyFileChangesRequest>,
    ) -> Result<Response<NotifyFileChangesResponse>, Status> {
        Ok(Response::new(NotifyFileChangesResponse {
            error: String::new(),
        }))
    }

    async fn prepare(
        &self,
        _request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        Ok(Response::new(PrepareResponse {
            error: String::new(),
        }))
    }

    type StreamPrepareProgressStream = ProgressStream;

    async fn stream_prepare_progress(
        &self,
        _request: Request<PrepareProgressRequest>,
    ) -> Result<Response<Self::StreamPrepareProgressStream>, Status> {
        let stream = async_stream::stream! {
            yield Ok(ProgressEvent {
                r#type: 0,
                provider_name: "frontend".into(),
                files_processed: 0,
                total_files: 0,
            });
        };
        Ok(Response::new(Box::pin(stream)))
    }
}
