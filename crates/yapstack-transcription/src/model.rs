use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::error::TranscriptionError;

type Result<T> = std::result::Result<T, TranscriptionError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelSize {
    Tiny,
    Base,
    Small,
    Medium,
}

impl ModelSize {
    /// Returns the ggml model filename.
    pub fn filename(&self) -> &'static str {
        match self {
            ModelSize::Tiny => "ggml-tiny.bin",
            ModelSize::Base => "ggml-base.bin",
            ModelSize::Small => "ggml-small.bin",
            ModelSize::Medium => "ggml-medium.bin",
        }
    }

    /// Returns the approximate size in bytes.
    pub fn approximate_size_bytes(&self) -> u64 {
        match self {
            ModelSize::Tiny => 75_000_000,
            ModelSize::Base => 142_000_000,
            ModelSize::Small => 466_000_000,
            ModelSize::Medium => 1_500_000_000,
        }
    }

    /// Returns the Hugging Face download URL.
    pub fn download_url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
            self.filename()
        )
    }

    /// Returns a human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            ModelSize::Tiny => "Tiny (~75 MB)",
            ModelSize::Base => "Base (~142 MB)",
            ModelSize::Small => "Small (~466 MB)",
            ModelSize::Medium => "Medium (~1.5 GB)",
        }
    }

    /// All available model sizes.
    pub fn all() -> &'static [ModelSize] {
        &[
            ModelSize::Tiny,
            ModelSize::Base,
            ModelSize::Small,
            ModelSize::Medium,
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub size: ModelSize,
    pub downloaded: bool,
    pub path: Option<PathBuf>,
    pub display_name: String,
    pub approximate_size_bytes: u64,
}

const VAD_MODEL_FILENAME: &str = "ggml-silero-v6.2.0.bin";
const VAD_MODEL_URL: &str =
    "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin";
const VAD_MODEL_SIZE_BYTES: u64 = 885_000;

// ----------- Parakeet TDT v3 -----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParakeetVariant {
    /// nvidia/parakeet-tdt-0.6b-v3 (multilingual, 25 European languages),
    /// repackaged to ONNX by `istupakov` on HuggingFace. fp32 weights — the
    /// encoder ships a 2.4 GB external `.onnx.data` blob which prevents
    /// accelerator EPs (CoreML, WebGPU) from loading the model. Retained
    /// only for the CPU-only Intel-macOS build, which has no GPU EP path;
    /// every WebGPU-capable host (Apple Silicon, Windows, Linux) uses the
    /// int8 single-file bundle instead.
    TdtV3,
    /// int8-quantized variant of the same `nvidia/parakeet-tdt-0.6b-v3`
    /// model, also from `istupakov`. Single-file inlined ONNX bundles —
    /// no `.onnx.data` external initializer, so accelerators can load
    /// the encoder. Published GPU benchmarks show INT8/FP16/FP32 hit
    /// identical accuracy on LibriSpeech (97.84%) and indistinguishable
    /// WER on the leaderboard (15.72–15.77%); RNN-T joint decisions are
    /// driven by relative logit magnitudes so quantization doesn't change
    /// argmax decoding. Default on every WebGPU-capable host — Apple Silicon
    /// (Metal), Windows and Linux (Dawn → Vulkan/D3D12).
    TdtV3Int8,
}

impl ParakeetVariant {
    /// The variant we should pick at install time for the current host.
    /// Apple Silicon and non-macOS hosts (Windows/Linux) both run the int8
    /// bundle so the WebGPU EP can load it — int8 is a single-file inlined
    /// ONNX bundle with no external `.onnx.data` initializer, which is what
    /// blocks accelerator EPs from loading the fp32 encoder. The fp32
    /// `TdtV3` variant is retained only for the CPU-only Intel-macOS build,
    /// which has neither a WebGPU nor a CoreML acceleration path today.
    pub fn recommended_for_host() -> Self {
        // macOS + aarch64 (Apple Silicon, WebGPU/Metal) → int8.
        // Non-macOS (Windows/Linux, WebGPU/Dawn) → int8.
        // Only CPU-only Intel macOS falls back to the fp32 bundle.
        let macos_intel = cfg!(target_os = "macos") && !cfg!(target_arch = "aarch64");
        if macos_intel {
            ParakeetVariant::TdtV3
        } else {
            ParakeetVariant::TdtV3Int8
        }
    }

