use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

use crate::parser::{StreamCandidate, BILIBILI_UA};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const STDERR_TAIL_LINES: usize = 50;
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(200);

type SpawnedFfmpeg = (
    Child,
    Arc<Mutex<VecDeque<String>>>,
    Option<tokio::task::JoinHandle<()>>,
);

#[derive(Debug)]
pub struct RecordingExit {
    pub manually_stopped: bool,
    pub status_success: bool,
    pub wait_error: Option<String>,
    pub stderr_tail: Vec<String>,
}

struct ActiveRecording {
    stop_tx: Option<oneshot::Sender<()>>,
    completion_rx: watch::Receiver<Option<Result<(), String>>>,
}

pub struct Recorder {
    active_records: Arc<Mutex<HashMap<i64, ActiveRecording>>>,
    ffmpeg_path: String,
}

impl Recorder {
    pub fn new(ffmpeg_path: String) -> Self {
        Self {
            active_records: Arc::new(Mutex::new(HashMap::new())),
            ffmpeg_path,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_record<F, Fut>(
        &self,
        task_id: i64,
        candidates: &[StreamCandidate],
        output_path: &str,
        proxy: &str,
        cookie: &str,
        room_id: &str,
        on_exit: F,
    ) -> Result<(), String>
    where
        F: FnOnce(RecordingExit) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        if candidates.is_empty() {
            return Err("直播流地址为空，主播可能未开播".to_string());
        }
        if self.is_active(task_id) {
            return Err("该录制任务已经在运行".to_string());
        }
        if let Some(parent) = std::path::Path::new(output_path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建输出目录失败: {}", e))?;
        }

        let mut failure_details = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            if std::path::Path::new(output_path).exists() {
                let _ = std::fs::remove_file(output_path);
            }
            let (mut child, stderr_tail, stderr_task) =
                self.spawn_ffmpeg(&candidate.url, output_path, proxy, cookie, room_id)?;

            match wait_until_ready(&mut child, output_path).await {
                Ok(()) => {
                    let (stop_tx, mut stop_rx) = oneshot::channel();
                    let (completion_tx, completion_rx) = watch::channel(None);
                    {
                        let mut records = self.active_records.lock().map_err(|e| e.to_string())?;
                        records.insert(
                            task_id,
                            ActiveRecording {
                                stop_tx: Some(stop_tx),
                                completion_rx,
                            },
                        );
                    }

                    let active_records = Arc::clone(&self.active_records);
                    tokio::spawn(async move {
                        let (manually_stopped, status_success, wait_error) = tokio::select! {
                            result = child.wait() => match result {
                                Ok(status) => (false, status.success(), None),
                                Err(error) => (false, false, Some(error.to_string())),
                            },
                            _ = &mut stop_rx => {
                                let kill_error = child.start_kill().err().map(|error| error.to_string());
                                match child.wait().await {
                                    Ok(status) => (true, status.success(), kill_error),
                                    Err(error) => (true, false, Some(error.to_string())),
                                }
                            }
                        };
                        if let Some(stderr_task) = stderr_task {
                            let _ = stderr_task.await;
                        }
                        let stderr_tail = stderr_tail
                            .lock()
                            .map(|tail| tail.iter().cloned().collect())
                            .unwrap_or_default();
                        let result = on_exit(RecordingExit {
                            manually_stopped,
                            status_success,
                            wait_error,
                            stderr_tail,
                        })
                        .await;
                        let _ = completion_tx.send(Some(result.clone()));
                        if let Ok(mut records) = active_records.lock() {
                            records.remove(&task_id);
                        }
                    });
                    return Ok(());
                }
                Err(error) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    if let Some(stderr_task) = stderr_task {
                        let _ = stderr_task.await;
                    }
                    let detail = stderr_tail
                        .lock()
                        .ok()
                        .and_then(|tail| tail.back().cloned())
                        .unwrap_or(error);
                    failure_details.push(format!("CDN {}: {}", index + 1, redact_url(&detail)));
                }
            }
        }

