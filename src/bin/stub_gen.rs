use std::fs;

fn main() -> pyo3_stub_gen::Result<()> {
    fwsi::python_api::stub_info()?.generate()?;
    let path = "fwsi.pyi";
    let content = fs::read_to_string(path)?;
    let content = content
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{content}\n"))?;
    Ok(())
}
