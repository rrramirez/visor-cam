slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let panel = RecorderPanel::new()?;
    panel.run()?;
    Ok(())
}
