slint::slint! {
    export component RecorderPanel inherits Window {
        title: "Visor";
        width: 360px;
        height: 200px;
        Text {
            text: "Visor";
            font-size: 24px;
        }
    }
}

fn main() -> anyhow::Result<()> {
    RecorderPanel::new()?.run()?;
    Ok(())
}
