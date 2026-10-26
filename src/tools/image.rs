//! `see_image`: a look at a picture in the project, answered by a vision model
//! on the chat route. The worker runs on the raw completions route, which
//! carries no images, so the picture goes to another model and only the
//! words come back.

use async_trait::async_trait;
use base64::prelude::*;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError, arg_str, cap, resolve};
use crate::lm::chat::describe_image;
use crate::lm::{Effort, Endpoint, ToolSpec};

/// Pictures larger than this are refused rather than sent.
const MAX_BYTES: u64 = 12_000_000;
const OUT_LINES: usize = 200;
const OUT_BYTES: usize = 12_000;
const DEFAULT_QUESTION: &str = "Describe this image in detail: its layout, every piece of text in it verbatim, and anything that looks wrong or unfinished.";

pub struct SeeImage {
    pub endpoint: Endpoint,
    pub effort: Option<Effort>,
}

/// The media type for a file name, for the kinds the models read.
pub fn mime_of(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

#[async_trait]
impl Tool for SeeImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "see_image".into(),
            description: "Look at an image file (a screenshot, a diagram) and answer a question \
                          about it, or describe it. The only way you can see a picture."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The image file, png, jpg, gif or webp" },
                    "question": { "type": "string", "description": "What to find out from it (optional)" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let given = arg_str(args, "path")?;
        let path = resolve(&ctx.root, given)?;
        let mime = mime_of(given).ok_or_else(|| {
            ToolError::BadArgs("see_image reads png, jpg, gif and webp files".into())
        })?;
        let size = std::fs::metadata(&path)?.len();
        if size > MAX_BYTES {
            return Err(ToolError::Failed(format!(
                "{given} is {} MB; images over {} MB are not sent",
                size / 1_000_000,
                MAX_BYTES / 1_000_000
            )));
        }
        let bytes = std::fs::read(&path)?;
        let data_url = format!("data:{mime};base64,{}", BASE64_STANDARD.encode(&bytes));
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .unwrap_or(DEFAULT_QUESTION);
        let text = describe_image(&self.endpoint, self.effort, &data_url, question)
            .await
            .map_err(|e| ToolError::Failed(format!("the vision model: {e}")))?;
        Ok(cap(&text, OUT_LINES, OUT_BYTES))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_picture_kinds() {
        assert_eq!(mime_of("shots/a.PNG"), Some("image/png"));
        assert_eq!(mime_of("b.jpeg"), Some("image/jpeg"));
        assert_eq!(mime_of("notes.md"), None);
        assert_eq!(mime_of("noext"), None);
    }
}
