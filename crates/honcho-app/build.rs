fn main() {
    // the icon Explorer and the taskbar show for the .exe
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/sql-honcho.ico");
        res.compile().expect("compile Windows resources");
    }
}
