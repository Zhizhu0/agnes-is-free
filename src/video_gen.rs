use serde::Deserialize;

/// Response from the video task creation endpoint.
#[derive(Debug, Deserialize)]
pub struct CreateVideoResponse {
    pub id: Option<String>,
    pub task_id: Option<String>,
    pub video_id: Option<String>,
    #[allow(dead_code)]
    pub status: Option<String>,
    #[allow(dead_code)]
    pub progress: Option<i64>,
}

/// Response from the video polling endpoint.
///
/// The API inconsistently uses `status` (top-level) or `internal_status` for the
/// status field, so we manually implement Deserialize to accept either.
#[derive(Debug)]
pub struct PollVideoResponse {
    pub status: Option<String>,
    pub video_url: Option<String>,
    pub output_url: Option<String>,
    pub error: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for PollVideoResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawResponse {
            pub status: Option<String>,
            pub internal_status: Option<String>,
            pub remixed_from_video_id: Option<String>,
            pub url: Option<String>,
            pub output_url: Option<String>,
            pub error: Option<serde_json::Value>,
        }
        let raw = RawResponse::deserialize(deserializer)?;
        Ok(PollVideoResponse {
            status: raw.status.or(raw.internal_status),
            video_url: raw.remixed_from_video_id.or(raw.url),
            output_url: raw.output_url,
            error: raw.error,
        })
    }
}

/// Result of polling a video task.
#[derive(Debug)]
pub enum PollResult {
    /// Still processing (queued / in_progress).
    Pending,
    /// Video is ready — value is the MP4 URL.
    Completed(String),
    /// Generation failed — value is the error message.
    Failed(String),
}

pub struct VideoGenClient {
    client: reqwest::Client,
}

