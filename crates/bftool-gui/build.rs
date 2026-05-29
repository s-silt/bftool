//! 构建脚本:给 Windows 上的 `bftool-gui.exe` 嵌入图标 + 版本/产品信息(PE 资源段)。
//! 只在 windows 目标生效;失败只 warn 不中断(图标是锦上添花,exe 仍须能 build)。

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "归档备份工具 bftool");
        res.set("FileDescription", "归档备份工具(bftool)桌面版");
        res.set("LegalCopyright", "MIT License");
        // 版本号:winresource 自动从 CARGO_PKG_VERSION 派生 FileVersion/ProductVersion。
        if let Err(e) = res.compile() {
            // 不吞错:把失败显式打到构建日志(资源嵌入失败不致命,exe 仍可用)。
            println!("cargo:warning=嵌入 Windows 资源失败(图标/版本信息),exe 仍可用: {e}");
        }
    }
}
