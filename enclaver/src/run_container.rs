use anyhow::{anyhow, Result};
use bollard::container::{Config, LogOutput, LogsOptions, WaitContainerOptions};
use bollard::models::{DeviceMapping, HostConfig, PortBinding, PortMap};
use bollard::Docker;
use futures_util::stream::{StreamExt, TryStreamExt};
use tokio_util::sync::CancellationToken;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

pub struct RunWrapper {
    docker: Arc<Docker>,
}

impl RunWrapper {
    pub fn new() -> Result<Self> {
        let docker = Arc::new(
            Docker::connect_with_local_defaults()
                .map_err(|e| anyhow!("connecting to docker: {}", e))?,
        );

        Ok(Self {
            docker,
        })
    }

    pub async fn run_enclaver_image(
        &self,
        image_name: &str,
        port_forwards: Vec<String>,
    ) -> Result<RunHandle> {
        let port_re = regex::Regex::new(r"(\d+):(\d+)")?;

        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        let mut port_bindings = PortMap::new();

        for spec in port_forwards {
            let captures = port_re.captures(&spec).ok_or_else(|| {
                anyhow!(
                    "port forward specification '{spec}' does not match the format 'host_port:container_port'",
                )
            })?;
            let host_port = captures.get(1).unwrap().as_str();
            let container_port = captures.get(2).unwrap().as_str();
            exposed_ports.insert(format!("{container_port}/tcp"), HashMap::new());

            port_bindings.insert(
                format!("{container_port}/tcp"),
                Some(vec![PortBinding {
                    host_port: Some(host_port.to_string()),
                    host_ip: None,
                }]),
            );
        }

        let container_id = self
            .docker
            .create_container::<String, String>(
                None,
                Config {
                    image: Some(image_name.to_string()),
                    cmd: Some(vec![]), // TODO(russell_h): pass through additional args
                    attach_stderr: Some(true),
                    attach_stdout: Some(true),
                    host_config: Some(HostConfig {
                        devices: Some(vec![DeviceMapping {
                            path_on_host: Some(String::from("/dev/nitro_enclaves")),
                            path_in_container: Some(String::from("/dev/nitro_enclaves")),
                            cgroup_permissions: Some(String::from("rwm")),
                        }]),
                        port_bindings: Some(port_bindings),
                        ..Default::default()
                    }),
                    exposed_ports: Some(exposed_ports),
                    ..Default::default()
                },
            )
            .await?
            .id;

        self.docker
            .start_container::<String>(&container_id, None)
            .await?;

        let stream_task = self.start_output_stream_task(container_id.clone()).await?;

        Ok(RunHandle {
            docker: self.docker.clone(),
            container_id,
            stream_task,
        })
    }

    async fn start_output_stream_task(&self, container_id: String) -> Result<tokio::task::JoinHandle<()>> {
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();

        let mut log_stream = self.docker.logs::<String>(
            &container_id,
            Some(LogsOptions {
                follow: true,
                stdout: true,
                stderr: true,
                ..Default::default()
            }),
        );

        let jh = tokio::task::spawn(async move {
            while let Some(Ok(item)) = log_stream.next().await {
                match item {
                    LogOutput::StdOut { message } => stdout.write_all(&message).await.unwrap(),
                    LogOutput::StdErr { message } => stderr.write_all(&message).await.unwrap(),
                    _ => {}
                }
            }
        });

        Ok(jh)
    }
}

pub struct RunHandle {
    container_id: String,
    stream_task: tokio::task::JoinHandle<()>,
    docker: Arc<Docker>,
}

impl RunHandle {
    pub async fn wait(self, cancel: CancellationToken) -> Result<i64> {
        let res = {
            let inner_wait_future = self.inner_wait();

            tokio::select! {
                _ = cancel.cancelled() => {
                    // Attempt to stop the container. Ignore any errors, then resume
                    // waiting for the container to exit so we can get the exit code.
                    let _ = self.docker.stop_container(&self.container_id, None).await;
                    inner_wait_future.await
                },
                res = self.inner_wait() => { res }
            }
        };

        self.cleanup().await?;

        res
    }

    async fn inner_wait(&self) -> Result<i64> {
        let status_code = self
            .docker
            .wait_container(&self.container_id, None::<WaitContainerOptions<String>>)
            .try_collect::<Vec<_>>()
            .await?
            .first()
            .ok_or_else(|| anyhow!("missing wait response from daemon",))?
            .status_code;

        Ok(status_code)
    }

    pub async fn cleanup(self) -> Result<()> {
        let _ = self.docker.remove_container(&self.container_id, None).await;
        self.stream_task.await?;

        Ok(())
    }
}