    /// Subdirectory under `$APP_DATA_DIR/models/` where this variant's files live.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ParakeetVariant::TdtV3 => "parakeet-tdt-v3",
            ParakeetVariant::TdtV3Int8 => "parakeet-tdt-v3-int8",
        }
    }

    /// Files this variant requires on disk to load. Each tuple is
    /// `(filename, download_url, approximate_size_bytes)`.
    /// `parakeet-rs` expects these inside the variant directory.
    pub fn files(&self) -> &'static [(&'static str, &'static str, u64)] {
        match self {
            ParakeetVariant::TdtV3 => &[
                (
                    "encoder-model.onnx",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/encoder-model.onnx",
                    41_770_866,
                ),
                (
                    "encoder-model.onnx.data",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/encoder-model.onnx.data",
                    2_435_420_160,
                ),
                (
                    "decoder_joint-model.onnx",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/decoder_joint-model.onnx",
                    72_520_893,
                ),
                (
                    "vocab.txt",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/vocab.txt",
                    93_939,
                ),
            ],
            // Sizes verified against the HF tree (2026-07-02): encoder ~652 MB,
            // decoder ~18 MB, preprocessor ~140 KB, vocab ~94 KB. The
            // sub-MB values double as Content-Length fallbacks; HF returns
            // accurate Content-Length so they're rarely consulted.
            ParakeetVariant::TdtV3Int8 => &[
                (
                    "encoder-model.int8.onnx",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/encoder-model.int8.onnx",
                    652_183_999,
                ),
                (
                    "decoder_joint-model.int8.onnx",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/decoder_joint-model.int8.onnx",
                    18_202_004,
                ),
                (
                    "nemo128.onnx",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/nemo128.onnx",
                    150_000,
                ),
                (
                    "vocab.txt",
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/vocab.txt",
                    96_000,
                ),
            ],
        }
    }

    pub fn approximate_size_bytes(&self) -> u64 {
        self.files().iter().map(|(_, _, s)| s).sum()
    }

    /// User-facing label. Both variants intentionally report the same
    /// model identity (`Parakeet TDT v3`) — the int8 vs fp32 split is an
    /// implementation detail driven by what the host can accelerate, not
    /// something the user picks. Size ranges are the only honest hint
    /// that they differ on disk.
    pub fn display_name(&self) -> &'static str {
        match self {
            ParakeetVariant::TdtV3 => "Parakeet TDT v3 (~2.5 GB)",
            ParakeetVariant::TdtV3Int8 => "Parakeet TDT v3 (~700 MB)",
        }
    }

    pub fn all() -> &'static [ParakeetVariant] {
        &[ParakeetVariant::TdtV3, ParakeetVariant::TdtV3Int8]
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
async fn download_file(
    url: &str,
    dest: &Path,
    fallback_size: u64,
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

    let stream_result: Result<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| TranscriptionError::DownloadFailed(e.to_string()))?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;

            let progress = (downloaded as f32 / total_size as f32).min(1.0);
            on_progress(progress);
        }
        file.flush().await?;
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

#[derive(Clone)]
pub struct ModelManager {
    models_dir: PathBuf,
}

