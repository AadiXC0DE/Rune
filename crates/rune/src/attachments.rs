//! Bounded, explicit image attachments for a one-shot ask turn.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rune_agent::turn::CallResult;
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
    fn reducer_for(&self, plan: &RequestPlan) -> Box<dyn StreamReducer> {
        self.dialect.reducer_for(plan)
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

/// First id given to a tool image.
const FIRST_TOOL_IMAGE_ID: u64 = 1 << 32;

/// Images tools returned during a conversation, by the id recorded in its history.
#[derive(Default)]
pub struct ToolImages {
    next: AtomicU64,
    data: Mutex<BTreeMap<u64, Arc<str>>>,
}

impl ToolImages {
    /// Ids start at 1 << 32 so they never meet prompt attachment ids.
    pub fn attach(&self, result: &CallResult) -> Vec<ContentPart> {
        let Some(image) = result.output.image.as_ref().filter(|_| result.executed) else {
            return Vec::new();
        };
        let id = FIRST_TOOL_IMAGE_ID.saturating_add(self.next.fetch_add(1, Ordering::Relaxed));
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::from(crate::session::base64_encode(&image.bytes)));
        vec![ContentPart::Image {
            image: ImageRef {
                id,
                media_type: image.media_type.to_owned(),
                encoded_bytes: image.bytes.len() as u64,
            },
        }]
    }

    /// Copies every held image into the plan before a request is built.
    ///
    /// A tool image id already in the plan without data, as a resumed
    /// conversation holds, is never given out again, so it cannot pick up the
    /// data of a later image.
    pub fn hydrate(&self, plan: &mut RequestPlan) {
        let data = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorded = plan
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                ContentPart::Image { image } if image.id >= FIRST_TOOL_IMAGE_ID => Some(image.id),
                _ => None,
            })
            .max();
        if let Some(id) = recorded {
            let next = id.saturating_sub(FIRST_TOOL_IMAGE_ID).saturating_add(1);
            self.next.fetch_max(next, Ordering::Relaxed);
        }
        plan.image_data
            .extend(data.iter().map(|(id, encoded)| (*id, Arc::clone(encoded))));
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

    fn image_result(id: &str, executed: bool) -> CallResult {
        let mut output = rune_tools::ToolOutput::success("image shot.png (3 bytes) attached");
        output.image = Some(rune_tools::ToolImage {
            media_type: "image/png",
            bytes: PNG.to_vec(),
        });
        CallResult {
            call: rune_agent::turn::PreparedCall {
                id: id.to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"shot.png"}"#.to_owned(),
            },
            output,
            executed,
        }
    }

    #[test]
    fn tool_images_are_attached_once_and_hydrated_into_each_request() {
        let images = ToolImages::default();
        let first = images.attach(&image_result("a", true));
        let second = images.attach(&image_result("b", true));
        assert!(images.attach(&image_result("c", false)).is_empty());
        assert!(
            images
                .attach(&CallResult {
                    output: rune_tools::ToolOutput::success("text"),
                    ..image_result("d", true)
                })
                .is_empty()
        );
        let ids: Vec<u64> = first
            .iter()
            .chain(&second)
            .map(|part| match part {
                ContentPart::Image { image } => {
                    assert_eq!(image.media_type, "image/png");
                    assert_eq!(image.encoded_bytes, PNG.len() as u64);
                    image.id
                }
                other => panic!("not an image: {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![1 << 32, (1 << 32) + 1]);

        let encoded = crate::session::base64_encode(PNG);
        for _ in 0..2 {
            let mut plan = RequestPlan::new("fixture-model");
            images.hydrate(&mut plan);
            assert_eq!(plan.image_data.len(), 2);
            assert_eq!(plan.image_data[&(1 << 32)].as_ref(), encoded);
            assert_eq!(plan.image_data[&((1 << 32) + 1)].as_ref(), encoded);
        }

        // A resumed history may hold ids this process never gave out.
        let resumed = ToolImages::default();
        let mut plan = RequestPlan::new("fixture-model");
        plan.messages = vec![Message::user_parts(vec![ContentPart::Image {
            image: ImageRef {
                id: (1 << 32) + 4,
                media_type: "image/png".to_owned(),
                encoded_bytes: 3,
            },
        }])];
        resumed.hydrate(&mut plan);
        assert!(plan.image_data.is_empty());
        let fresh = resumed.attach(&image_result("e", true));
        assert!(matches!(&fresh[..], [ContentPart::Image { image }] if image.id == (1 << 32) + 5));
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
