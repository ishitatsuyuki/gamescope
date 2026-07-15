//! Bounded asynchronous screenshot encoding.

use std::{
    fs::File,
    io::{BufWriter, Write as _},
    path::Path,
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Debug)]
pub struct ScreenshotJob {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// File encoding never runs on Wayland, XWM, or DRM critical sections.
pub struct ScreenshotWriter {
    jobs: Option<SyncSender<ScreenshotJob>>,
    results: Receiver<Result<String, String>>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenshotWriter {
    #[must_use]
    pub fn spawn() -> Self {
        let (jobs, job_rx) = mpsc::sync_channel::<ScreenshotJob>(4);
        let (result_tx, results) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("gamescope-screenshot".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let path = job.path.clone();
                    let result = encode(job)
                        .map(|()| path.clone())
                        .map_err(|error| format!("failed to save screenshot {path}: {error}"));
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("screenshot encoder thread must start");
        Self {
            jobs: Some(jobs),
            results,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, job: ScreenshotJob) -> Result<(), String> {
        self.jobs
            .as_ref()
            .expect("screenshot writer is running")
            .try_send(job)
            .map_err(|error| match error {
                TrySendError::Full(_) => "screenshot encoder queue is full".into(),
                TrySendError::Disconnected(_) => "screenshot encoder stopped".into(),
            })
    }

    pub fn try_result(&self) -> Option<Result<String, String>> {
        self.results.try_recv().ok()
    }

    pub fn wait_result(&self, timeout: Duration) -> Option<Result<String, String>> {
        self.results.recv_timeout(timeout).ok()
    }
}

impl Drop for ScreenshotWriter {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn encode(job: ScreenshotJob) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let expected = usize::try_from(job.width)?
        .checked_mul(usize::try_from(job.height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("screenshot dimensions overflow")?;
    if job.rgba.len() != expected {
        return Err(format!(
            "RGBA buffer has {} bytes, expected {expected}",
            job.rgba.len()
        )
        .into());
    }
    match Path::new(&job.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => encode_png(&job),
        Some("ppm") => encode_ppm(&job),
        Some("raw") => {
            File::create(&job.path)?.write_all(&job.rgba)?;
            Ok(())
        }
        extension => Err(format!("unsupported screenshot extension {extension:?}").into()),
    }
}

fn encode_png(job: &ScreenshotJob) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(&job.path)?),
        job.width,
        job.height,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&job.rgba)?;
    Ok(())
}

fn encode_ppm(job: &ScreenshotJob) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut output = BufWriter::new(File::create(&job.path)?);
    write!(output, "P6\n{} {}\n255\n", job.width, job.height)?;
    for pixel in job.rgba.chunks_exact(4) {
        output.write_all(&pixel[..3])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::{ScreenshotJob, ScreenshotWriter};

    #[test]
    fn raw_screenshot_is_encoded_off_thread() {
        let path = std::env::temp_dir().join(format!(
            "gamescope-rs-screenshot-{}-{}.raw",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let writer = ScreenshotWriter::spawn();
        writer
            .submit(ScreenshotJob {
                path: path.display().to_string(),
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            })
            .expect("queue screenshot");
        let result = writer
            .wait_result(Duration::from_secs(1))
            .expect("encoder result");
        assert!(result.is_ok());
        assert_eq!(fs::read(&path).expect("raw screenshot"), [1, 2, 3, 4]);
        fs::remove_file(path).expect("remove screenshot");
    }
}