impl ModelManager {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let models_dir = app_data_dir.join("models");
        Self { models_dir }
    }

    /// Returns the directory where models are stored.
    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Check if a model is downloaded.
    pub fn is_available(&self, size: ModelSize) -> bool {
        self.model_path(size).is_some()
    }

    /// Get path to model file if it exists.
    pub fn model_path(&self, size: ModelSize) -> Option<PathBuf> {
        let path = self.models_dir.join(size.filename());
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Get path where a model would be stored (whether or not it exists).
    pub fn expected_model_path(&self, size: ModelSize) -> PathBuf {
        self.models_dir.join(size.filename())
    }

    /// Download model with progress callback.
    ///
    /// The callback receives a progress value from 0.0 to 1.0.
    pub async fn download(
        &self,
        size: ModelSize,
        on_progress: impl Fn(f32) + Send,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.models_dir).await?;

        let url = size.download_url();
        let dest = self.models_dir.join(size.filename());

        info!("downloading model {} from {}", size.filename(), url);
        download_file(&url, &dest, size.approximate_size_bytes(), &on_progress).await?;
        info!("model {} downloaded successfully", size.filename());

        Ok(dest)
    }

    /// Verify model file checksum using streaming reads to avoid loading
    /// the entire file (potentially 1.5 GB+) into memory.
    pub async fn verify_checksum(&self, size: ModelSize, expected_sha256: &str) -> Result<bool> {
        use tokio::io::AsyncReadExt;

        let path = self
            .model_path(size)
            .ok_or_else(|| TranscriptionError::ModelNotFound(size.filename().to_string()))?;

        let file = tokio::fs::File::open(&path).await?;
        let mut reader = tokio::io::BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024]; // 64 KB chunks

        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }

        let result = format!("{:x}", hasher.finalize());

        if result != expected_sha256 {
            warn!(
                "checksum mismatch for {}: expected {}, got {} — deleting corrupted file",
                size.filename(),
                expected_sha256,
                result
            );
            let _ = tokio::fs::remove_file(&path).await;
            return Ok(false);
        }

        Ok(true)
    }

    /// Delete a downloaded model.
    pub async fn delete(&self, size: ModelSize) -> Result<()> {
        if let Some(path) = self.model_path(size) {
            tokio::fs::remove_file(&path).await?;
            info!("deleted model {}", size.filename());
        }
        Ok(())
    }

    /// List all downloaded models.
    pub fn list_downloaded(&self) -> Vec<ModelSize> {
        ModelSize::all()
            .iter()
            .copied()
            .filter(|s| self.is_available(*s))
            .collect()
    }

    /// Get info for all models (downloaded or not).
    pub fn list_all(&self) -> Vec<ModelInfo> {
        ModelSize::all()
            .iter()
            .map(|&size| ModelInfo {
                size,
                downloaded: self.is_available(size),
                path: self.model_path(size),
                display_name: size.display_name().to_string(),
                approximate_size_bytes: size.approximate_size_bytes(),
            })
            .collect()
    }

    /// Get path to VAD model file if it exists.
    pub fn vad_model_path(&self) -> Option<PathBuf> {
        let path = self.models_dir.join(VAD_MODEL_FILENAME);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Download the Silero VAD model with progress callback.
    pub async fn download_vad_model(&self, on_progress: impl Fn(f32) + Send) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.models_dir).await?;

        let dest = self.models_dir.join(VAD_MODEL_FILENAME);

        info!("downloading VAD model from {}", VAD_MODEL_URL);
        download_file(VAD_MODEL_URL, &dest, VAD_MODEL_SIZE_BYTES, &on_progress).await?;
        info!("VAD model downloaded successfully");

        Ok(dest)
    }

    /// Ensure the VAD model is available, downloading if missing.
    pub async fn ensure_vad_model(&self) -> Result<PathBuf> {
        if let Some(path) = self.vad_model_path() {
            return Ok(path);
        }
        self.download_vad_model(|_| {}).await
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
        variant
            .files()
            .iter()
            .all(|(name, _, _)| dir.join(name).exists())
    }

    /// Download every missing file for `variant`, reporting overall progress
    /// across all files in [0.0, 1.0].
    pub async fn download_parakeet(
        &self,
        variant: ParakeetVariant,
        on_progress: impl Fn(f32) + Send + Sync,
    ) -> Result<PathBuf> {
        let dir = self.parakeet_model_dir(variant);
        tokio::fs::create_dir_all(&dir).await?;

        let files = variant.files();
        let total_size: u64 = files.iter().map(|(_, _, s)| *s).sum();
        let mut completed: u64 = 0;

        for (name, url, size) in files {
            let dest = dir.join(name);
            if dest.exists() {
                completed += *size;
                on_progress((completed as f32 / total_size as f32).min(1.0));
                continue;
            }
            info!("downloading parakeet file {} from {}", name, url);
            let base = completed;
            download_file(url, &dest, *size, &|file_progress| {
                let bytes_so_far = base + (file_progress * *size as f32) as u64;
                on_progress((bytes_so_far as f32 / total_size as f32).min(1.0));
            })
            .await?;
            completed += *size;
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
        tokio::fs::create_dir_all(&self.models_dir).await?;
        let dest = self.models_dir.join(variant.filename());
        let url = variant.download_url();
        info!(
            "downloading sortformer model {} from {}",
            variant.filename(),
            url
        );
        download_file(&url, &dest, variant.approximate_size_bytes(), &on_progress).await?;
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
    fn test_model_size_filenames() {
        assert_eq!(ModelSize::Tiny.filename(), "ggml-tiny.bin");
        assert_eq!(ModelSize::Base.filename(), "ggml-base.bin");
        assert_eq!(ModelSize::Small.filename(), "ggml-small.bin");
        assert_eq!(ModelSize::Medium.filename(), "ggml-medium.bin");
    }

    #[test]
    fn test_model_size_urls() {
        let url = ModelSize::Tiny.download_url();
        assert!(url.contains("huggingface.co"));
        assert!(url.contains("ggml-tiny.bin"));
    }

    #[test]
    fn test_model_size_all() {
        assert_eq!(ModelSize::all().len(), 4);
    }

    #[test]
    fn parakeet_variant_dir_names_distinct() {
        let mut names: Vec<&str> = ParakeetVariant::all()
            .iter()
            .map(|v| v.dir_name())
            .collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            ParakeetVariant::all().len(),
            "every Parakeet variant must have a unique dir_name so download/delete don't collide"
        );
    }

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
            .any(|(name, _, _)| name.ends_with(".onnx.data") || name.ends_with(".onnx_data"));
        assert!(
            !has_external,
            "int8 variant must not include any `.onnx.data` external initializer"
        );
    }

    #[test]
    fn parakeet_variant_int8_includes_preprocessor() {
        // The int8 bundle on HuggingFace ships a separate `nemo128.onnx`
        // preprocessor file that the fp32 bundle does not have (fp32's
        // preprocessor is baked into the encoder graph). Loading int8
        // without this file would surface as a missing-input error at
        // session-build time. Lock it in so a future trim doesn't drop it.
        let int8_files = ParakeetVariant::TdtV3Int8.files();
        let has_preprocessor = int8_files
            .iter()
            .any(|(name, _, _)| *name == "nemo128.onnx");
        assert!(
            has_preprocessor,
            "int8 variant must include nemo128.onnx preprocessor"
        );
    }

    #[test]
    fn parakeet_recommended_for_host_matches_target() {
        // `recommended_for_host` is `cfg!`-driven, so the test runner sees
        // exactly one branch on each CI runner. We assert against the same
        // cfg flags here so the test is meaningful on every target without
        // having to mock the host detection. Only CPU-only Intel macOS keeps
        // the fp32 bundle; every WebGPU-capable host (Apple Silicon +
        // Windows/Linux) takes the int8 single-file bundle.
        let macos_intel = cfg!(target_os = "macos") && !cfg!(target_arch = "aarch64");
        let expected = if macos_intel {
            ParakeetVariant::TdtV3
        } else {
            ParakeetVariant::TdtV3Int8
        };
        assert_eq!(ParakeetVariant::recommended_for_host(), expected);
    }

    #[test]
    fn test_model_manager_not_available() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(!manager.is_available(ModelSize::Tiny));
        assert!(manager.model_path(ModelSize::Tiny).is_none());
    }

    #[test]
    fn test_model_manager_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.list_downloaded().is_empty());
    }

    #[test]
    fn test_model_manager_list_all() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let all = manager.list_all();
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|m| !m.downloaded));
    }

    #[test]
    fn test_model_manager_detects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("ggml-tiny.bin"), b"fake model data").unwrap();

        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.is_available(ModelSize::Tiny));
        assert!(!manager.is_available(ModelSize::Base));
        assert_eq!(manager.list_downloaded(), vec![ModelSize::Tiny]);
    }

    #[test]
    fn test_vad_model_not_available() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.vad_model_path().is_none());
    }

    #[test]
    fn test_vad_model_detects_existing() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("ggml-silero-v6.2.0.bin"), b"fake vad model").unwrap();

        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.vad_model_path().is_some());
    }

    #[tokio::test]
    async fn test_verify_checksum_deletes_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();

        // Write fake model data
        let model_path = models_dir.join(ModelSize::Tiny.filename());
        std::fs::write(&model_path, b"fake model data that will fail checksum").unwrap();
        assert!(model_path.exists());

        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.is_available(ModelSize::Tiny));

        // Verify against a wrong hash — should return Ok(false) and delete the file
        let result = manager
            .verify_checksum(
                ModelSize::Tiny,
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .await
            .unwrap();
        assert!(!result);
        assert!(!model_path.exists(), "corrupted file should be deleted");
        assert!(!manager.is_available(ModelSize::Tiny));
    }

    #[test]
    fn test_model_size_serde_roundtrip() {
        for size in ModelSize::all() {
            let json = serde_json::to_string(size).unwrap();
            let deserialized: ModelSize = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, *size);
        }
    }

    #[test]
    fn test_parakeet_variant_files_are_consistent() {
        let v = ParakeetVariant::TdtV3;
        let files = v.files();
        assert!(!files.is_empty());
        // Required by parakeet-rs's ParakeetTDT::from_pretrained.
        let names: Vec<&str> = files.iter().map(|(n, _, _)| *n).collect();
        assert!(names.contains(&"vocab.txt"));
        assert!(names.iter().any(|n| n.contains("encoder")));
        assert!(names.iter().any(|n| n.contains("decoder")));
    }

    #[test]
    fn test_parakeet_not_available_until_all_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        let variant = ParakeetVariant::TdtV3;
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
        let variant = ParakeetVariant::TdtV3;
        let v_dir = manager.parakeet_model_dir(variant);
        std::fs::create_dir_all(&v_dir).unwrap();
        for (name, _, _) in variant.files() {
            std::fs::write(v_dir.join(name), b"placeholder").unwrap();
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