        let _ = std::fs::remove_file(output_path);
        Err(format!(
            "所有直播线路均无法产生有效数据：{}",
            failure_details.join("；")
        ))
    }

    fn spawn_ffmpeg(
        &self,
        stream_url: &str,
        output_path: &str,
        proxy: &str,
        cookie: &str,
        room_id: &str,
    ) -> Result<SpawnedFfmpeg, String> {
        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.args(["-y", "-nostdin", "-loglevel", "warning", "-nostats"]);
        if stream_url.starts_with("http://") || stream_url.starts_with("https://") {
            let mut headers = format!(
                "Referer: https://live.bilibili.com/{}\r\nUser-Agent: {}\r\n",
                room_id, BILIBILI_UA
            );
            if !cookie.trim().is_empty() {
                headers.push_str("Cookie: ");
                headers.push_str(cookie.trim());
                headers.push_str("\r\n");
            }
            cmd.args([
                "-rw_timeout",
                "60000000",
                "-reconnect",
                "1",
                "-reconnect_streamed",
                "1",
                "-reconnect_delay_max",
                "10",
                "-headers",
                &headers,
            ]);
        }
        cmd.args(["-i", stream_url, "-c", "copy", "-f", "flv", output_path])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.as_std_mut().creation_flags(0x08000000);
        if !proxy.trim().is_empty() {
            cmd.env("http_proxy", proxy.trim());
            cmd.env("https_proxy", proxy.trim());
        }

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg 未找到".to_string()
            } else {
                format!("启动 ffmpeg 录制失败: {}", e)
            }
        })?;
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_task = child.stderr.take().map(|stderr| {
            let stderr_tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            })
        });
        Ok((child, stderr_tail, stderr_task))
    }

    pub async fn stop_record(&self, task_id: i64) -> Result<bool, String> {
        let (stop_tx, mut completion_rx) = {
            let mut records = self.active_records.lock().map_err(|e| e.to_string())?;
            let Some(recording) = records.get_mut(&task_id) else {
                return Ok(false);
            };
            (recording.stop_tx.take(), recording.completion_rx.clone())
        };
        if let Some(stop_tx) = stop_tx {
            let _ = stop_tx.send(());
        }
        tokio::time::timeout(STOP_WAIT_TIMEOUT, async {
            loop {
                if let Some(result) = completion_rx.borrow().clone() {
                    return result;
                }
                completion_rx
                    .changed()
                    .await
                    .map_err(|_| "录制进程状态通道已关闭".to_string())?;
            }
        })
        .await
        .map_err(|_| "等待录制进程停止超时".to_string())??;
        Ok(true)
    }

    pub fn is_active(&self, task_id: i64) -> bool {
        self.active_records
            .lock()
            .map(|records| records.contains_key(&task_id))
            .unwrap_or(false)
    }
}

async fn wait_until_ready(child: &mut Child, output_path: &str) -> Result<(), String> {
    let started_at = tokio::time::Instant::now();
    loop {
        if std::fs::metadata(output_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("检查 FFmpeg 状态失败: {}", e))?
        {
            return Err(format!("FFmpeg 在产生数据前退出: {}", status));
        }
        if started_at.elapsed() >= STARTUP_TIMEOUT {
            return Err("等待直播流数据超时".to_string());
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }
}

fn redact_url(message: &str) -> String {
    let mut result = message.to_string();
    if let Some(start) = result.find("http") {
        if let Some(end_offset) = result[start..].find(char::is_whitespace) {
            result.replace_range(start..start + end_offset, "[直播地址]");
        } else {
            result.replace_range(start.., "[直播地址]");
        }
    }
    result
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Ok(mut records) = self.active_records.lock() {
            for (_, mut recording) in records.drain() {
                if let Some(stop_tx) = recording.stop_tx.take() {
                    let _ = stop_tx.send(());
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::Recorder;
    use crate::parser::StreamCandidate;
    use std::os::windows::process::CommandExt;
    use std::process::Command as StdCommand;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn observes_natural_ffmpeg_exit_and_removes_active_record() {
        let ffmpeg = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join("ffmpeg-x86_64-pc-windows-msvc.exe");
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "bilibili-recorder-lifecycle-{}-{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let input_path = temp_dir.join("finite-input.flv");
        let output_path = temp_dir.join("finite-output.flv");
        let mut generator = StdCommand::new(&ffmpeg);
        generator
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x32:r=1",
                "-t",
                "2",
                "-c:v",
                "flv",
                "-f",
                "flv",
            ])
            .arg(&input_path)
            .creation_flags(0x08000000);
        assert!(generator.status().unwrap().success());

        let recorder = Recorder::new(ffmpeg.to_string_lossy().to_string());
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        recorder
            .start_record(
                42,
                &[StreamCandidate {
                    url: input_path.to_string_lossy().to_string(),
                }],
                output_path.to_str().unwrap(),
                "",
                "",
                "42",
                move |exit| async move {
                    let _ = exit_tx.send(exit);
                    Ok(())
                },
            )
            .await
            .unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(10), exit_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(!exit.manually_stopped);
        assert!(exit.status_success);
        assert!(std::fs::metadata(&output_path).unwrap().len() > 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!recorder.is_active(42));
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
