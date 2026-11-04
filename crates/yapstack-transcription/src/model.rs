use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::error::TranscriptionError;

type Result<T> = std::result::Result<T, TranscriptionError>;

// ----------- Unused model files -----------

/// Paths under `models/` that earlier releases wrote and no current model
/// reads: the Whisper GGML weights and GGML Silero VAD file (ADR 0005), the
/// fp32 Parakeet bundle, and the int8 bundle's preprocessor, which
/// `parakeet-rs` never loads.
const UNUSED_MODEL_PATHS: &[&str] = &[
    "ggml-tiny.bin",
    "ggml-base.bin",
    "ggml-small.bin",
    "ggml-medium.bin",
    "ggml-silero-v6.2.0.bin",
    "parakeet-tdt-v3",
    "parakeet-tdt-v3-int8/nemo128.onnx",
];

// ----------- Parakeet TDT v3 -----------

/// One file of a model bundle, pinned by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    /// Exact size at the pinned revision; also the progress denominator
    /// when the server sends no Content-Length.
    pub bytes: u64,
    /// Lowercase hex SHA-256; a download that doesn't match is discarded.
    pub sha256: &'static str,
}

/// `istupakov/parakeet-tdt-0.6b-v3-onnx` at a fixed commit, so a later push
/// to the repo can't change what installs download. The hashes in
/// [`ParakeetVariant::files`] are the LFS SHA-256s at this revision.
macro_rules! istupakov_v3_url {
    ($file:literal) => {
        concat!(
            "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/",
            "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/",
            $file
        )
    };
}

const VOCAB_V3: ModelFile = ModelFile {
    name: "vocab.txt",
    url: istupakov_v3_url!("vocab.txt"),
    bytes: 93_939,
    sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParakeetVariant {
    /// int8-quantized `nvidia/parakeet-tdt-0.6b-v3` (multilingual, 25
    /// European languages), repackaged to ONNX by `istupakov`. Single-file
    /// inlined ONNX bundles with no `.onnx.data` external initializer, so
    /// accelerator EPs can load the encoder. Published GPU benchmarks show
    /// INT8/FP16/FP32 hit identical accuracy on LibriSpeech (97.84%) and
    /// indistinguishable WER on the leaderboard (15.72–15.77%).
    TdtV3Int8,
}

impl ParakeetVariant {
    /// The variant to install on this host. Every target runs the int8
    /// bundle.
    pub fn recommended_for_host() -> Self {
        ParakeetVariant::TdtV3Int8
    }

    /// Subdirectory under `$APP_DATA_DIR/models/` where this variant's files live.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ParakeetVariant::TdtV3Int8 => "parakeet-tdt-v3-int8",
        }
    }

    /// Files this variant requires on disk to load. `parakeet-rs` expects
    /// these inside the variant directory. It computes the mel features
    /// itself, so the bundle's `nemo128.onnx` preprocessor is not fetched.
    pub fn files(&self) -> &'static [ModelFile] {
        match self {
            ParakeetVariant::TdtV3Int8 => &[
                ModelFile {
                    name: "encoder-model.int8.onnx",
                    url: istupakov_v3_url!("encoder-model.int8.onnx"),
                    bytes: 652_183_999,
                    sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
                },
                ModelFile {
                    name: "decoder_joint-model.int8.onnx",
                    url: istupakov_v3_url!("decoder_joint-model.int8.onnx"),
                    bytes: 18_202_004,
                    sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
                },
                VOCAB_V3,
            ],
        }
    }

    pub fn approximate_size_bytes(&self) -> u64 {
        self.files().iter().map(|f| f.bytes).sum()
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ParakeetVariant::TdtV3Int8 => "Parakeet TDT v3 (~700 MB)",
        }
    }

    pub fn all() -> &'static [ParakeetVariant] {
        &[ParakeetVariant::TdtV3Int8]
    }
}

