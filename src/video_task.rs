/// Status of an in-progress video generation task.
#[derive(Debug, Clone)]
pub enum VideoStatus {
    /// Task accepted, waiting for generation to finish.
    Pending,
    /// Video generated successfully — value is the local file path.
    Completed(String),
    /// Generation failed — value is the error reason.
    Failed(String),
}