impl VideoGenClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Internal implementation that dispatches between:
    /// - text-to-video (no images)
    /// - single-image-to-video (1 image → `image` field)
    /// - multi-image video / keyframes (1+ images → `extra_body.image` array)
    async fn create_internal(
        &self,
        base_url: &str,
        api_key: &str,
        prompt: &str,
        image_uris: Option<&[String]>,
        width: u32,
        height: u32,
        num_frames: u32,
        frame_rate: u32,
        negative_prompt: Option<&str>,
        seed: Option<u64>,
        is_keyframe: bool,
    ) -> Result<String, String> {
        // base_url is like "https://apihub.agnes-ai.com/v1".
        // The video create endpoint is at domain root: https://apihub.agnes-ai.com/v1/videos
        // so we just append "/videos" (the /v1 is already in base_url).
        let url = format!("{base_url}/videos");
        eprintln!("[agnes] INFO: POST {url} (video generation)");
        let mut body = serde_json::json!({
            "model": "agnes-video-v2.0",
            "prompt": prompt,
            "width": width,
            "height": height,
            "num_frames": num_frames,
            "frame_rate": frame_rate,
        });

        let uris = image_uris.unwrap_or(&[]);

        // Use extra_body.image array with data URIs (data:image/png;base64,...) —
        // same format as the image generation API. The video API shares the same
        // image handling backend and expects data URIs, not raw base64.
        if !uris.is_empty() {
            let mut extra = serde_json::json!({ "image": uris });
            if is_keyframe && uris.len() >= 2 {
                extra["mode"] = serde_json::json!("keyframes");
            }
            body["extra_body"] = extra;
        }

        eprintln!(
            "[agnes] DEBUG: Video create: {} image(s), uri_len={}",
            uris.len(),
            uris.first().map(|u| u.len()).unwrap_or(0)
        );

        if let Some(np) = negative_prompt {
            body["negative_prompt"] = serde_json::json!(np);
        }
        if let Some(s) = seed {
            body["seed"] = serde_json::json!(s);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unable to read error body>".into());
            return Err(format!("API error ({status}): {body}"));
        }

        let raw_text = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        eprintln!(
            "[agnes] DEBUG: Video create response: {}",
            raw_text.chars().take(500).collect::<String>()
        );

        let json: CreateVideoResponse = serde_json::from_str(&raw_text)
            .map_err(|e| format!("Failed to parse response JSON: {e}"))?;

        // Prefer video_id, fall back to task_id (both work for polling).
        let video_id = json
            .video_id
            .or(json.task_id)
            .or(json.id)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "Response contained no video_id/task_id. Raw: {}",
                    raw_text.chars().take(200).collect::<String>()
                )
            })?;

        eprintln!("[agnes] INFO: Video task created: {video_id}");
        Ok(video_id)
    }

    /// Create a multi-image/keyframe video task with exponential backoff retry.
    ///
    /// Retries up to `max_retries` times when the server returns 503 (service busy).
    /// Backoff: 1s, 2s, 4s, 8s, 16s, 32s, 64s, 128s, 256s, 512s (for 10 retries).
    /// For non-503 errors or successful responses, returns immediately.
    pub async fn create_with_retry_multi(
        &self,
        base_url: &str,
        api_key: &str,
        prompt: &str,
        image_uris: &[String],
        width: u32,
        height: u32,
        num_frames: u32,
        frame_rate: u32,
        negative_prompt: Option<&str>,
        seed: Option<u64>,
        is_keyframe: bool,
        max_retries: u32,
    ) -> Result<String, String> {
        let mut last_error = String::new();
        for attempt in 0..max_retries {
            if attempt > 0 {
                // Exponential backoff: 1s, 2s, 4s, 8s, ...
                let delay_secs = 1u64 << (attempt - 1);
                eprintln!(
                    "[agnes] INFO: Video create retry {attempt}/{max_retries} after {delay_secs}s..."
                );
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }

            match self
                .create_internal(
                    base_url,
                    api_key,
                    prompt,
                    Some(image_uris),
                    width,
                    height,
                    num_frames,
                    frame_rate,
                    negative_prompt,
                    seed,
                    is_keyframe,
                )
                .await
            {
                Ok(video_id) => {
                    if attempt > 0 {
                        eprintln!(
                            "[agnes] INFO: Video create succeeded on attempt {attempt}"
                        );
                    }
                    return Ok(video_id);
                }
                Err(e) => {
                    // Check if it's a 503 (service busy) — only retry on this.
                    if e.contains("503") || e.contains("Service busy") {
                        last_error = e;
                        eprintln!(
                            "[agnes] WARN: Video create attempt {attempt} failed (server busy): {last_error}"
                        );
                        continue;
                    }
                    // Non-retryable error — return immediately.
                    return Err(e);
                }
            }
        }

        Err(format!(
            "Video create failed after {max_retries} retries. Last error: {last_error}"
        ))
    }

    /// Poll the status of a video task.
    pub async fn poll(
        &self,
        base_url: &str,
        api_key: &str,
        video_id: &str,
    ) -> Result<PollResult, String> {
        // The poll endpoint is at domain root: https://apihub.agnes-ai.com/agnesapi
        // Derive domain root by stripping the trailing "/v1" from base_url.
        let domain_root = base_url.trim_end_matches("/v1");
        let url = format!("{domain_root}/agnesapi?video_id={video_id}");
        eprintln!("[agnes] INFO: GET {url} (poll video status)");

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
            .map_err(|e| format!("Poll request failed: {e}"))?;

        let status_code = resp.status();
        if !status_code.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Poll error ({status_code}): {body}"));
        }

        let raw_text = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read poll response: {e}"))?;

        eprintln!(
            "[agnes] DEBUG: Video poll response: {}",
            raw_text.chars().take(500).collect::<String>()
        );

        let json: PollVideoResponse = serde_json::from_str(&raw_text)
            .map_err(|e| format!("Failed to parse poll response: {e}"))?;

        // Try multiple possible URL fields (API may use any of these)
        let video_url: Option<String> = json
            .video_url
            .or(json.output_url)
            .filter(|u| !u.is_empty());

        match json.status.as_deref() {
            Some("completed") => {
                match video_url {
                    Some(url) => Ok(PollResult::Completed(url)),
                    None => Err("API returned 'completed' but no video URL in response".into()),
                }
            }
            Some("failed") => {
                let reason = json
                    .error
                    .and_then(|e| serde_json::to_string(&e).ok())
                    .unwrap_or_else(|| "Unknown error".into());
                Ok(PollResult::Failed(reason))
            }
            Some("queued") | Some("in_progress") | Some(_) | None => Ok(PollResult::Pending),
        }
    }

    /// Download a video from a URL and save it to a local path.
    /// Returns the path to the downloaded file.
    pub async fn download_video(
        &self,
        video_url: &str,
        dest_path: &str,
    ) -> Result<(), String> {
        eprintln!("[agnes] INFO: Downloading video from {}", &video_url[..video_url.len().min(120)]);

        let resp = self
            .client
            .get(video_url)
            .send()
            .await
            .map_err(|e| format!("Download request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("Download HTTP error: {status}"));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Download read failed: {e}"))?
            .to_vec();

        eprintln!("[agnes] INFO: Downloaded {} bytes, saving to {dest_path}", bytes.len());

        std::fs::write(dest_path, &bytes)
            .map_err(|e| format!("Failed to write video file: {e}"))?;

        Ok(())
    }
}