// ----------- Sortformer (speaker diarization) -----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortformerVariant {
    /// `diar_streaming_sortformer_4spk-v2.1.onnx` — up to 4 speakers,
    /// streaming-capable. Hosted under altunenes' parakeet-rs HF repo.
    V2_1,
}

impl SortformerVariant {
    pub fn filename(&self) -> &'static str {
        match self {
            SortformerVariant::V2_1 => "diar_streaming_sortformer_4spk-v2.1.onnx",
        }
    }

    pub fn download_url(&self) -> String {
        format!(
            "https://huggingface.co/altunenes/parakeet-rs/resolve/main/{}",
            self.filename()
        )
    }

    pub fn approximate_size_bytes(&self) -> u64 {
        match self {
            SortformerVariant::V2_1 => 50_000_000,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            SortformerVariant::V2_1 => "Sortformer 4-spk v2.1 (~50 MB)",
        }
    }
}

/// Download a file from `url` to `dest` via a temp file, with streaming progress.
/// `fallback_size` is used when the server doesn't provide Content-Length.
/// With `sha256`, a download whose content hashes differently is deleted
/// and never reaches `dest`.
async fn download_file(
    url: &str,
    dest: &Path,
    fallback_size: u64,
    sha256: Option<&str>,
    on_progress: &(impl Fn(f32) + Send),
) -> Result<()> {
    let temp_dest = dest.with_extension("download");

    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Err(TranscriptionError::DownloadFailed(format!(
            "HTTP {} for {}",
            response.status(),
            url
        )));
    }

    let total_size = response.content_length().unwrap_or(fallback_size);

    let mut file = tokio::fs::File::create(&temp_dest).await?;
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();

    let stream_result: Result<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| TranscriptionError::DownloadFailed(e.to_string()))?;
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
            downloaded += chunk.len() as u64;

            let progress = (downloaded as f32 / total_size as f32).min(1.0);
            on_progress(progress);
        }
        file.flush().await?;
        if let Some(expected) = sha256 {
            let actual = hex_lower(&hasher.finalize());
            if actual != expected {
                return Err(TranscriptionError::DownloadFailed(format!(
                    "SHA-256 mismatch for {url}: expected {expected}, got {actual}"
                )));
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = stream_result {
        drop(file);
        let _ = tokio::fs::remove_file(&temp_dest).await;
        return Err(e);
    }

    drop(file);

    if let Err(e) = tokio::fs::rename(&temp_dest, dest).await {
        let _ = tokio::fs::remove_file(&temp_dest).await;
        return Err(e.into());
    }

    on_progress(1.0);
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Destinations with a download in flight. Shared by every clone of a
/// `ModelManager`, because each Tauri command works on its own clone.
type InFlight = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>;

/// Held for the duration of one download; releases its destination on drop.
struct DownloadClaim {
    in_flight: InFlight,
    dest: PathBuf,
}

impl Drop for DownloadClaim {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .expect("in-flight mutex poisoned")
            .remove(&self.dest);
    }
}

#[derive(Clone)]
pub struct ModelManager {
    models_dir: PathBuf,
    in_flight: InFlight,
}

impl ModelManager {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let models_dir = app_data_dir.join("models");
        Self {
            models_dir,
            in_flight: InFlight::default(),
        }
    }

    /// Claim `dest` for one download. A second download of the same
    /// destination is refused while the first runs: both would write the
    /// same `.download` temp file.
    fn claim_download(&self, dest: &Path) -> Result<DownloadClaim> {
        let mut set = self.in_flight.lock().expect("in-flight mutex poisoned");
        if !set.insert(dest.to_path_buf()) {
            return Err(TranscriptionError::DownloadInProgress(
                dest.display().to_string(),
            ));
        }
        Ok(DownloadClaim {
            in_flight: self.in_flight.clone(),
            dest: dest.to_path_buf(),
        })
    }

    /// Returns the directory where models are stored.
    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    // ---------- Unused files ----------

    /// Delete every path in [`UNUSED_MODEL_PATHS`] that exists. Each removal
    /// is logged; one that fails is logged and skipped.
    pub fn remove_unused_model_files(&self) {
        for rel in UNUSED_MODEL_PATHS {
            let path = self.models_dir.join(rel);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let removed = if meta.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => info!("removed unused model file {}", path.display()),
                Err(e) => warn!("couldn't remove unused model file {}: {e}", path.display()),
            }
        }
    }

    // ---------- Parakeet ----------

    /// Directory where a Parakeet variant's ONNX files live (whether or not
    /// they're all downloaded yet). The directory is what `parakeet-rs`'
    /// `from_pretrained` is called with.
    pub fn parakeet_model_dir(&self, variant: ParakeetVariant) -> PathBuf {
        self.models_dir.join(variant.dir_name())
    }

    /// True iff every required file for `variant` is present on disk.
    pub fn parakeet_is_available(&self, variant: ParakeetVariant) -> bool {
        let dir = self.parakeet_model_dir(variant);
        variant.files().iter().all(|f| dir.join(f.name).exists())
    }

    /// Download every missing file for `variant`, reporting overall progress
    /// across all files in [0.0, 1.0].
    pub async fn download_parakeet(
        &self,
        variant: ParakeetVariant,
        on_progress: impl Fn(f32) + Send + Sync,
    ) -> Result<PathBuf> {
        let dir = self.parakeet_model_dir(variant);
        let _claim = self.claim_download(&dir)?;
        tokio::fs::create_dir_all(&dir).await?;

        let files = variant.files();
        let total_size: u64 = files.iter().map(|f| f.bytes).sum();
        let mut completed: u64 = 0;

        for file in files {
            let dest = dir.join(file.name);
            if dest.exists() {
                completed += file.bytes;
                on_progress((completed as f32 / total_size as f32).min(1.0));
                continue;
            }
            info!("downloading parakeet file {} from {}", file.name, file.url);
            let base = completed;
            download_file(
                file.url,
                &dest,
                file.bytes,
                Some(file.sha256),
                &|file_progress| {
                    let bytes_so_far = base + (file_progress * file.bytes as f32) as u64;
                    on_progress((bytes_so_far as f32 / total_size as f32).min(1.0));
                },
            )
            .await?;
            completed += file.bytes;
        }

        on_progress(1.0);
        info!(
            "parakeet variant {} downloaded successfully",
            variant.dir_name()
        );
        Ok(dir)
    }

    /// Ensure every file for `variant` is present, downloading any that are missing.
    pub async fn ensure_parakeet(&self, variant: ParakeetVariant) -> Result<PathBuf> {
        if self.parakeet_is_available(variant) {
            return Ok(self.parakeet_model_dir(variant));
        }
        self.download_parakeet(variant, |_| {}).await
    }

    /// Delete all files for a Parakeet variant.
    pub async fn delete_parakeet(&self, variant: ParakeetVariant) -> Result<()> {
        let dir = self.parakeet_model_dir(variant);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await?;
            info!("deleted parakeet variant {}", variant.dir_name());
        }
        Ok(())
    }

    // ---------- Sortformer ----------

    pub fn sortformer_model_path(&self, variant: SortformerVariant) -> Option<PathBuf> {
        let path = self.models_dir.join(variant.filename());
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    pub async fn download_sortformer(
        &self,
        variant: SortformerVariant,
        on_progress: impl Fn(f32) + Send,
    ) -> Result<PathBuf> {
        let dest = self.models_dir.join(variant.filename());
        let _claim = self.claim_download(&dest)?;
        tokio::fs::create_dir_all(&self.models_dir).await?;
        let url = variant.download_url();
        info!(
            "downloading sortformer model {} from {}",
            variant.filename(),
            url
        );
        download_file(
            &url,
            &dest,
            variant.approximate_size_bytes(),
            None,
            &on_progress,
        )
        .await?;
        info!(
            "sortformer model {} downloaded successfully",
            variant.filename()
        );
        Ok(dest)
    }

    pub async fn ensure_sortformer(&self, variant: SortformerVariant) -> Result<PathBuf> {
        if let Some(path) = self.sortformer_model_path(variant) {
            return Ok(path);
        }
        self.download_sortformer(variant, |_| {}).await
    }

    pub async fn delete_sortformer(&self, variant: SortformerVariant) -> Result<()> {
        if let Some(path) = self.sortformer_model_path(variant) {
            tokio::fs::remove_file(&path).await?;
            info!("deleted sortformer model {}", variant.filename());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parakeet_variant_int8_has_no_external_data_file() {
        // The whole reason int8 exists in this catalogue is so the encoder
        // bundle has no `.onnx.data` external initializer (which blocks
        // parakeet-rs accelerator EPs from loading the model). If a future
        // refactor accidentally re-adds an external-data file to the int8
        // variant's `files()` list this test fires.
        let int8_files = ParakeetVariant::TdtV3Int8.files();
        let has_external = int8_files
            .iter()
            .any(|f| f.name.ends_with(".onnx.data") || f.name.ends_with(".onnx_data"));
        assert!(
            !has_external,
            "int8 variant must not include any `.onnx.data` external initializer"
        );
    }

    #[test]
    fn parakeet_variant_int8_skips_unused_preprocessor() {
        // parakeet-rs computes the mel features in Rust; nothing loads the
        // bundle's `nemo128.onnx`, so new installs don't fetch it.
        assert!(ParakeetVariant::TdtV3Int8
            .files()
            .iter()
            .all(|f| f.name != "nemo128.onnx"));
    }

    #[test]
    fn parakeet_files_are_pinned_by_revision_and_hash() {
        for variant in ParakeetVariant::all() {
            for f in variant.files() {
                assert!(
                    f.url
                        .contains("/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/")
                        && f.url.ends_with(f.name),
                    "{f:?}"
                );
                assert!(
                    f.sha256.len() == 64
                        && f.sha256
                            .bytes()
                            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                    "{f:?}"
                );
            }
        }
    }

    /// Serves `body` once over plain HTTP and returns its URL.
    async fn serve_once(body: &'static [u8]) -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
        });
        format!("http://{addr}/file")
    }

    /// SHA-256 of `b"hello"`.
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[tokio::test]
    async fn download_with_matching_hash_lands() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.onnx");
        let url = serve_once(b"hello").await;
        download_file(&url, &dest, 5, Some(HELLO_SHA256), &|_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn download_with_wrong_hash_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.onnx");
        let url = serve_once(b"tampered").await;
        let err = download_file(&url, &dest, 8, Some(HELLO_SHA256), &|_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(&err, TranscriptionError::DownloadFailed(m) if m.contains("SHA-256 mismatch")),
            "{err:?}"
        );
        assert!(!dest.exists());
        assert!(!dest.with_extension("download").exists());
    }

    #[tokio::test]
    async fn concurrent_parakeet_download_of_same_variant_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        // A clone stands in for the second Tauri command call.
        let other = manager.clone();
        let _first = manager
            .claim_download(&manager.parakeet_model_dir(ParakeetVariant::TdtV3Int8))
            .unwrap();

        let second = other
            .download_parakeet(ParakeetVariant::TdtV3Int8, |_| {})
            .await;
        assert!(
            matches!(second, Err(TranscriptionError::DownloadInProgress(_))),
            "{second:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_sortformer_download_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let dest = manager
            .models_dir()
            .join(SortformerVariant::V2_1.filename());
        let _first = manager.claim_download(&dest).unwrap();

        let second = manager
            .clone()
            .download_sortformer(SortformerVariant::V2_1, |_| {})
            .await;
        assert!(matches!(
            second,
            Err(TranscriptionError::DownloadInProgress(_))
        ));
    }

    #[test]
    fn download_claim_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let dest = manager.parakeet_model_dir(ParakeetVariant::TdtV3Int8);
        drop(manager.claim_download(&dest).unwrap());
        let _a = manager.claim_download(&dest).unwrap();
        // Other destinations are independent.
        assert!(manager
            .claim_download(
                &manager
                    .models_dir()
                    .join(SortformerVariant::V2_1.filename())
            )
            .is_ok());
    }

    #[test]
    fn remove_unused_model_files_keeps_current_models_and_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let models = manager.models_dir().to_path_buf();
        let int8 = manager.parakeet_model_dir(ParakeetVariant::TdtV3Int8);
        let fp32 = models.join("parakeet-tdt-v3");
        std::fs::create_dir_all(&int8).unwrap();
        std::fs::create_dir_all(&fp32).unwrap();
        for f in ParakeetVariant::TdtV3Int8.files() {
            std::fs::write(int8.join(f.name), b"current").unwrap();
        }
        std::fs::write(int8.join("nemo128.onnx"), b"unused").unwrap();
        std::fs::write(fp32.join("encoder-model.onnx.data"), b"fp32").unwrap();
        for name in [
            "ggml-small.bin",
            "ggml-medium.bin",
            "ggml-silero-v6.2.0.bin",
        ] {
            std::fs::write(models.join(name), b"whisper").unwrap();
        }
        let sortformer = models.join(SortformerVariant::V2_1.filename());
        std::fs::write(&sortformer, b"current").unwrap();
        std::fs::write(models.join("notes.txt"), b"keep me").unwrap();

        manager.remove_unused_model_files();

        assert!(!fp32.exists());
        assert!(!int8.join("nemo128.onnx").exists());
        for name in [
            "ggml-small.bin",
            "ggml-medium.bin",
            "ggml-silero-v6.2.0.bin",
        ] {
            assert!(!models.join(name).exists(), "{name}");
        }
        assert!(manager.parakeet_is_available(ParakeetVariant::TdtV3Int8));
        assert!(sortformer.exists());
        assert!(models.join("notes.txt").exists());
    }

    #[test]
    fn remove_unused_model_files_without_models_dir_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        ModelManager::new(dir.path().to_path_buf()).remove_unused_model_files();
        assert!(!dir.path().join("models").exists());
    }

    #[test]
    fn test_parakeet_variant_files_are_consistent() {
        let v = ParakeetVariant::TdtV3Int8;
        let files = v.files();
        assert!(!files.is_empty());
        // Required by parakeet-rs's ParakeetTDT::from_pretrained.
        let names: Vec<&str> = files.iter().map(|f| f.name).collect();
        assert!(names.contains(&"vocab.txt"));
        assert!(names.iter().any(|n| n.contains("encoder")));
        assert!(names.iter().any(|n| n.contains("decoder")));
    }

    #[test]
    fn test_parakeet_not_available_until_all_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let variant = ParakeetVariant::TdtV3Int8;
        assert!(!manager.parakeet_is_available(variant));

        // Drop one of the required files into the variant dir; partial
        // download must still report unavailable.
        let v_dir = manager.parakeet_model_dir(variant);
        std::fs::create_dir_all(&v_dir).unwrap();
        std::fs::write(v_dir.join("vocab.txt"), b"placeholder").unwrap();
        assert!(!manager.parakeet_is_available(variant));
    }

    #[test]
    fn test_parakeet_available_when_all_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let variant = ParakeetVariant::TdtV3Int8;
        let v_dir = manager.parakeet_model_dir(variant);
        std::fs::create_dir_all(&v_dir).unwrap();
        for f in variant.files() {
            std::fs::write(v_dir.join(f.name), b"placeholder").unwrap();
        }
        assert!(manager.parakeet_is_available(variant));
    }

    #[test]
    fn test_sortformer_not_available_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager
            .sortformer_model_path(SortformerVariant::V2_1)
            .is_none());
    }

    #[test]
    fn test_sortformer_detects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(
            models_dir.join(SortformerVariant::V2_1.filename()),
            b"fake sortformer",
        )
        .unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager
            .sortformer_model_path(SortformerVariant::V2_1)
            .is_some());
    }
}
