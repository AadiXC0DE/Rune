//! Bounded, explicit image attachments for a one-shot ask turn.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_net::message::{ContentPart, ImageRef};
use rune_net::provider::{Provider, ProviderCapabilities, RequestPlan, Routing};
use rune_net::stream::{Limit, StreamReducer};

/// Hard byte ceiling for the complete attachment batch, before base64.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGES: usize = 16;

#[derive(Default)]
pub struct Attachments {
    images: Vec<ImageRef>,
    data: BTreeMap<u64, Arc<str>>,
}

impl Attachments {
    /// Reads the whole batch before a request, with bounded reads even if a file grows.
    pub fn read(paths: &[PathBuf]) -> Result<Self> {
        if paths.len() > MAX_IMAGES {
            return Err(RuneError::invalid_field(
                "image",
                format!("at most {MAX_IMAGES} image attachments are accepted"),
            ));
        }
        let mut attachments = Self::default();
        let mut total = 0;
        for path in paths {
            // Check the path before opening so directories and special files are
            // refused without a potentially blocking read.
            if !std::fs::metadata(path)?.is_file() {
                return Err(RuneError::invalid_field(
                    "image",
                    format!("`{}` is not a regular image file", path.display()),
                ));
            }
            let file = std::fs::File::open(path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(RuneError::invalid_field(
                    "image",
                    "expected a regular image file",
                ));
            }
            let remaining = MAX_IMAGE_BYTES.saturating_sub(total);
            if metadata.len() > remaining as u64 {
                return Err(RuneError::too_large(
                    "image",
                    total.saturating_add(usize::try_from(metadata.len()).unwrap_or(usize::MAX)),
                    MAX_IMAGE_BYTES,
                ));
            }
            let mut bytes = Vec::new();
            file.take(remaining.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            total = total.saturating_add(bytes.len());
            if total > MAX_IMAGE_BYTES {
                return Err(RuneError::too_large("image", total, MAX_IMAGE_BYTES));
            }
            let media_type = rune_tools::vision::image_media_type(&bytes)
                .filter(|media_type| *media_type != "image/bmp")
                .ok_or_else(|| {
                    RuneError::new(
                        ErrorCode::Unsupported,
                        format!(
                            "`{}` does not have a supported image signature",
                            path.display()
                        ),
                    )
                    .with_hint("attach a PNG, JPEG, GIF, or WebP file")
                })?;
            let id = (attachments.images.len() as u64).saturating_add(1);
            attachments.images.push(ImageRef {
                id,
                media_type: media_type.to_owned(),
                encoded_bytes: bytes.len() as u64,
            });
            attachments
                .data
                .insert(id, Arc::from(crate::session::base64_encode(&bytes)));
        }
        Ok(attachments)
    }

    pub fn parts(&self) -> impl Iterator<Item = ContentPart> + '_ {
        self.images
            .iter()
            .cloned()
            .map(|image| ContentPart::Image { image })
    }

    pub fn provider(&self, dialect: Box<dyn Provider>) -> Box<dyn Provider> {
        if self.images.is_empty() {
            dialect
        } else {
            Box::new(AttachedProvider {
                dialect,
                data: self.data.clone(),
            })
        }
    }
}

/// Hydrates references on every request, including retries and follow-up tool steps.
struct AttachedProvider {
    dialect: Box<dyn Provider>,
    data: BTreeMap<u64, Arc<str>>,
}

