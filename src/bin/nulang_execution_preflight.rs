//! Deterministic deployment/recovery preflight for an exact durable execution.
//!
//! This executable does not modify workflow history or perform migrations.

use nulang::behavior_manifest::BehaviorManifest;
use nulang::execution_compatibility::DurableExecutionPin;
use std::fs;

const USAGE: &str = concat!(
    "usage:\n",
    "  nulang-execution-preflight pin <execution-id> <behavior.json> <artifact.nbc>\n",
    "  nulang-execution-preflight verify <execution-id> <pin.json> <behavior.json> <artifact.nbc>"
);

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|error| format!("cannot read {path}: {error}"))
}

fn read_manifest(path: &str) -> Result<BehaviorManifest, String> {
    BehaviorManifest::from_json(&read_file(path)?)
        .map_err(|error| format!("invalid behavior manifest {path}: {error}"))
}

fn run(args: &[String]) -> Result<String, String> {
    match args {
        [action, execution_id, manifest_path, artifact_path] if action == "pin" => {
            let manifest = read_manifest(manifest_path)?;
            let artifact = read_file(artifact_path)?;
            let pin = DurableExecutionPin::create(execution_id, &manifest, &artifact)
                .map_err(|error| error.to_string())?;
            String::from_utf8(pin.to_json().map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())
        }
        [action, execution_id, pin_path, manifest_path, artifact_path] if action == "verify" => {
            let pin = DurableExecutionPin::from_json(&read_file(pin_path)?)
                .map_err(|error| error.to_string())?;
            let manifest = read_manifest(manifest_path)?;
            let artifact = read_file(artifact_path)?;
            pin.validate_resume(execution_id, &manifest, &artifact)
                .map_err(|error| error.to_string())?;
            Ok(format!("verified pinned durable execution: {execution_id}"))
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_inputs_fail_closed() {
        assert!(run(&[]).unwrap_err().contains("usage"));
        assert!(run(&["verify".to_string()]).is_err());
    }

    #[test]
    fn pin_creation_does_not_accept_missing_artifact() {
        let args = vec![
            "pin".to_string(),
            "execution-1".to_string(),
            "absent-manifest.json".to_string(),
            "absent.nbc".to_string(),
        ];
        assert!(run(&args).is_err());
    }
}
