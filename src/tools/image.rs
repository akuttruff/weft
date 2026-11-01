//! `see_image`: a look at a picture, answered by a vision model
//! on the chat route. The worker runs on the raw completions route, which
//! carries no images, so the picture goes to another model and only the
//! words come back.

use std::path::{Path, PathBuf};

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
    /// Answers already given, by picture and question, shared by the
    /// planner's and the worker's tools: a look takes a minute, and both
    /// tend to ask about the same screenshot.
    pub seen: Seen,
}

/// What `see_image` has answered this session.
pub type Seen =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<(PathBuf, String), String>>>;

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
        let path = find_image(&ctx.root, given)?;
        let mime = mime_of(given).ok_or_else(|| {
            ToolError::BadArgs("see_image reads png, jpg, gif and webp files".into())
        })?;
        let key = (path.clone(), question_of(args).to_string());
        if let Some(text) = self.seen.lock().unwrap().get(&key) {
            return Ok(text.clone());
        }
        let size = std::fs::metadata(&path)
            .map_err(|e| unreadable(&path, e))?
            .len();
        if size > MAX_BYTES {
            return Err(ToolError::Failed(format!(
                "{given} is {} MB; images over {} MB are not sent",
                size / 1_000_000,
                MAX_BYTES / 1_000_000
            )));
        }
        let bytes = std::fs::read(&path).map_err(|e| unreadable(&path, e))?;
        let data_url = format!("data:{mime};base64,{}", BASE64_STANDARD.encode(&bytes));
        let text = describe_image(&self.endpoint, self.effort, &data_url, &key.1)
            .await
            .map_err(|e| ToolError::Failed(format!("the vision model: {e}")))?;
        let text = cap(&text, OUT_LINES, OUT_BYTES);
        self.seen.lock().unwrap().insert(key, text.clone());
        Ok(text)
    }
}

/// The question asked, or the standing one.
fn question_of(args: &Value) -> &str {
    args.get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .unwrap_or(DEFAULT_QUESTION)
}

/// A file that would not open, said so the user knows what to do: macOS
/// keeps some folders (iCloud Drive, Desktop, Documents) from a terminal
/// until it is let in.
fn unreadable(path: &Path, e: std::io::Error) -> ToolError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => ToolError::Failed(format!(
            "macOS did not let weft read {}: give your terminal app access (System Settings › \
             Privacy & Security › Files and Folders, or Full Disk Access), or copy the picture \
             into the project",
            path.display()
        )),
        std::io::ErrorKind::NotFound => {
            ToolError::Failed(format!("there is no picture at {}", path.display()))
        }
        _ => ToolError::Io(e),
    }
}

/// The picture a path names. A screenshot dropped into the task lives outside
/// the project (Desktop, iCloud), so an absolute path may point anywhere: this
/// tool only reads pictures, and only to show them to a local model. The path
/// as the terminal pasted it has shell escapes (`com\~apple`), and macOS puts
/// a narrow no-break space before "PM" that a model retypes as a plain one, so
/// a miss looks again with those smoothed over.
fn find_image(root: &Path, given: &str) -> Result<PathBuf, ToolError> {
    let unescaped = unescape(given.trim());
    let path = if let Some(rest) = unescaped.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        home.join(rest)
    } else if Path::new(&unescaped).is_absolute() {
        PathBuf::from(&unescaped)
    } else {
        resolve(root, &unescaped)?
    };
    if path.exists() {
        return Ok(path);
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(path);
    };
    // The likely spellings first, by name alone: some folders (iCloud Drive)
    // let a terminal open a file it names but not list what is there.
    for name in spellings(&name.to_string_lossy()) {
        let p = dir.join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    let want = spaces_plain(&name.to_string_lossy());
    let found = std::fs::read_dir(dir).ok().and_then(|rd| {
        rd.flatten().map(|e| e.path()).find(|p| {
            p.file_name()
                .is_some_and(|n| spaces_plain(&n.to_string_lossy()) == want)
        })
    });
    Ok(found.unwrap_or(path))
}

/// Other ways the name may be spelt on disk: macOS writes "10.38 PM" in a
/// screenshot's name with a narrow no-break space, which a model types as a
/// plain one, and the other way round.
fn spellings(name: &str) -> Vec<String> {
    const NARROW: &str = "\u{202f}";
    let mut out = Vec::new();
    for half in ["AM", "PM"] {
        let plain = format!(" {half}");
        let narrow = format!("{NARROW}{half}");
        if name.contains(&plain) {
            out.push(name.replace(&plain, &narrow));
        }
        if name.contains(&narrow) {
            out.push(name.replace(&narrow, &plain));
        }
    }
    out
}

/// `a\ b\~c` → `a b~c`: a shell's backslash escapes taken out.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
        } else {
            out.push(c);
        }
    }
    out
}

/// Every kind of space as a plain one.
fn spaces_plain(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_narrow_space_before_pm_is_tried_both_ways() {
        assert_eq!(
            spellings("Shot at 10.38.45 PM.png"),
            ["Shot at 10.38.45\u{202f}PM.png"]
        );
        assert_eq!(
            spellings("Shot at 9.01\u{202f}AM.png"),
            ["Shot at 9.01 AM.png"]
        );
        assert!(spellings("plain.png").is_empty());
    }

    #[test]
    fn a_folder_that_cannot_be_listed_still_gives_up_its_screenshot() {
        let root = tempfile::tempdir().unwrap();
        let shots = tempfile::tempdir().unwrap();
        let real = shots.path().join("Shot at 10.38.45\u{202f}PM.png");
        std::fs::write(&real, b"png").unwrap();
        // No reading the folder, as iCloud Drive refuses a terminal; the
        // file itself still opens by its exact name.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(shots.path(), std::fs::Permissions::from_mode(0o300)).unwrap();
        let typed = format!("{}/Shot at 10.38.45 PM.png", shots.path().display());
        let found = find_image(root.path(), &typed);
        std::fs::set_permissions(shots.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(found.unwrap(), real);
    }

    #[test]
    fn finds_a_dropped_screenshot_outside_the_project() {
        let root = tempfile::tempdir().unwrap();
        let shots = tempfile::tempdir().unwrap();
        let real = shots.path().join("Shot at 10.38.45\u{202f}PM.png");
        std::fs::write(&real, b"png").unwrap();
        let typed = format!("{}/Shot\\ at 10.38.45 PM.png", shots.path().display());
        assert_eq!(find_image(root.path(), &typed).unwrap(), real);
        assert_eq!(unescape(r"com\~apple\~CloudDocs"), "com~apple~CloudDocs");
        // Relative paths still keep to the project.
        assert!(matches!(
            find_image(root.path(), "../x.png"),
            Err(ToolError::Outside(_))
        ));
    }

    #[test]
    fn knows_the_picture_kinds() {
        assert_eq!(mime_of("shots/a.PNG"), Some("image/png"));
        assert_eq!(mime_of("b.jpeg"), Some("image/jpeg"));
        assert_eq!(mime_of("notes.md"), None);
        assert_eq!(mime_of("noext"), None);
    }
}