impl Provider for AttachedProvider {
    fn name(&self) -> &'static str {
        self.dialect.name()
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.dialect.capabilities()
    }
    fn request_path(&self) -> &'static str {
        self.dialect.request_path()
    }
    fn completion_path(&self, plan: &RequestPlan) -> Result<String> {
        self.dialect.completion_path(plan)
    }
    fn models_path(&self) -> Option<&'static str> {
        self.dialect.models_path()
    }
    fn reducer(&self) -> Box<dyn StreamReducer> {
        self.dialect.reducer()
    }
    fn extra_headers(&self) -> Vec<(&'static str, String)> {
        self.dialect.extra_headers()
    }
    fn routing(&self) -> Routing {
        self.dialect.routing()
    }
    fn limits(&self) -> Limit {
        self.dialect.limits()
    }
    fn validate(&self, plan: &RequestPlan) -> Result<()> {
        self.dialect.validate(plan)
    }
    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let mut hydrated = plan.clone();
        hydrated.image_data.extend(self.data.clone());
        self.dialect.build_request(&hydrated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_net::message::Message;

    const PNG: &[u8] = include_bytes!("../tests/fixtures/attachment.png");

    #[test]
    fn attachments_reach_each_supporting_dialect_as_inline_image_parts() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = dir.path().join("image.dat");
        std::fs::write(&path, PNG).expect("PNG fixture with no image extension");
        let attachments = Attachments::read(&[path]).expect("attachments");
        let mut parts = vec![ContentPart::Text {
            text: "look".to_owned(),
        }];
        parts.extend(attachments.parts());
        let mut plan = RequestPlan::new("fixture-model");
        plan.messages = vec![Message::user_parts(parts)];
        let encoded = crate::session::base64_encode(PNG);
        let dialects: [Box<dyn Provider>; 4] = [
            Box::new(rune_net::chat_completions::ChatCompletions),
            Box::new(rune_net::responses::Responses),
            Box::new(rune_net::anthropic::Anthropic),
            Box::new(rune_net::gemini::Gemini),
        ];
        for dialect in dialects {
            let name = dialect.name();
            let provider = attachments.provider(dialect);
            provider.validate(&plan).expect("image capability");
            let body = provider.build_request(&plan).expect("inline request");
            let content = match name {
                "responses" => &body["input"][0]["content"],
                "gemini" => &body["contents"][0]["parts"],
                _ => &body["messages"][0]["content"],
            };
            assert_eq!(content.as_array().expect("two content parts").len(), 2);
            match name {
                "anthropic" => assert_eq!(
                    content[1]["source"],
                    serde_json::json!({"type": "base64", "media_type": "image/png", "data": encoded})
                ),
                "gemini" => assert_eq!(
                    content[1]["inlineData"],
                    serde_json::json!({"mimeType": "image/png", "data": encoded})
                ),
                "responses" => assert_eq!(
                    content[1]["image_url"],
                    format!("data:image/png;base64,{encoded}")
                ),
                _ => assert_eq!(
                    content[1]["image_url"]["url"],
                    format!("data:image/png;base64,{encoded}")
                ),
            }
            assert!(!body.to_string().contains("image-ref:"));
        }
    }

    #[test]
    fn attachment_count_is_bounded_before_reading_files() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = dir.path().join("image.png");
        std::fs::write(&path, PNG).expect("PNG fixture");
        let paths = vec![path; MAX_IMAGES];
        assert_eq!(
            Attachments::read(&paths)
                .expect("maximum batch")
                .parts()
                .count(),
            MAX_IMAGES
        );
        let paths = vec![dir.path().join("missing.png"); MAX_IMAGES + 1];
        let error = Attachments::read(&paths).err().expect("count cap");
        assert_eq!(error.code(), ErrorCode::InvalidField);
        assert!(error.message().contains("at most 16"));
    }

    #[test]
    fn invalid_missing_and_nonregular_attachments_are_refused() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let invalid = dir.path().join("invalid.png");
        std::fs::write(&invalid, "not an image").expect("invalid fixture");
        assert_eq!(
            Attachments::read(&[invalid]).err().expect("invalid").code(),
            ErrorCode::Unsupported
        );
        assert_eq!(
            Attachments::read(&[dir.path().to_owned()])
                .err()
                .expect("directory")
                .code(),
            ErrorCode::InvalidField
        );
        assert!(Attachments::read(&[dir.path().join("missing.png")]).is_err());
    }
}
