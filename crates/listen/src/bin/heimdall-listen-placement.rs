fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../models/parakeet-ultra");
        for name in ["Preprocessor", "Encoder", "Decoder", "JointDecisionv3"] {
            let units = if name == "Preprocessor" {
                objc2_core_ml::MLComputeUnits::CPUOnly
            } else {
                objc2_core_ml::MLComputeUnits::CPUAndNeuralEngine
            };
            let plan =
                listen::placement::compute_plan(&path.join(format!("{name}.mlmodelc")), units)?;
            println!("{name}: {}", serde_json::to_string(&plan)?);
        }
    }
    Ok(())
}
