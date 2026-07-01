use serde::Deserialize;
use std::fmt;

/// Errors that can occur during image generation.
#[derive(Debug, Clone)]
pub enum ImageGenError {
    /// The HTTP request failed (network, timeout, HTTP error).
    Http(String),
    /// The API returned an error response.
    ApiError(String),
    /// Failed to decode base64 or parse the response.
    DecodeError(String),
}

impl fmt::Display for ImageGenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageGenError::Http(msg) => write!(f, "Image gen HTTP error: {msg}"),
            ImageGenError::ApiError(msg) => write!(f, "Image gen API error: {msg}"),
            ImageGenError::DecodeError(msg) => write!(f, "Image gen decode error: {msg}"),
        }
    }
}

impl std::error::Error for ImageGenError {}

/// Response from the image generation API.
#[derive(Debug, Deserialize)]
struct ImageGenResponse {
    data: Vec<ImageGenData>,
}

#[derive(Debug, Deserialize)]
struct ImageGenData {
    b64_json: Option<String>,
    url: Option<String>,
}

/// Image generation client.
pub struct ImageGenClient {
    client: reqwest::Client,
}

impl ImageGenClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Generate an image from a text prompt.
    /// Returns the raw PNG bytes on success.
    pub async fn generate(
        &self,
        base_url: &str,
        api_key: &str,
        prompt: &str,
    ) -> Result<Vec<u8>, ImageGenError> {
        let url = format!("{base_url}/images/generations");
        eprintln!("[agnes] INFO: POST {url} (image generation)");

        let body = serde_json::json!({
            "model": "agnes-image-2.1-flash",
            "prompt": prompt,
            "size": "1024x768",
            "return_base64": true,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                let err = ImageGenError::Http(format!("Request failed: {e}"));
                eprintln!("[agnes] ERROR: {err}");
                err
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unable to read error body>".into());
            let err = ImageGenError::Http(format!("API error ({status}): {body}"));
            eprintln!("[agnes] ERROR: {err}");
            return Err(err);
        }

        // Dump the raw API response for debugging.
        let raw_text = resp.text().await.map_err(|e| {
            let err = ImageGenError::DecodeError(format!("Failed to read response body: {e}"));
            eprintln!("[agnes] ERROR: {err}");
            err
        })?;
        eprintln!("[agnes] DEBUG: Image API response: {}", raw_text.chars().take(2000).collect::<String>());

        let json: ImageGenResponse = serde_json::from_str(&raw_text).map_err(|e| {
            let err = ImageGenError::DecodeError(format!("Failed to parse response JSON: {e}"));
            eprintln!("[agnes] ERROR: {err}");
            err
        })?;

        // Try url first (OpenAI standard), then b64_json (return_base64 mode).
        let image_data = json
            .data
            .first()
            .and_then(|d| d.url.as_ref().map(|u| u.as_str()).or_else(|| d.b64_json.as_ref().map(|b| b.as_str())))
            .ok_or_else(|| {
                let err = ImageGenError::ApiError(format!(
                    "Response contained no image data. data array length: {}",
                    json.data.len()
                ));
                eprintln!("[agnes] ERROR: {err}");
                err
            })?;

        // If it's a URL, download the image. If it's base64, decode it.
        if image_data.starts_with("http://") || image_data.starts_with("https://") {
            eprintln!("[agnes] INFO: Image returned as URL, downloading...");
            return self.download_image(image_data).await;
        }

        let b64 = image_data;

        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| {
                let err = ImageGenError::DecodeError(format!("Base64 decode failed: {e}"));
                eprintln!("[agnes] ERROR: {err}");
                err
            })
    }

    /// Download an image from a URL.
    async fn download_image(&self, url: &str) -> Result<Vec<u8>, ImageGenError> {
        eprintln!("[agnes] INFO: Downloading image from {}", &url[..url.len().min(120)]);
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| ImageGenError::Http(format!("Image download failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ImageGenError::Http(format!(
                "Image download HTTP {status}"
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ImageGenError::Http(format!("Image download read failed: {e}")))?
            .to_vec();

        eprintln!("[agnes] INFO: Downloaded {} bytes", bytes.len());
        Ok(bytes)
    }
}
