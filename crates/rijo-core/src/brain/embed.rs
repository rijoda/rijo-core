//! Embed — fastembed v5 (ONNX/ort): real semantic embeddings
//! Fallback: SimEmbed only when fastembed is not initialized.
//!
//! Model cache: fastembed's default is `./.fastembed_cache`, i.e. RELATIVE TO THE
//! WORKING DIRECTORY. Rijo Code is started from wherever the project is, so that
//! default downloaded its own ~128 MB copy of the ONNX model and the tokenizer
//! into every directory the user ever worked in (found in ~/Projects/rijo,
//! ~/Projects/test, test2, test3, openram, …). The cache now lives in one place
//! under `~/.rijo/models`, so the download happens once.

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone)]
pub struct EmbedConfig {
    pub model_id: String,
    pub dim: usize,
    pub show_progress: bool,
    pub max_batch_size: usize,
    /// Where the ONNX model and tokenizer are cached (see `model_cache_dir`).
    pub cache_dir: PathBuf,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model_id: "all-MiniLM-L6-v2".into(),
            dim: 384,
            show_progress: false,
            max_batch_size: 32,
            cache_dir: model_cache_dir(),
        }
    }
}

/// Stable location of the model cache.
///
/// Precedence: `RIJO_MODEL_CACHE` (tests, CI, custom installs), otherwise
/// `~/.rijo/models/fastembed`. The directory is created on demand.
///
/// Note: fastembed also honours `HF_HOME`, and that takes precedence over the
/// cache dir — documented because it silently overrides this setting.
pub fn model_cache_dir() -> PathBuf {
    let dir = std::env::var("RIJO_MODEL_CACHE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::rijo_home().join("models").join("fastembed"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// One place for the initialization options: the previous code built
/// `TextInitOptions::default()` twice (which is also where the CWD-relative cache
/// came from) and ignored `model_id` entirely.
fn init_options(cfg: &EmbedConfig) -> TextInitOptions {
    let model = cfg.model_id.parse::<EmbeddingModel>().unwrap_or_default();
    TextInitOptions::new(model)
        .with_cache_dir(cfg.cache_dir.clone())
        .with_show_download_progress(cfg.show_progress)
}

/// Makes sure the model is present in the cache and returns its directory.
///
/// Used by `rijo --download-models` (install time) and by tests: the download
/// then happens once, during installation, instead of on the first turn in every
/// new working directory.
pub fn ensure_model(cfg: &EmbedConfig) -> Result<PathBuf, String> {
    let cache = cfg.cache_dir.clone();
    let mut model = TextEmbedding::try_new(init_options(cfg)).map_err(|e| e.to_string())?;
    // One throwaway embedding: proves the ONNX session and the tokenizer really
    // work, not just that the files exist.
    model
        .embed(vec!["warmup"], None)
        .map_err(|e| e.to_string())?;
    Ok(cache)
}

/// Size of the cached model files in bytes (for `--download-models` and
/// `--check`).
pub fn cache_size(dir: &std::path::Path) -> u64 {
    fn walk(path: &std::path::Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // symlink_metadata: the HuggingFace layout links every file in
            // `snapshots/…` to its blob in `blobs/…`. Following the links counted
            // every byte twice (128 MB reported as 255 MB).
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                walk(&path, total);
            } else if meta.is_file() {
                *total += meta.len();
            }
        }
    }
    let mut total = 0;
    walk(dir, &mut total);
    total
}

pub trait TextEmbed: Send {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
    fn dim(&self) -> usize;
}

pub struct Embedder {
    inner: Mutex<Option<Box<dyn TextEmbed + Send>>>,
    config: EmbedConfig,
}

impl Embedder {
    pub fn new(config: EmbedConfig) -> Self {
        Self {
            inner: Mutex::new(None),
            config,
        }
    }

    pub fn with_fallback() -> Self {
        Self {
            inner: Mutex::new(Some(Box::new(SimEmbed { dim: 384 }))),
            config: EmbedConfig::default(),
        }
    }

    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut lock = self.inner.lock().map_err(|e| e.to_string())?;
        if lock.is_none() {
            // fastembed nutzt ort intern; die DLL muss geladen sein, bevor
            // TextEmbedding::try_new aufgerufen wird. ORT_DYLIB_PATH ist
            // global, also reicht dieser eine Aufruf für beide Abnehmer.
            crate::router::session::ensure_ort_dll_loaded();
            let emb: TextEmbedding = TextEmbedding::try_new(init_options(&self.config))
                .map_err(|e| format!("fastembed init failed: {e}"))?;
            *lock = Some(Box::new(FastEmbedBackend {
                inner: emb,
                dim: self.config.dim,
            }));
        }
        Ok(lock.as_mut().unwrap().embed(&[text])?.remove(0))
    }

    /// Batch embedding: one ONNX call for many texts (batches of 32).
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let mut lock = self.inner.lock().map_err(|e| e.to_string())?;
        if lock.is_none() {
            crate::router::session::ensure_ort_dll_loaded();
            let emb: TextEmbedding = TextEmbedding::try_new(init_options(&self.config))
                .map_err(|e| format!("fastembed init failed: {e}"))?;
            *lock = Some(Box::new(FastEmbedBackend {
                inner: emb,
                dim: self.config.dim,
            }));
        }
        lock.as_mut().unwrap().embed(texts)
    }

    pub fn dim(&self) -> usize {
        self.config.dim
    }
}

