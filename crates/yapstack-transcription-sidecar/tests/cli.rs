//! Process-level CLI contract: the host must never get a sidecar running an
//! engine it didn't ask for.

use std::process::{Command, Stdio};

#[test]
fn unknown_engine_exits_non_zero() {
    let status = Command::new(env!("CARGO_BIN_EXE_yapstack-transcription-sidecar"))
        .args(["--engine", "whisper"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn sidecar");
    assert!(!status.success(), "sidecar accepted --engine whisper");
}
