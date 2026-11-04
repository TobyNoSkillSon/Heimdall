use anyhow::Result;
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().collect();
    anyhow::ensure!(a.len() == 5, "model.onnx inputs.json output-dir cpu|metal");
    println!(
        "{}",
        speak::shader_module_probe(a[1].as_ref(), a[2].as_ref(), a[3].as_ref(), a[4] == "metal")?
    );
    Ok(())
}