struct FastEmbedBackend {
    inner: TextEmbedding,
    dim: usize,
}
impl TextEmbed for FastEmbedBackend {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        self.inner.embed(texts, None).map_err(|e| e.to_string())
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

// Fallback stays unchanged
struct SimEmbed {
    dim: usize,
}
impl TextEmbed for SimEmbed {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dim];
                let bytes = t.as_bytes();
                for (i, &b) in bytes.iter().enumerate() {
                    v[i % self.dim] += b as f32 / 255.0;
                }
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in &mut v {
                        *x /= norm;
                    }
                }
                v
            })
            .collect())
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    lazy_static::lazy_static! {
        /// RIJO_MODEL_CACHE / HOME are process-wide.
        static ref ENV_LOCK: Mutex<()> = Mutex::new(());
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// The whole point of the change: one cache directory, independent of where
    /// Rijo Code was started (the fastembed default was `./.fastembed_cache`).
    #[test]
    fn the_model_cache_does_not_depend_on_the_working_directory() {
        let _guard = lock();
        std::env::remove_var("RIJO_MODEL_CACHE");
        let dir = model_cache_dir();
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(
            dir.ends_with("models/fastembed"),
            "not under the rijo home: {dir:?}"
        );
        assert!(
            !dir.starts_with(std::env::current_dir().unwrap()),
            "cache inside the workspace again: {dir:?}"
        );
        assert!(
            dir.exists(),
            "cache directory is created on demand: {dir:?}"
        );
        // Called twice: same answer.
        assert_eq!(dir, model_cache_dir());
    }

    #[test]
    fn the_cache_directory_can_be_overridden() {
        let _guard = lock();
        let tmp = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("my-models");
        std::env::set_var("RIJO_MODEL_CACHE", &custom);
        assert_eq!(model_cache_dir(), custom);
        assert!(custom.exists(), "override directory is created too");
        // An empty value counts as unset, not as "cache in the current directory".
        std::env::set_var("RIJO_MODEL_CACHE", "   ");
        let fallback = model_cache_dir();
        assert!(fallback.ends_with("models/fastembed"), "{fallback:?}");
        std::env::remove_var("RIJO_MODEL_CACHE");
    }

    /// The init options must carry the model AND the cache dir — the previous
    /// code ignored `model_id` and left the cache dir at fastembed's default.
    #[test]
    fn init_options_carry_model_and_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = EmbedConfig {
            model_id: "bge-small-en-v1.5".into(),
            cache_dir: tmp.path().to_path_buf(),
            ..EmbedConfig::default()
        };
        let options = init_options(&cfg);
        assert_eq!(options.cache_dir, tmp.path());
        assert_eq!(options.model_name, EmbeddingModel::BGESmallENV15);

        // An unknown name falls back to the default model instead of failing the
        // whole embedder.
        let unknown = EmbedConfig {
            model_id: "not-a-model".into(),
            ..cfg
        };
        assert_eq!(init_options(&unknown).model_name, EmbeddingModel::default());
    }

    /// The real end of the change: a start from a DIFFERENT working directory
    /// loads the ONNX model from the shared cache and creates no local copy.
    ///
    /// Runs the actual embedder (no LLM needed), so it also proves the cached
    /// model is usable, not just present. It changes the process working
    /// directory, hence the env lock shared with the other tests here.
    #[test]
    fn embedding_from_another_directory_reuses_the_shared_cache() {
        let _guard = lock();
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("some-other-project");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&elsewhere).unwrap();

        // Seed the shared cache through the real API (downloads once, then hits).
        let cfg = EmbedConfig::default();
        let dir = ensure_model(&cfg).expect("model must be available");
        let before = cache_size(&dir);

        let embedder = Embedder::new(cfg.clone());
        let vector = embedder
            .embed_text("der Modell-Cache ist jetzt global")
            .unwrap();
        assert_eq!(vector.len(), 384, "{:?}", vector.len());
        assert!(vector.iter().any(|v| *v != 0.0), "empty embedding");

        // A second embedder (the next start) must not have downloaded anything.
        let after = cache_size(&dir);
        assert_eq!(
            before, after,
            "the second load downloaded again: {before} -> {after}"
        );
        assert!(
            !elsewhere.join(".fastembed_cache").exists(),
            "a local cache was created in the working directory: {}",
            elsewhere.display()
        );

        std::env::set_current_dir(original).unwrap();
    }

    #[test]
    fn cache_size_walks_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        std::fs::write(tmp.path().join("a/one.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(tmp.path().join("a/b/two.bin"), vec![0u8; 32]).unwrap();
        assert_eq!(cache_size(tmp.path()), 42);
        assert_eq!(cache_size(&tmp.path().join("missing")), 0);

        // A symlinked file (the HuggingFace blob layout) counts once, not twice.
        #[cfg(unix)]
        {
            let linked = tmp.path().join("a/b/two-link.bin");
            let _ = std::fs::remove_file(&linked);
            std::os::unix::fs::symlink(tmp.path().join("a/b/two.bin"), &linked).unwrap();
            assert_eq!(cache_size(tmp.path()), 42, "symlink double counted");
        }
    }
}
