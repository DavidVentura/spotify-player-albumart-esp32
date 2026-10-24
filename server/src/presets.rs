//! Saved images that can be shown with one click, loaded once at startup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::frame::Frame;
use crate::scale;

pub struct Preset {
    /// The file as stored, served to the browser as a thumbnail.
    pub original: Vec<u8>,
    pub frame: Arc<Frame>,
}

/// Keyed by file name without extension.
pub type Presets = BTreeMap<String, Preset>;

#[derive(Debug)]
pub enum PresetError {
    Read(PathBuf, std::io::Error),
    Name(PathBuf),
    Image(PathBuf, image::ImageError),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresetError::Read(path, e) => write!(f, "reading {}: {e}", path.display()),
            PresetError::Name(path) => write!(f, "{} has no usable file name", path.display()),
            PresetError::Image(path, e) => write!(f, "decoding {}: {e}", path.display()),
        }
    }
}

impl std::error::Error for PresetError {}

/// Every file in `directory` must be an image.
pub fn load(directory: &Path) -> Result<Presets, PresetError> {
    let entries =
        std::fs::read_dir(directory).map_err(|e| PresetError::Read(directory.to_path_buf(), e))?;
    let mut presets = Presets::new();
    for entry in entries {
        let path = entry
            .map_err(|e| PresetError::Read(directory.to_path_buf(), e))?
            .path();
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| PresetError::Name(path.clone()))?
            .to_string();
        let original = std::fs::read(&path).map_err(|e| PresetError::Read(path.clone(), e))?;
        let frame = scale::to_frame(&original).map_err(|e| PresetError::Image(path.clone(), e))?;
        presets.insert(
            name,
            Preset {
                original,
                frame: Arc::new(frame),
            },
        );
    }
    Ok(presets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_bundled_presets() {
        let presets = load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("presets")).unwrap();
        assert!(presets.contains_key("poop"));
        assert!(presets.contains_key("dog"));
    }

    #[test]
    fn rejects_non_images() {
        let directory = std::env::temp_dir().join(format!("presets-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("notes.txt"), "not an image").unwrap();
        let result = load(&directory);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(matches!(result, Err(PresetError::Image(..))));
    }
}
