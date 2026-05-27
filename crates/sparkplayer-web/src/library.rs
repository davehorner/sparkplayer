//! Web implementations of [`MediaLibrary`] and [`ConfigStore`]. There is no
//! filesystem browser in the browser: tracks come from a fetched `manifest.json`
//! or from user-picked local files (handled in `lib.rs`), so most of these
//! methods are inert. Settings persist in `localStorage`.

use std::path::{Path, PathBuf};

use web_sys::window;

use sparkplayer_core::backend::{ConfigStore, MediaLibrary};
use sparkplayer_core::config::Config;
use sparkplayer_core::library::{Track, TrackRef};
use sparkplayer_core::metadata::TrackMeta;
use sparkplayer_core::subtitles::SubtitleSet;

const STORAGE_KEY: &str = "sparkplayer";

pub struct WebLibrary;

impl MediaLibrary for WebLibrary {
    fn browse(&self, _dir: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    fn load_playlist(&self, _source: &TrackRef) -> anyhow::Result<Vec<Track>> {
        Ok(Vec::new())
    }

    fn scan_directory(&self, _dir: &Path) -> Vec<Track> {
        Vec::new()
    }

    fn read_metadata(&self, _source: &TrackRef) -> TrackMeta {
        // Titles/artists shown in the UI fall back to the Track's display name
        // (set from the manifest or the picked file). The element learns its
        // real duration asynchronously; `lib.rs` folds that into the App.
        TrackMeta::default()
    }

    fn find_cover(&self, _source: &TrackRef) -> Option<Vec<u8>> {
        None
    }

    fn load_subtitles(&self, _source: &TrackRef) -> SubtitleSet {
        SubtitleSet::default()
    }
}

pub struct LocalStorageConfig;

impl LocalStorageConfig {
    fn storage() -> Option<web_sys::Storage> {
        window()?.local_storage().ok().flatten()
    }
}

impl ConfigStore for LocalStorageConfig {
    fn load(&self) -> Config {
        match Self::storage().and_then(|s| s.get_item(STORAGE_KEY).ok().flatten()) {
            Some(content) => Config::parse(&content),
            None => Config::default(),
        }
    }

    fn save(&self, cfg: &Config) {
        if let Some(storage) = Self::storage() {
            let _ = storage.set_item(STORAGE_KEY, &cfg.serialize());
        }
    }
}
