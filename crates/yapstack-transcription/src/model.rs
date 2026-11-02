use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::error::TranscriptionError;

type Result<T> = std::result::Result<T, TranscriptionError>;

// ----------- Legacy model files -----------

/// Files earlier releases downloaded into `models/` that no current model
/// uses: the Whisper GGML weights and the GGML Silero VAD file (both retired
/// by ADR 0005). They are never deleted silently; the user removes them from
/// Transcription settings.
const LEGACY_MODEL_FILES: &[&str] = &[
    "ggml-tiny.bin",
    "ggml-base.bin",
    "ggml-small.bin",
    "ggml-medium.bin",
    "ggml-silero-v6.2.0.bin",
];

/// A legacy model file present on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyModelFile {
    pub name: String,
    pub bytes: u64,
}

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

    // ---------- Legacy files ----------

    /// Legacy model files present in `models/`, in catalogue order.
    pub fn list_legacy_model_files(&self) -> Vec<LegacyModelFile> {
        LEGACY_MODEL_FILES
            .iter()
            .filter_map(|name| {
                let meta = std::fs::metadata(self.models_dir.join(name)).ok()?;
                meta.is_file().then(|| LegacyModelFile {
                    name: (*name).to_string(),
                    bytes: meta.len(),
                })
            })
            .collect()
    }

    /// Delete the named legacy model files. Names outside the legacy list are
    /// refused, so a caller can never delete a current model through this
    /// path. Files already gone are skipped.
    pub async fn delete_legacy_model_files(&self, names: &[String]) -> Result<()> {
        if let Some(bad) = names
            .iter()
            .find(|n| !LEGACY_MODEL_FILES.contains(&n.as_str()))
        {
            return Err(TranscriptionError::ModelNotFound(format!(
                "not a legacy model file: {bad}"
            )));
        }
        for name in names {
            match tokio::fs::remove_file(self.models_dir.join(name)).await {
                Ok(()) => info!("deleted legacy model file {name}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
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
    fn legacy_model_files_empty_when_none_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new(dir.path().to_path_buf());
        assert!(manager.list_legacy_model_files().is_empty());
    }

    #[test]
    fn legacy_model_files_lists_only_known_files_with_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("ggml-small.bin"), b"12345").unwrap();
        std::fs::write(models_dir.join("ggml-silero-v6.2.0.bin"), b"12").unwrap();
        std::fs::write(models_dir.join("notes.txt"), b"keep me").unwrap();
        std::fs::write(
            models_dir.join(SortformerVariant::V2_1.filename()),
            b"current",
        )
        .unwrap();

        let manager = ModelManager::new(dir.path().to_path_buf());
        assert_eq!(
            manager.list_legacy_model_files(),
            vec![
                LegacyModelFile {
                    name: "ggml-small.bin".into(),
                    bytes: 5
                },
                LegacyModelFile {
                    name: "ggml-silero-v6.2.0.bin".into(),
                    bytes: 2
                },
            ]
        );
    }

    #[tokio::test]
    async fn delete_legacy_model_files_removes_listed_files() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("ggml-small.bin"), b"x").unwrap();
        std::fs::write(models_dir.join("ggml-medium.bin"), b"x").unwrap();

        let manager = ModelManager::new(dir.path().to_path_buf());
        manager
            .delete_legacy_model_files(&["ggml-small.bin".into(), "ggml-tiny.bin".into()])
            .await
            .unwrap();
        assert!(!models_dir.join("ggml-small.bin").exists());
        assert!(models_dir.join("ggml-medium.bin").exists());
    }

    #[tokio::test]
    async fn delete_legacy_model_files_refuses_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let models_dir = dir.path().join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        let current = models_dir.join(SortformerVariant::V2_1.filename());
        std::fs::write(&current, b"current").unwrap();
        std::fs::write(models_dir.join("ggml-small.bin"), b"x").unwrap();

        let manager = ModelManager::new(dir.path().to_path_buf());
        let names = vec![
            "ggml-small.bin".to_string(),
            SortformerVariant::V2_1.filename().to_string(),
        ];
        assert!(manager.delete_legacy_model_files(&names).await.is_err());
        assert!(current.exists());
        assert!(
            models_dir.join("ggml-small.bin").exists(),
            "a refused batch deletes nothing"
        );
        let traversal = vec!["../outside.bin".to_string()];
        assert!(manager.delete_legacy_model_files(&traversal).await.is_err());
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
