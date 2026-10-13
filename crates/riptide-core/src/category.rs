// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! File categories: which folder a download belongs in, and which links are worth capturing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A named group of extensions with a destination folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    /// Lowercase, without the leading dot.
    pub extensions: Vec<String>,
    /// Folder relative to the download root, or absolute to send the category elsewhere.
    pub folder: PathBuf,
    /// Whether browser integration should offer to take links of these types.
    #[serde(default = "default_true")]
    pub capture: bool,
}

fn default_true() -> bool {
    true
}

/// The ordered category list. The first category claiming an extension wins, and anything
/// unclaimed falls to [`CategorySet::fallback`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategorySet {
    pub categories: Vec<Category>,
    /// Where unmatched files go.
    pub fallback: PathBuf,
}

impl Default for CategorySet {
    fn default() -> Self {
        Self::standard()
    }
}

impl CategorySet {
    /// A sensible default set covering what people actually download.
    pub fn standard() -> Self {
        let cat = |name: &str, folder: &str, exts: &[&str]| Category {
            name: name.to_string(),
            folder: PathBuf::from(folder),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            capture: true,
        };
        Self {
            categories: vec![
                cat(
                    "Video",
                    "Video",
                    &[
                        "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg",
                        "m2ts", "ts", "3gp", "ogv", "rm", "rmvb", "asf", "divx", "vob", "f4v",
                    ],
                ),
                cat(
                    "Audio",
                    "Audio",
                    &[
                        "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "ape",
                        "alac", "mid", "midi", "ra",
                    ],
                ),
                cat(
                    "Archives",
                    "Archives",
                    &[
                        "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "tgz", "tbz", "lzh",
                        "ace", "arj", "cab", "iso", "img", "dmg",
                    ],
                ),
                cat(
                    "Programs",
                    "Programs",
                    &[
                        "exe", "msi", "msu", "appx", "msix", "apk", "deb", "rpm", "pkg", "bin",
                        "run", "appimage",
                    ],
                ),
                cat(
                    "Documents",
                    "Documents",
                    &[
                        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp",
                        "rtf", "txt", "epub", "mobi", "azw3", "djvu", "csv",
                    ],
                ),
                cat(
                    "Images",
                    "Images",
                    &[
                        "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd",
                        "raw", "heic", "avif",
                    ],
                ),
            ],
            fallback: PathBuf::from("Other"),
        }
    }

    /// The category a filename belongs to, if any.
    pub fn categorize(&self, filename: &str) -> Option<&Category> {
        let ext = extension_of(filename)?;
        self.categories.iter().find(|c| c.extensions.contains(&ext))
    }

    /// The folder for a filename, under `root`.
    pub fn folder_for(&self, root: &Path, filename: &str) -> PathBuf {
        let sub = match self.categorize(filename) {
            Some(c) => &c.folder,
            None => &self.fallback,
        };
        // An absolute category folder redirects the whole category off the download root.
        if sub.is_absolute() {
            sub.clone()
        } else {
            root.join(sub)
        }
    }

    /// Whether browser integration should take over a link with this name.
    pub fn should_capture(&self, filename: &str) -> bool {
        match self.categorize(filename) {
            Some(c) => c.capture,
            // An unknown extension is not automatically interesting; grabbing every link would
            // intercept ordinary navigation.
            None => false,
        }
    }

    /// Every extension marked for capture, for handing to the browser extension.
    pub fn capture_extensions(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .categories
            .iter()
            .filter(|c| c.capture)
            .flat_map(|c| c.extensions.iter().cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// The lowercase extension of a filename, ignoring any query string.
pub fn extension_of(filename: &str) -> Option<String> {
    let name = filename.split(['?', '#']).next().unwrap_or(filename);
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (stem, ext) = base.rsplit_once('.')?;
    if stem.is_empty()
        || ext.is_empty()
        || ext.len() > 12
        || !ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_extracted_from_realistic_names() {
        assert_eq!(extension_of("movie.MKV").as_deref(), Some("mkv"));
        assert_eq!(extension_of("archive.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension_of("/path/to/song.mp3").as_deref(), Some("mp3"));
        // A query string is not part of the extension.
        assert_eq!(extension_of("video.mp4?token=abc.def").as_deref(), Some("mp4"));
    }

    #[test]
    fn things_that_are_not_extensions_are_rejected() {
        assert_eq!(extension_of("README"), None);
        assert_eq!(extension_of(".gitignore"), None, "a dotfile has no extension");
        assert_eq!(extension_of("trailing."), None);
        assert_eq!(extension_of("v1.2.3-build"), None, "not alphanumeric");
        assert_eq!(extension_of("file.averylongextension"), None);
    }

    #[test]
    fn files_are_routed_to_their_category_folder() {
        let set = CategorySet::standard();
        let root = Path::new("D:\\Downloads");
        assert_eq!(set.folder_for(root, "film.mkv"), root.join("Video"));
        assert_eq!(set.folder_for(root, "track.flac"), root.join("Audio"));
        assert_eq!(set.folder_for(root, "setup.exe"), root.join("Programs"));
        assert_eq!(set.folder_for(root, "paper.pdf"), root.join("Documents"));
        assert_eq!(set.folder_for(root, "backup.7z"), root.join("Archives"));
        // Unknown types land in the fallback rather than the root itself.
        assert_eq!(set.folder_for(root, "data.xyz"), root.join("Other"));
        assert_eq!(set.folder_for(root, "NOTES"), root.join("Other"));
    }

    #[test]
    fn an_absolute_category_folder_redirects_off_the_root() {
        let mut set = CategorySet::standard();
        set.categories[0].folder = PathBuf::from("E:\\Media\\Films");
        assert_eq!(
            set.folder_for(Path::new("D:\\Downloads"), "film.mkv"),
            PathBuf::from("E:\\Media\\Films")
        );
    }

    #[test]
    fn capture_covers_known_types_and_ignores_the_rest() {
        let set = CategorySet::standard();
        assert!(set.should_capture("film.mkv"));
        assert!(set.should_capture("setup.exe"));
        // Capturing every link would hijack ordinary page navigation.
        assert!(!set.should_capture("index.html"));
        assert!(!set.should_capture("page"));
    }

    #[test]
    fn a_category_can_be_excluded_from_capture() {
        let mut set = CategorySet::standard();
        let images = set.categories.iter_mut().find(|c| c.name == "Images").unwrap();
        images.capture = false;
        assert!(!set.should_capture("photo.jpg"));
        assert!(!set.capture_extensions().contains(&"jpg".to_string()));
        assert!(set.capture_extensions().contains(&"mkv".to_string()));
    }

    #[test]
    fn the_capture_list_is_sorted_and_free_of_duplicates() {
        let exts = CategorySet::standard().capture_extensions();
        let mut sorted = exts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(exts, sorted);
        assert!(exts.len() > 50, "expected a broad default list, got {}", exts.len());
    }

    #[test]
    fn the_category_set_round_trips_so_users_can_edit_it() {
        let set = CategorySet::standard();
        let json = serde_json::to_string_pretty(&set).unwrap();
        let back: CategorySet = serde_json::from_str(&json).unwrap();
        assert_eq!(back.categories.len(), set.categories.len());
        assert_eq!(
            back.folder_for(Path::new("/d"), "a.mp4"),
            set.folder_for(Path::new("/d"), "a.mp4")
        );
    }
}
