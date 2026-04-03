// build.rs
#[cfg(windows)]
fn main() {
    // 使用winres为Windows EXE嵌入ico图标
    let mut res = winres::WindowsResource::new();
    res.set_icon("src/icon.ico");
    res.compile().unwrap();
}

#[cfg(not(windows))]
fn main() {}
